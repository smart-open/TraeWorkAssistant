//! WorkBuddy M7 生态接入 · OAuth 扫码登录（F-50，§3.10）（原 workbuddy.rs 机械拆分）。
//!
//! 流程（无 PKCE，state 服务端签发；对齐已验证的 workbuddy2api/官方插件实现）：
//!   ① POST https://copilot.tencent.com/v2/plugin/auth/state?platform=workbuddy → state + authUrl
//!      （API 上游在 copilot.tencent.com，不在 www.codebuddy.cn；UA 三段式
//!       `WorkBuddy/5.8.3 WorkBuddy/5.8.3 CLI/2.161.0`，Origin/Referer 指向 www.codebuddy.cn。
//!       2026-10-18 实测：platform=workbuddy + WorkBuddy 桌面 UA 两者同时满足，服务端才返回
//!       www.workbuddy.cn/login 新登录页）
//!   ② 系统浏览器打开 authUrl（服务端 URL 原样优先；缺失或缺 state 凭证参数时按
//!      REGION_CN 当前形态 `www.workbuddy.cn/login?platform=workbuddy&state=<state>&version=5.8.3`
//!      构造，绝不打开裸链接）
//!   ③ GET /v2/plugin/auth/token?state= 轮询（≤300s，间隔 3s）
//!   ④ GET /v2/plugin/login/account?state= 带 Bearer 取 uid/nickname → 自动入池 + 凭证回写 token store
//! 每流程独立 cookie jar（手工捕获 Set-Cookie 回传，不引新依赖）；凭证零明文输出（不进日志/事件/UI）。

use tauri::{AppHandle, Emitter, Manager};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{account_id_of, as_str, load_pool, save_pool, upsert_token_store, WorkBuddyAccount, WbPool};

/// OAuth 防重入（CN/Intl 共享闸门，一次仅允许一个流程）
static OAUTH_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 闸门 RAII 守卫（对齐 qoder OAuthGuard 同款）：drop 自动复位 OAUTH_RUNNING，
/// 覆盖线程内 panic 与一切提前退出路径——防共享闸门永久卡死（前端弹框永挂
/// loading，CN/Intl 后续登录全被拒）
struct OAuthGuard;
impl Drop for OAuthGuard {
    fn drop(&mut self) {
        OAUTH_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

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

/// 系统浏览器打开 URL（issue #153 修复 4：ShellExecuteW「open」动词直开 URL，
/// 由系统按协议关联选默认浏览器——cmd start 依赖 shell 解析且控制台隐藏不稳定；
/// 仅放行 http/https 且无引号空格，单参数直传无 shell 层，& / % 等保留字无需转义）
pub(super) fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains(['"', '\'', ' ']) {
        return Err(format!("拒绝打开非法 URL：{url}"));
    }
    let url_wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let verb_wide: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb_wide.as_ptr(),
            url_wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // 返回值 > 32 = 成功的 HINSTANCE；≤ 32 为 SE_ERR 错误码
    if result as usize > 32 {
        Ok(())
    } else {
        Err(format!("打开浏览器失败（ShellExecuteW 错误码 {}）", result as usize))
    }
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
///    → 按区域配置构造 `{login_base}/login?platform={platform}&state=<state>[&version=]`
///    （CN 当前为 `www.workbuddy.cn/login?platform=workbuddy&state=<state>&version=5.8.3`；
///    antigravity-tools 卡密登录链接同款拼法），
///    绝不原样打开缺 state 的登录页链接，也绝不回退裸基础域 URL。
fn resolve_auth_url(
    body: &serde_json::Value,
    state_id: &str,
    ep: &crate::tasks::wb_common::WbRegionEndpoints,
) -> Result<String, String> {
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
        .unwrap_or_else(|| {
            // 回退构造（CN：`www.workbuddy.cn/login?platform=workbuddy&state=<state>&version=5.8.3`，
            // 与官方桌面端 5.8.3 打开形态一致；Intl：R-1 实测官方带 version 参数，对齐带上）
            let mut link = format!("{}/login?platform={}&state={}", ep.login_base, ep.platform, state_id);
            if !ep.client_version.is_empty() {
                link.push_str(&format!("&version={}", ep.client_version));
            }
            link
        });
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

/// OAuth 入池落库后端（§3.1 不交织：CN 走 wb 池现状不动，Intl 走 buddy-ai 独立池）
pub(super) enum PoolSink {
    /// 国内 wb 池（wb_accounts / wb_tokens 表 + vault ns "wb"）
    Cn,
    /// 国际 buddy-ai 独立池（buddy_ai_accounts / buddy_ai_tokens 表 + vault ns "buddy-ai"）
    BuddyAi,
}

impl PoolSink {
    /// 事件前缀（wb-oauth-* / buddy-ai-oauth-*，UI 分区原则不混流）
    pub(super) fn event_prefix(&self) -> &'static str {
        match self {
            Self::Cn => "wb",
            Self::BuddyAi => "buddy-ai",
        }
    }

    fn lock<'a>(&self, state: &'a AppState) -> std::sync::MutexGuard<'a, ()> {
        match self {
            Self::Cn => state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner()),
            Self::BuddyAi => state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    fn load(&self, state: &AppState) -> WbPool {
        match self {
            Self::Cn => load_pool(state),
            Self::BuddyAi => super::buddy_ai::load_buddy_pool(state),
        }
    }

    fn save(&self, state: &AppState, pool: &WbPool) -> Result<(), String> {
        match self {
            Self::Cn => save_pool(state, pool),
            Self::BuddyAi => super::buddy_ai::save_buddy_pool(state, pool),
        }
    }

    fn upsert_token(&self, state: &AppState, id: &str, creds: &serde_json::Value) -> Result<(), String> {
        match self {
            Self::Cn => upsert_token_store(state, id, creds),
            Self::BuddyAi => {
                crate::tasks::wb_common::buddy_ai_token_store_upsert(&state.data_dir, id, creds)
            }
        }
    }

    /// 既有凭证 rec（重扫码合并写回用；BuddyAi = buddy_ai_tokens 表）
    fn load_token(&self, state: &AppState, id: &str) -> serde_json::Value {
        match self {
            Self::Cn => {
                crate::tasks::wb_common::load_token_store(state)
                    .get("tokens")
                    .and_then(|t| t.get(id))
                    .cloned()
                    .unwrap_or_default()
            }
            Self::BuddyAi => {
                crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir)
                    .get("tokens")
                    .and_then(|t| t.get(id))
                    .cloned()
                    .unwrap_or_default()
            }
        }
    }
}

/// OAuth 主流程（后台线程执行）：进度经 `<prefix>-oauth-progress` 事件推送，结果经 `<prefix>-oauth-done`。
/// 区域参数化（§4.1）：CN 保持 copilot.tencent.com 链路原样；Intl 按 R-1 实测结果注入
/// `WbRegionEndpoints::REGION_GLOBAL`（auth_base/workbuddy.ai + platform=workbuddy-ai + version）。
pub(super) fn oauth_flow(
    app: &AppHandle,
    state: &AppState,
    ep: &'static crate::tasks::wb_common::WbRegionEndpoints,
    sink: PoolSink,
) -> Result<(String, String), String> {
    // API 上游按区域取用：CN = copilot.tencent.com（workbuddy2api main.go:28 同款——打到
    // www.codebuddy.cn 会拿到与 CLI 登录流程不匹配的裸登录链接）；Intl = www.workbuddy.ai（R-1）
    let base = ep.auth_base;
    let web_origin = ep.web_origin;
    // UA/头模拟官方客户端（CN/Intl 均为 WorkBuddy 桌面端身份——CN 5.8.3 / Intl 5.7.6，
    // 2026-10-18 实测 CN 需 platform=workbuddy + 桌面 UA 双满足才返回新登录页）；
    // 自造 UA 可能被服务端降级处理
    let client_ua = ep.ua;
    let intl = ep.platform == "workbuddy-ai";
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
    let mut jar = std::collections::HashMap::new();
    let progress_event = format!("{}-oauth-progress", sink.event_prefix());
    let emit = |stage: &str, message: &str, auth_url: Option<&str>| {
        let _ = app.emit(
            &progress_event,
            serde_json::json!({ "stage": stage, "message": message, "auth_url": auth_url }),
        );
    };

    // ① 发起：auth/state?platform=<区域 platform>
    emit("init", "正在请求登录 state…", None);
    let mut req = agent
        .post(&format!("{base}/v2/plugin/auth/state?platform={}", ep.platform))
        .set("User-Agent", client_ua)
        .set("Accept", "application/json, text/plain, */*")
        .set("X-Requested-With", "XMLHttpRequest")
        .set("Origin", web_origin)
        .set("Referer", &format!("{web_origin}/"))
        .set("Content-Type", "application/json");
    if intl {
        // 附录 B：state 端点匿名四连头（agent2api 同款；与既有 X-No-* 占位机制同源）
        req = req
            .set("X-No-Authorization", "true")
            .set("X-No-User-Id", "true")
            .set("X-No-Enterprise-Id", "true")
            .set("X-No-Department-Info", "true");
    }
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
    let auth_url = resolve_auth_url(&body, &state_id, ep)?;

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
            "{}: OAuth auth/state 响应键[{}] data键[{}]，打开登录页 {auth_url}",
            if intl { "buddy-ai" } else { "workbuddy" },
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
            .get(&format!("{base}/v2/plugin/auth/token?state={state_id}"))
            .set("User-Agent", client_ua)
            .set("Accept", "application/json, text/plain, */*")
            .set("X-Requested-With", "XMLHttpRequest")
            .set("Origin", web_origin)
            .set("Referer", &format!("{web_origin}/"));
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
    // ④ 取账号资料：CN = /v2/plugin/login/account（带 Bearer，workbuddy2api 同款）；
    // Intl = /v2/plugin/auth/account（R-1 实测国际域单数端点 404 → 静默容错，
    // uid 走 JWT sub 兜底、账号标识落 billing 侧，与端点矩阵口径一致）
    let mut req = agent
        .get(&format!("{base}{}?state={state_id}", ep.account_path))
        .set("User-Agent", client_ua)
        .set("Accept", "application/json, text/plain, */*")
        .set("X-Requested-With", "XMLHttpRequest")
        .set("Origin", web_origin)
        .set("Referer", &format!("{web_origin}/"))
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
    // 读-改-写互斥：块作用域罩住 load→合并→save 池段，防旧副本整文档覆盖；
    // upsert_token_store 留在锁外，缩短临界区
    let target_id = {
        let _guard = sink.lock(state);
        let mut pool = sink.load(state);
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
        sink.save(state, &pool)?;
        target_id
    };
    let creds = serde_json::json!({
        "access_token": token,
        "refresh_token": if refresh_token.is_empty() { None } else { Some(refresh_token) },
        "expires_at_ms": exp_s.map(|s| s * 1000),
    });
    // 重扫码合并写回（BuddyAi sink）：upsert 是整行 REPLACE，从零构造会抹掉
    // last_daily_chat（当日打卡幂等失效→重复跑六步链）/uid/domain 等既有键——
    // 与导入路径 BL-2 同类缺陷。新键优先，旧键缺省补齐；新码未回发 rt 时保留旧 rt
    if matches!(sink, PoolSink::BuddyAi) {
        // 审查修复（M14）：凭证读-合并-写全程持每账号刷新锁（对齐导入路径
        // buddy_ai_account_import_creds 同款锁域）——与网关 401 自愈轮换并发时，
        // 锁外读旧 rec 再整行 upsert 会覆盖刚轮换落库的新 rt（账号被迫重登）
        let refresh_lock = crate::tasks::wb_common::refresh_lock_for(&target_id);
        let _refresh_guard = refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
        let existing = sink.load_token(state, &target_id);
        let mut creds = creds;
        if let (Some(obj), Some(eobj)) = (creds.as_object_mut(), existing.as_object()) {
            for (k, v) in eobj {
                obj.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if creds.get("refresh_token").map(serde_json::Value::is_null).unwrap_or(false) {
            if let Some(old_rt) = existing.get("refresh_token").filter(|v| !v.is_null()) {
                creds["refresh_token"] = old_rt.clone();
            }
        }
        sink.upsert_token(state, &target_id, &creds)?;
    } else {
        sink.upsert_token(state, &target_id, &creds)?;
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "{}: OAuth 扫码入池 {target_id}（{nickname}）",
            if intl { "buddy-ai" } else { "workbuddy" }
        ),
    );

    let label = if nickname.is_empty() { target_id.clone() } else { nickname.clone() };
    emit("success", &format!("账号「{label}」已扫码登录并自动入池"), None);
    Ok((target_id, label))
}

/// OAuth 扫码登录（F-50）：后台线程执行全流程，事件驱动 UI；同时仅允许一个流程。
#[tauri::command(async)]
pub fn workbuddy_oauth_login(app: AppHandle) -> Result<(), String> {
    oauth_login_async(app, &crate::tasks::wb_common::REGION_CN, PoolSink::Cn)
}

/// 通用异步启动器（CN/Intl 共用 OAUTH_RUNNING 全局闸门，一次仅允许一个流程；
/// 进度/终态事件按池前缀分流：wb-oauth-* / buddy-ai-oauth-*）
pub(super) fn oauth_login_async(
    app: AppHandle,
    ep: &'static crate::tasks::wb_common::WbRegionEndpoints,
    sink: PoolSink,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if OAUTH_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("已有 OAuth 扫码流程进行中".into());
    }
    let done_event = format!("{}-oauth-done", sink.event_prefix());
    // 审查修复（M1）：复用主进程 manage 的 AppState 共享实例——其 wb_pool_lock /
    // buddy_ai_pool_lock / jwt_refresh_lock 等 Arc 字段与命令、调度线程同源。
    // 原 AppState::new() 在本线程构造全新实例，锁域互不相干 → OAuth 写池与
    // 命令/调度写池并发时失去互斥，可能整池覆盖丢账号。
    let st = app.state::<AppState>().inner().clone();
    // 审查修复：裸 thread::spawn 线程创建失败即 panic 且发生在闸门复位之外 →
    // OAUTH_RUNNING 永久 true、done 事件永不发出，CN/Intl 共享闸门后续登录全被拒。
    // 改为 Builder 命名线程 + spawn 返回 io::Result：失败复位闸门并返回 Err；
    // 线程内闸门复位统一由 OAuthGuard RAII 承担（含 panic 路径）
    let spawned = std::thread::Builder::new()
        .name("wb-oauth".into())
        .spawn(move || {
            let _guard = OAuthGuard;
            // panic 兜底（审查修复）：oauth_flow panic 原实现会让 OAUTH_RUNNING 永久卡死
            //（CN/Intl 共享闸门）且 done 事件永不发出 → 前端弹框永远 loading 直到重启。
            // catch_unwind 确保复位 + 补发失败终态
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                oauth_flow(&app, &st, ep, sink)
            }));
            let payload = match outcome {
                Ok(Ok((id, nickname))) => serde_json::json!({
                    "ok": true,
                    "id": id,
                    "nickname": nickname,
                    "message": format!("账号「{nickname}」已扫码登录并自动入池"),
                }),
                Ok(Err(e)) => serde_json::json!({ "ok": false, "message": e }),
                Err(_) => serde_json::json!({
                    "ok": false,
                    "message": "OAuth 流程内部异常（已恢复），请重试或查看日志"
                }),
            };
            // 终态事件：emit 失败落日志（issue #44 遗留项——前端 OAuth 弹框依赖此事件收尾）
            crate::events::emit_logged(&app, &done_event, payload, None);
            // OAUTH_RUNNING 复位由 _guard drop 承担（含 panic 路径）
        });
    if spawned.is_err() {
        OAUTH_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
        return Err("OAuth 后台线程启动失败，请重试".into());
    }
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

    /// 测试便捷包装：CN 端点形态（与既有断言字面量同源）
    fn resolve_cn(body: &serde_json::Value, state_id: &str) -> Result<String, String> {
        resolve_auth_url(body, state_id, &crate::tasks::wb_common::REGION_CN)
    }

    /// ① 服务端 authUrl 已含 state → 原样打开，不补参不构造
    ///    （workbuddy2api `fmt.Println(st.AuthURL)` 同款）；data 信封包裹同样可解析
    #[test]
    fn resolve_auth_url_complete_field_untouched() {
        let body = oauth_body(r#"{"state":"s1","auth_url":"https://login.example/auth?state=tok9"}"#);
        assert_eq!(
            resolve_cn(&body, "s1").unwrap(),
            "https://login.example/auth?state=tok9"
        );
        let wrapped = oauth_body(
            r#"{"code":0,"msg":"","data":{"state":"st-1","authUrl":"https://copilot.tencent.com/login?platform=CLI&state=st-1"}}"#,
        );
        assert_eq!(
            resolve_cn(&wrapped, "st-1").unwrap(),
            "https://copilot.tencent.com/login?platform=CLI&state=st-1"
        );
    }

    /// ② 故障现场形态：服务端只回裸登录页链接（query 缺 state）
    ///    → 不原样打开、也不给裸链接补参，按 REGION_CN 当前配置构造
    ///    `www.workbuddy.cn/login?platform=workbuddy&state=<state>&version=5.8.3`
    #[test]
    fn resolve_auth_url_bare_url_replaced_by_canonical_login_link() {
        let body = oauth_body(r#"{"state":"st-123","url":"https://www.codebuddy.cn/login?platform=CLI"}"#);
        assert_eq!(
            resolve_cn(&body, "st-123").unwrap(),
            "https://www.workbuddy.cn/login?platform=workbuddy&state=st-123&version=5.8.3"
        );
        let wrapped = oauth_body(r#"{"data":{"authState":"AB-9","authUrl":"https://cb.cn/login"}}"#);
        assert_eq!(
            resolve_cn(&wrapped, "AB-9").unwrap(),
            "https://www.workbuddy.cn/login?platform=workbuddy&state=AB-9&version=5.8.3"
        );
        assert_eq!(find_state_credential(&wrapped).map(|(_, v)| v), Some("AB-9".to_string()));
    }

    /// ③ URL 字段缺失 / 非 http(s) → 用 state 构造 REGION_CN 回退链接；
    ///    state 缺失/为空 → 明确报错，绝不打开无 state 的链接
    #[test]
    fn resolve_auth_url_missing_or_invalid_errors() {
        assert_eq!(
            resolve_cn(&oauth_body(r#"{"state":"s1"}"#), "s1").unwrap(),
            "https://www.workbuddy.cn/login?platform=workbuddy&state=s1&version=5.8.3"
        );
        // 非法 authUrl（javascript:）视同缺失 → 构造，绝不打开
        assert_eq!(
            resolve_cn(&oauth_body(r#"{"authUrl":"javascript:alert(1)","state":"s1"}"#), "s1").unwrap(),
            "https://www.workbuddy.cn/login?platform=workbuddy&state=s1&version=5.8.3"
        );
        // 入参 state 为空（响应亦无 state 字段可兜底）→ 报错
        assert!(resolve_cn(&oauth_body(r#"{"authUrl":"https://cb.cn/login?state=x"}"#), "").is_err());
        assert!(resolve_cn(&serde_json::json!({}), "  ").is_err());
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
