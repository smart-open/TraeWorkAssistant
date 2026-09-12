//! WorkBuddy M7 生态接入 · OAuth 扫码登录（F-50，§3.10）（原 workbuddy.rs 机械拆分）。
//!
//! 流程（无 PKCE，state 服务端签发）：
//!   ① POST /v2/plugin/auth/state?platform=CLI → state + authUrl
//!   ② 系统浏览器打开 authUrl（用户扫码/登录，与工具侧 cookie 天然隔离）
//!   ③ GET /v2/plugin/auth/token?state= 轮询（≤300s，间隔 3s）
//!   ④ GET /v2/plugin/login/account?state= 取 uid/nickname → 自动入池 + 凭证回写 token store
//! 每流程独立 cookie jar（手工捕获 Set-Cookie 回传，不引新依赖）；凭证零明文输出（不进日志/事件/UI）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, Emitter};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{account_id_of, as_str, load_pool, save_pool, upsert_token_store, WorkBuddyAccount};

static OAUTH_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 捕获响应 Set-Cookie 到独立 jar（简化：保留 name=value，忽略 Path/Expires 等属性）
fn oauth_capture_cookies(jar: &mut std::collections::HashMap<String, String>, resp: &ureq::Response) {
    for hv in resp.all("Set-Cookie") {
        if let Some(kv) = hv.split(';').next() {
            if let Some((k, v)) = kv.split_once('=') {
                jar.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
}

fn oauth_cookie_header(jar: &std::collections::HashMap<String, String>) -> Option<String> {
    if jar.is_empty() {
        None
    } else {
        Some(jar.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
    }
}

/// JWT payload 解码（不验证签名）：取 iss / sub / exp
pub(super) fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let seg = parts[1].trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(seg).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 系统浏览器打开 URL（Windows：cmd /c start，隐藏控制台；仅放行 http/https 且无引号空格）
pub(super) fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains(['"', '\'', ' ']) {
        return Err(format!("拒绝打开非法 URL：{url}"));
    }
    Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(0x08000000)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开浏览器失败: {e}"))
}

fn mask_phone(p: &str) -> String {
    let c: Vec<char> = p.chars().collect();
    if c.len() >= 7 {
        format!(
            "{}****{}",
            c[..3].iter().collect::<String>(),
            c[c.len() - 4..].iter().collect::<String>()
        )
    } else {
        "****".into()
    }
}

/// OAuth 主流程（后台线程执行）：进度经 wb-oauth-progress 事件推送，结果经 wb-oauth-done。
fn oauth_flow(app: &AppHandle, state: &AppState) -> Result<(String, String), String> {
    const BASE: &str = "https://www.codebuddy.cn";
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
    let mut jar = std::collections::HashMap::new();
    let emit = |stage: &str, message: &str, auth_url: Option<&str>| {
        let _ = app.emit(
            "wb-oauth-progress",
            serde_json::json!({ "stage": stage, "message": message, "auth_url": auth_url }),
        );
    };

    // ① 发起：auth/state?platform=CLI
    emit("init", "正在请求登录 state…", None);
    let mut req = agent
        .post(&format!("{BASE}/v2/plugin/auth/state?platform=CLI"))
        .set("User-Agent", "WorkBuddy")
        .set("Origin", BASE)
        .set("Referer", &format!("{BASE}/"))
        .set("Content-Type", "application/json");
    if let Some(c) = oauth_cookie_header(&jar) {
        req = req.set("Cookie", &c);
    }
    let resp = req.send_string("{}").map_err(|e| format!("请求 auth/state 失败: {e}"))?;
    oauth_capture_cookies(&mut jar, &resp);
    let body: serde_json::Value = resp.into_json().unwrap_or_default();
    let state_id = as_str(fs_utils::dig(&body, &["state"]))
        .or_else(|| as_str(fs_utils::dig(&body, &["authState"])))
        .ok_or("auth/state 响应中未找到 state")?;
    let auth_url = as_str(fs_utils::dig(&body, &["authUrl"]))
        .or_else(|| as_str(fs_utils::dig(&body, &["auth_url"])))
        .or_else(|| as_str(fs_utils::dig(&body, &["url"])))
        .ok_or("auth/state 响应中未找到 authUrl")?;

    // ② 浏览器打开登录页
    open_in_browser(&auth_url)?;
    emit(
        "browser",
        "已在系统浏览器打开登录页：请完成扫码/登录（完成后停留在结果页即可，无需复制内容）",
        Some(&auth_url),
    );
    fs_utils::app_log(&state.data_dir, "workbuddy: OAuth 扫码流程开始（auth/state 成功）");

    // ③ 轮询 auth/token（≤300s，间隔 3s；pending/非 2xx 一律继续等待）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut token = String::new();
    let mut refresh_token = String::new();
    let mut expires_in_s: Option<i64> = None;
    let mut polls: u32 = 0;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(3));
        polls += 1;
        if polls % 10 == 1 {
            emit("polling", "等待登录完成…（最长 300 秒）", None);
        }
        let mut req = agent
            .get(&format!("{BASE}/v2/plugin/auth/token?state={state_id}"))
            .set("User-Agent", "WorkBuddy")
            .set("Origin", BASE)
            .set("Referer", &format!("{BASE}/"));
        if let Some(c) = oauth_cookie_header(&jar) {
            req = req.set("Cookie", &c);
        }
        if let Ok(resp) = req.call() {
            oauth_capture_cookies(&mut jar, &resp);
            let body: serde_json::Value = resp.into_json().unwrap_or_default();
            if let Some(t) = as_str(fs_utils::dig(&body, &["accessToken"])) {
                if !t.is_empty() {
                    token = t;
                    refresh_token = as_str(fs_utils::dig(&body, &["refreshToken"])).unwrap_or_default();
                    expires_in_s = fs_utils::dig(&body, &["expiresIn"]).and_then(|v| v.as_i64());
                    break;
                }
            }
        }
    }
    if token.is_empty() {
        return Err("登录超时（300 秒）：请重试并确保在浏览器中完成登录".into());
    }

    // ④ 取账号资料（uid/nickname；uid 兜底取 JWT sub，过期时间兜底取 JWT exp）
    let mut uid = String::new();
    let mut nickname = String::new();
    let mut phone_masked = String::new();
    let mut edition = String::new();
    let mut req = agent
        .get(&format!("{BASE}/v2/plugin/login/account?state={state_id}"))
        .set("User-Agent", "WorkBuddy")
        .set("Origin", BASE)
        .set("Referer", &format!("{BASE}/"));
    if let Some(c) = oauth_cookie_header(&jar) {
        req = req.set("Cookie", &c);
    }
    if let Ok(resp) = req.call() {
        let body: serde_json::Value = resp.into_json().unwrap_or_default();
        uid = as_str(fs_utils::dig(&body, &["uid", "userId", "user_id"])).unwrap_or_default();
        nickname = as_str(fs_utils::dig(&body, &["nickname", "nickName", "name"])).unwrap_or_default();
        phone_masked = as_str(fs_utils::dig(&body, &["phone", "mobile", "phoneNumber"]))
            .map(|p| mask_phone(&p))
            .unwrap_or_default();
        edition = as_str(fs_utils::dig(&body, &["editionType", "edition_type"])).unwrap_or_default();
    }
    if uid.is_empty() {
        uid = jwt_claims(&token)
            .and_then(|c| c.get("sub").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
    }
    let exp_s: Option<i64> = expires_in_s
        .map(|s| chrono::Utc::now().timestamp() + s)
        .or_else(|| jwt_claims(&token).and_then(|c| c.get("exp").and_then(|v| v.as_i64())));

    // 入池（同 token 稳定同 id，F-04）+ 凭证回写 token store（不外泄）
    let id = account_id_of(&token);
    let now_s = chrono::Utc::now().timestamp();
    let mut pool = load_pool(state);
    if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == id) {
        if !uid.is_empty() {
            a.uid = uid.clone();
        }
        if !nickname.is_empty() {
            a.nickname = nickname.clone();
        }
        if !phone_masked.is_empty() {
            a.phone_masked = phone_masked.clone();
        }
        if !edition.is_empty() {
            a.edition_type = edition.clone();
        }
        a.access_token_expires_at = exp_s;
        a.auth_saved_at = Some(now_s);
        a.needs_relogin = false;
        a.relogin_reason.clear();
    } else {
        pool.accounts.push(WorkBuddyAccount {
            id: id.clone(),
            uid: uid.clone(),
            nickname: nickname.clone(),
            phone_masked,
            edition_type: edition,
            access_token_expires_at: exp_s,
            auth_saved_at: Some(now_s),
            ..Default::default()
        });
    }
    save_pool(state, &pool)?;
    let creds = serde_json::json!({
        "access_token": token,
        "refresh_token": if refresh_token.is_empty() { None } else { Some(refresh_token) },
        "expires_at_ms": exp_s.map(|s| s * 1000),
    });
    upsert_token_store(state, &id, &creds)?;
    fs_utils::app_log(&state.data_dir, &format!("workbuddy: OAuth 扫码入池 {id}（{nickname}）"));

    let label = if nickname.is_empty() { id.clone() } else { nickname.clone() };
    emit("success", &format!("账号「{label}」已扫码登录并自动入池"), None);
    Ok((id, label))
}

/// OAuth 扫码登录（F-50）：后台线程执行全流程，事件驱动 UI；同时仅允许一个流程。
#[tauri::command(async)]
pub fn workbuddy_oauth_login(app: AppHandle) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if OAUTH_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("已有 OAuth 扫码流程进行中".into());
    }
    std::thread::spawn(move || {
        let payload = match AppState::new().and_then(|st| oauth_flow(&app, &st)) {
            Ok((id, nickname)) => serde_json::json!({
                "ok": true,
                "id": id,
                "nickname": nickname,
                "message": format!("账号「{nickname}」已扫码登录并自动入池"),
            }),
            Err(e) => serde_json::json!({ "ok": false, "message": e }),
        };
        let _ = app.emit("wb-oauth-done", payload);
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
    });
    Ok(())
}

#[cfg(test)]
mod oauth_tests {
    use super::*;

    #[test]
    fn jwt_claims_decodes_payload() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"u-123","iss":"https://kc.example/realms/demo"}"#);
        let token = format!("aaa.{payload}.bbb");
        let c = jwt_claims(&token).expect("claims should decode");
        assert_eq!(c.get("sub").and_then(|v| v.as_str()), Some("u-123"));
        assert_eq!(c.get("iss").and_then(|v| v.as_str()), Some("https://kc.example/realms/demo"));
    }

    #[test]
    fn mask_phone_basic() {
        assert_eq!(mask_phone("13812345678"), "138****5678");
        assert_eq!(mask_phone("123"), "****");
    }
}
