//! WorkBuddy M7 生态接入 · OAuth 扫码登录（F-50，§3.10）（原 workbuddy.rs 机械拆分）。
//!
//! 流程（无 PKCE，state 服务端签发；对齐已验证的 workbuddy2api/官方插件实现）：
//!   ① POST https://copilot.tencent.com/v2/plugin/auth/state?platform=CLI → state + authUrl
//!      （API 上游在 copilot.tencent.com，不在 www.codebuddy.cn；UA 模拟官方 CLI
//!       `CLI/2.x CodeBuddy/2.x`，Origin/Referer 指向 www.codebuddy.cn）
//!   ② 系统浏览器打开 authUrl（服务端 URL 原样优先；缺失或缺 state 凭证参数时按
//!      已验证形态 `copilot.tencent.com/login?platform=CLI&state=<state>` 构造，绝不打开裸链接）
//!   ③ GET /v2/plugin/auth/token?state= 轮询（≤300s，间隔 3s）
//!   ④ GET /v2/plugin/login/account?state= 带 Bearer 取 uid/nickname → 自动入池 + 凭证回写 token store
//! 每流程独立 cookie jar（手工捕获 Set-Cookie 回传，不引新依赖）；凭证零明文输出（不进日志/事件/UI）。

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

/// 判断 URL 的 query 中是否已含指定 key（按 & 分段取 = 前缀，大小写不敏感）
fn auth_url_query_has_key(url: &str, key: &str) -> bool {
    let Some(pos) = url.find('?') else {
        return false;
    };
    url[pos + 1..]
        .split('#')
        .next()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .any(|k| k.eq_ignore_ascii_case(key))
}

/// 从 auth/state 响应中定位 state 凭证字段（候选链，dig 自动穿透 data 等信封包裹），
/// 返回 (响应字段名, 值)；值用于轮询与登录链接构造，字段名供测试识别来源。
fn find_state_credential(body: &serde_json::Value) -> Option<(&'static str, String)> {
    const CANDIDATES: [&str; 3] = ["state", "authState", "auth_state"];
    CANDIDATES.iter().find_map(|k| {
        as_str(fs_utils::dig(body, &[k]))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| (*k, v))
    })
}

/// 宽容解析 auth/state 响应中的登录 URL（F-50，对齐参考实现 workbuddy2api/官方插件）：
/// ① 候选字段链取第一个 http(s):// 开头且 query 已含 `state` 凭证参数的 authUrl **原样打开**
///    （参考实现 `fmt.Println(st.AuthURL)` 同款：服务端 URL 从不追加参数）；
/// ② URL 缺失 / 非 http(s) / query 缺 state（上一轮故障形态：错误上游只回裸登录页链接）
///    → 按已验证形态构造 `copilot.tencent.com/login?platform=CLI&state=<state>`
///    （antigravity-tools 卡密登录链接同款拼法；workbuddy-switch 缺 authUrl 时亦按此回退），
///    绝不原样打开缺 state 的登录页链接，也绝不回退裸基础域 URL。
fn resolve_auth_url(body: &serde_json::Value, state_id: &str) -> Result<String, String> {
    const LOGIN_BASE: &str = "https://copilot.tencent.com";
    let state_id = state_id.trim();
    if state_id.is_empty() {
        return Err("auth/state 响应中 state 为空，无法构造登录链接".into());
    }
    const CANDIDATES: [&str; 6] = ["auth_url", "authUrl", "login_url", "loginUrl", "redirect_url", "url"];
    let complete = CANDIDATES
        .iter()
        .find_map(|k| {
            as_str(fs_utils::dig(body, &[k]))
                .map(|v| v.trim().to_string())
                .filter(|v| v.starts_with("https://") || v.starts_with("http://"))
                .filter(|v| auth_url_query_has_key(v, "state"))
        })
        .unwrap_or_else(|| format!("{LOGIN_BASE}/login?platform=CLI&state={state_id}"));
    Ok(complete)
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
    // API 上游在 copilot.tencent.com（workbuddy2api main.go:28 同款）——打到
    // www.codebuddy.cn 会拿到与 CLI 登录流程不匹配的裸登录链接（上一轮故障根因）
    const BASE: &str = "https://copilot.tencent.com";
    const WEB_ORIGIN: &str = "https://www.codebuddy.cn";
    // UA/头模拟官方 CLI 客户端（workbuddy2api main.go:29,38-45 同款）；
    // 自造 UA 可能被服务端降级处理
    const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
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
        .set("User-Agent", CLIENT_UA)
        .set("Accept", "application/json, text/plain, */*")
        .set("X-Requested-With", "XMLHttpRequest")
        .set("Origin", WEB_ORIGIN)
        .set("Referer", &format!("{WEB_ORIGIN}/"))
        .set("Content-Type", "application/json");
    if let Some(c) = oauth_cookie_header(&jar) {
        req = req.set("Cookie", &c);
    }
    let resp = req.send_string("{}").map_err(|e| format!("请求 auth/state 失败: {e}"))?;
    oauth_capture_cookies(&mut jar, &resp);
    let body: serde_json::Value = resp.into_json().unwrap_or_default();
    // state 用于轮询；URL 缺 state 凭证参数时用它构造 copilot 登录链接（resolve_auth_url）
    let state_id = find_state_credential(&body)
        .map(|(_, v)| v)
        .ok_or("auth/state 响应中未找到 state")?;
    let auth_url = resolve_auth_url(&body, &state_id)?;

    // ② 浏览器打开登录页
    open_in_browser(&auth_url)?;
    emit(
        "browser",
        "已在系统浏览器打开登录页：请完成扫码/登录（完成后停留在结果页即可，无需复制内容）",
        Some(&auth_url),
    );
    // 观测（一条）：auth/state 响应顶层键名 + data 内层键名 + 最终打开 URL
    // （state 为本地一次性凭证，允许出现在 URL 中；token 值绝不入日志）
    let keys_of = |v: &serde_json::Value| {
        v.as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default()
    };
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "workbuddy: OAuth auth/state 响应键[{}] data键[{}]，打开登录页 {auth_url}",
            keys_of(&body),
            body.get("data").map(|d| keys_of(d)).unwrap_or_default()
        ),
    );

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
            .set("User-Agent", CLIENT_UA)
            .set("Accept", "application/json, text/plain, */*")
            .set("X-Requested-With", "XMLHttpRequest")
            .set("Origin", WEB_ORIGIN)
            .set("Referer", &format!("{WEB_ORIGIN}/"));
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
    // login/account 参考实现带 Bearer（workbuddy2api main.go:151-160 / workbuddy-switch
    // oauth.rs:115-123 同款），不带头会被当作未认证请求拿不到资料
    let mut uid = String::new();
    let mut nickname = String::new();
    let mut phone_masked = String::new();
    let mut edition = String::new();
    let mut req = agent
        .get(&format!("{BASE}/v2/plugin/login/account?state={state_id}"))
        .set("User-Agent", CLIENT_UA)
        .set("Accept", "application/json, text/plain, */*")
        .set("X-Requested-With", "XMLHttpRequest")
        .set("Origin", WEB_ORIGIN)
        .set("Referer", &format!("{WEB_ORIGIN}/"))
        .set("Authorization", &format!("Bearer {token}"));
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

    // 入池（按账号身份幂等合并，F-04）：uid 优先 / id（token 派生）兜底匹配（与 auth 导入
    // 共用 accounts::find_uid_or_id 语义）→ 命中原位更新并**保留原 id**（同账号换发 token
    // 不再重复入池，快照/分组引用不悬空），未命中才新增；凭证副本写回保留 id 名下。
    let id = account_id_of(&token);
    let now_s = chrono::Utc::now().timestamp();
    let mut pool = load_pool(state);
    let target_id = if let Some(a) = super::accounts::find_uid_or_id(&mut pool, &uid, &id) {
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
        a.id.clone()
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
        id.clone()
    };
    save_pool(state, &pool)?;
    let creds = serde_json::json!({
        "access_token": token,
        "refresh_token": if refresh_token.is_empty() { None } else { Some(refresh_token) },
        "expires_at_ms": exp_s.map(|s| s * 1000),
    });
    upsert_token_store(state, &target_id, &creds)?;
    fs_utils::app_log(&state.data_dir, &format!("workbuddy: OAuth 扫码入池 {target_id}（{nickname}）"));

    let label = if nickname.is_empty() { target_id.clone() } else { nickname.clone() };
    emit("success", &format!("账号「{label}」已扫码登录并自动入池"), None);
    Ok((target_id, label))
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

    fn oauth_body(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("valid json")
    }

    /// ① 服务端 authUrl 已含 state → 原样打开，不补参不构造
    ///    （workbuddy2api `fmt.Println(st.AuthURL)` 同款）；data 信封包裹同样可解析
    #[test]
    fn resolve_auth_url_complete_field_untouched() {
        let body = oauth_body(r#"{"state":"s1","auth_url":"https://login.example/auth?state=tok9"}"#);
        assert_eq!(
            resolve_auth_url(&body, "s1").unwrap(),
            "https://login.example/auth?state=tok9"
        );
        let wrapped = oauth_body(
            r#"{"code":0,"msg":"","data":{"state":"st-1","authUrl":"https://copilot.tencent.com/login?platform=CLI&state=st-1"}}"#,
        );
        assert_eq!(
            resolve_auth_url(&wrapped, "st-1").unwrap(),
            "https://copilot.tencent.com/login?platform=CLI&state=st-1"
        );
    }

    /// ② 故障现场形态（参考得出的关键形态）：服务端只回裸登录页链接（query 缺 state）
    ///    → 不原样打开、也不给裸链接补参，按已验证卡密形态构造
    ///    `copilot/login?platform=CLI&state=<state>`（antigravity-tools 卡密同款拼法）
    #[test]
    fn resolve_auth_url_bare_url_replaced_by_canonical_login_link() {
        let body = oauth_body(r#"{"state":"st-123","url":"https://www.codebuddy.cn/login?platform=CLI"}"#);
        assert_eq!(
            resolve_auth_url(&body, "st-123").unwrap(),
            "https://copilot.tencent.com/login?platform=CLI&state=st-123"
        );
        let wrapped = oauth_body(r#"{"data":{"authState":"AB-9","authUrl":"https://cb.cn/login"}}"#);
        assert_eq!(
            resolve_auth_url(&wrapped, "AB-9").unwrap(),
            "https://copilot.tencent.com/login?platform=CLI&state=AB-9"
        );
        assert_eq!(find_state_credential(&wrapped).map(|(_, v)| v), Some("AB-9".to_string()));
    }

    /// ③ URL 字段缺失 / 非 http(s) → 用 state 构造同款链接（workbuddy-switch 回退同款）；
    ///    state 缺失/为空 → 明确报错，绝不打开无 state 的链接
    #[test]
    fn resolve_auth_url_missing_or_invalid_errors() {
        assert_eq!(
            resolve_auth_url(&oauth_body(r#"{"state":"s1"}"#), "s1").unwrap(),
            "https://copilot.tencent.com/login?platform=CLI&state=s1"
        );
        // 非法 authUrl（javascript:）视同缺失 → 构造，绝不打开
        assert_eq!(
            resolve_auth_url(&oauth_body(r#"{"authUrl":"javascript:alert(1)","state":"s1"}"#), "s1").unwrap(),
            "https://copilot.tencent.com/login?platform=CLI&state=s1"
        );
        // 入参 state 为空（响应亦无 state 字段可兜底）→ 报错
        assert!(resolve_auth_url(&oauth_body(r#"{"authUrl":"https://cb.cn/login?state=x"}"#), "").is_err());
        assert!(resolve_auth_url(&serde_json::json!({}), "  ").is_err());
    }

    /// ④ OAuth 自动入池的幂等匹配决策（复用 accounts::find_uid_or_id）：
    ///    同 uid 新 token（id 漂移）→ uid 优先命中旧条目；uid 缺失 → id 兜底；异账号 → 未命中
    #[test]
    fn oauth_pool_merge_matches_uid_first_then_id() {
        use super::super::accounts::find_uid_or_id;
        use super::super::common::WbPool;
        let mut pool = WbPool {
            accounts: vec![WorkBuddyAccount {
                id: "wb-oldoldoldold".into(),
                uid: "u1".into(),
                ..Default::default()
            }],
        };
        // 同 uid 换发 token（新 token 派生新 id）：按 uid 命中旧条目 → 保留原 id，不重复入池
        let hit = find_uid_or_id(&mut pool, "u1", "wb-newnewnewnew").expect("same uid must match");
        assert_eq!(hit.id, "wb-oldoldoldold");
        // login/account 未返回 uid（uid 为空）：按 id 兜底匹配
        let hit2 = find_uid_or_id(&mut pool, "", "wb-oldoldoldold").expect("id fallback must match");
        assert_eq!(hit2.uid, "u1");
        // 异账号：未命中 → 走新增分支
        assert!(find_uid_or_id(&mut pool, "u2", "wb-otherotherob").is_none());
    }
}
