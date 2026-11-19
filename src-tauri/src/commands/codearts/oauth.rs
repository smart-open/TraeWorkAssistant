//! CodeArts 浏览器 OAuth 登录（命令层）：本地回环回调 + STS 换凭据 + 入池。
//!
//! 不抢占窗口：授权 URL 用 ShellExecuteW「open」交给系统默认浏览器（无 owner 窗口、
//! 非应用内 WebView、不 set_focus）；本模块不创建/聚焦任何 Tauri 窗口。
//! 全程进度经 `codearts-oauth-progress`、终态经 `codearts-oauth-done` 推给前端。

use std::sync::{Mutex, OnceLock};
use serde_json::Value;
use tauri::{AppHandle, State};

use crate::state::AppState;
use crate::tasks::codearts_common::{self, CodeartsCreds};
use crate::tasks::codearts_oauth as oauth;

/// 本地回环端口（与 Trae 的 17388 错开；≥10000 满足真实插件要求）。
const CALLBACK_PORT: u16 = 17389;
/// 当前进行中的登录会话（全局单例，重入先取消旧的）。
static FLOW: OnceLock<Mutex<Option<FlowHandle>>> = OnceLock::new();
static OAUTH_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct FlowHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
}

fn flow_slot() -> &'static Mutex<Option<FlowHandle>> {
    FLOW.get_or_init(|| Mutex::new(None))
}

/// 停止当前回环服务器（释放端口），幂等。
fn stop_loopback() {
    if let Ok(mut slot) = flow_slot().lock() {
        if let Some(h) = slot.take() {
            let _ = h.shutdown.send(true);
        }
    }
}

/// 系统浏览器打开 URL（复制自 qoder/oauth.rs 的 issue #153 范式，无 owner 窗口）。
#[cfg(windows)]
fn open_in_browser(url: &str) -> Result<(), String> {
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
    if result as usize > 32 {
        Ok(())
    } else {
        Err(format!("打开浏览器失败（错误码 {}）", result as usize))
    }
}

#[cfg(not(windows))]
fn open_in_browser(_url: &str) -> Result<(), String> {
    Err("打开浏览器仅支持 Windows".into())
}

fn progress(app: &AppHandle, stage: &str, message: &str, auth_url: Option<&str>) {
    crate::events::emit_logged(
        app,
        "codearts-oauth-progress",
        serde_json::json!({ "stage": stage, "message": message, "auth_url": auth_url }),
        None,
    );
}

fn done(app: &AppHandle, ok: bool, id: &str, name: &str, message: &str) {
    crate::events::emit_logged(
        app,
        "codearts-oauth-done",
        serde_json::json!({ "ok": ok, "id": id, "name": name, "message": message }),
        None,
    );
}

/// STS 换取凭据 → 入池。返回 (id, name)。
fn exchange_and_store(
    state: &AppState,
    code: &str,
    verifier: &str,
    dpop_key_pem: &str,
    redirect_uri: &str,
) -> Result<(String, String), String> {
    // ① DPoP 签 token 请求
    let dpop = oauth::sign_dpop_jws(dpop_key_pem, "POST", oauth::STS_TOKEN_ENDPOINT)?;
    // URLSearchParams 形态：逐字段编码再 & 连接（整串编码会破坏 =/&）
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", oauth::CLIENT_ID)
        .append_pair("code", code)
        .append_pair("code_verifier", verifier)
        .append_pair("grant_type", "authorization_code")
        .append_pair("redirect_uri", redirect_uri)
        .finish();

    let agent = crate::tasks::http_agent(30);
    // 注意：STS token 端点只认 DPoP 头，绝不发 Authorization——
    // 该端点会把 Authorization 当 AK/SK 签名解析，`DPoP <jws>` 会被判 APIGW.0301。
    let resp = agent
        .post(oauth::STS_TOKEN_ENDPOINT)
        .set("DPoP", &dpop)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let text = r.into_string().unwrap_or_default();
                format!("STS token 请求失败（HTTP {code}）：{}", text.chars().take(300).collect::<String>())
            }
            other => format!("STS token 请求网络错误：{other}"),
        })?;
    let token: Value = resp.into_json().map_err(|e| format!("STS 响应解析失败：{e}"))?;
    let creds: CodeartsCreds = oauth::credential_from_token(&token, verifier, dpop_key_pem)?;

    // ② 尽力取稳定 user_id（失败不阻断登录）
    let (user_id, domain_id) = fetch_identity(&creds).unwrap_or_default();

    // ③ 入池（OAuth 账号 id 由 user_id 派生，重复登录幂等）
    let id = codearts_common::oauth_account_id_of(&user_id, &domain_id, &creds.refresh_token);
    store_oauth_account(state, &id, &creds, &user_id)?;
    let name = if user_id.is_empty() {
        format!("CodeArts {}", &id[..id.len().min(12)])
    } else {
        user_id
    };
    Ok((id, name))
}

/// 尽力拉 caller-identity 取 user_id/domain_id（用临时 security_token）。
fn fetch_identity(creds: &CodeartsCreds) -> Result<(String, String), String> {
    let agent = crate::tasks::http_agent(10);
    let resp = agent
        .get(oauth::STS_IDENTITY_ENDPOINT)
        .set("X-Auth-Token", &creds.security_token)
        .call()
        .map_err(|e| format!("身份查询失败：{e}"))?;
    let v: Value = resp.into_json().map_err(|e| format!("身份响应解析失败：{e}"))?;
    let uid = v.get("user_id").and_then(Value::as_str).unwrap_or("").to_string();
    let domain = v.get("domain_id").and_then(Value::as_str).unwrap_or("").to_string();
    Ok((uid, domain))
}

/// 入池：写凭据（vault）+ 账号池记录（OAuth 来源）。
fn store_oauth_account(
    state: &AppState,
    id: &str,
    creds: &CodeartsCreds,
    user_id: &str,
) -> Result<(), String> {
    // 先写凭据（vault 失败不落库）
    codearts_common::save_token(&state.data_dir, id, creds)?;
    // 再更新账号池（持 codearts_pool_lock，防并发整池覆盖）
    let _guard = state.codearts_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut accounts = crate::commands::codearts::common::load_pool_checked(state)?;
    let ak_masked = {
        let ak = &creds.ak;
        let n = ak.chars().count();
        if n <= 8 { "****".into() }
        else {
            let head: String = ak.chars().take(4).collect();
            let tail: String = ak.chars().skip(n - 4).collect();
            format!("{head}****{tail}")
        }
    };
    if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
        a.ak_masked = ak_masked;
        a.has_credential = true;
        a.note = if user_id.is_empty() { a.note.clone() } else { format!("OAuth · {user_id}") };
    } else {
        accounts.push(crate::commands::codearts::common::CodeartsAccount {
            id: id.to_string(),
            name: if user_id.is_empty() { "CodeArts OAuth".into() } else { format!("CodeArts {user_id}") },
            ak_masked,
            has_credential: true,
            note: format!("OAuth · {}", if user_id.is_empty() { "—" } else { user_id }),
            added_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        });
    }
    crate::commands::codearts::common::save_pool(state, &accounts)
}

/// 启动 CodeArts OAuth 登录：起回环 → 生成 PKCE/DPoP → 系统浏览器打开授权页。
#[tauri::command]
pub async fn codearts_oauth_login(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    if OAUTH_CANCEL.swap(false, std::sync::atomic::Ordering::SeqCst) {
        // 上次取消标志消费掉
    }
    stop_loopback();
    let st = state.inner().clone();

    let (verifier, challenge) = oauth::pkce_pair()?;
    let dpop_key = oauth::generate_dpop_key()?;
    let ticket_id = crate::commands::oauth::random_hex_result(32)?;
    let redirect_uri = format!("http://127.0.0.1:{CALLBACK_PORT}{}", oauth::REDIRECT_PATH);

    // 起 axum 回环服务器（固定端口；绑定失败直接报，让前端提示用户手动粘贴兜底）
    use axum::extract::Query;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use std::collections::HashMap;

    let verifier_cb = verifier.clone();
    let dpop_cb = dpop_key.clone();
    let redirect_cb = redirect_uri.clone();
    let app_cb = app.clone();
    let st_cb = st.clone();

    let callback = move |Query(params): Query<HashMap<String, String>>| {
        let app = app_cb.clone();
        let st = st_cb.clone();
        let verifier = verifier_cb.clone();
        let dpop = dpop_cb.clone();
        let redirect = redirect_cb.clone();
        async move {
            let code = params.get("code").cloned();
            let ok = code.is_some();
            if let Some(code) = code {
                let data_dir_for_log = st.data_dir.clone();
                let (v, d, r) = (verifier.clone(), dpop.clone(), redirect.clone());
                let result = tokio::task::spawn_blocking(move || {
                    exchange_and_store(&st, &code, &v, &d, &r)
                })
                .await;
                match result {
                    Ok(Ok((id, name))) => {
                        progress(&app, "done", "授权完成，凭据已入池", None);
                        done(&app, true, &id, &name, "CodeArts 登录成功");
                    }
                    Ok(Err(e)) => {
                        crate::fs_utils::app_log(&data_dir_for_log, &format!("CodeArts OAuth 入池失败: {e}"));
                        progress(&app, "error", "凭据换取失败，请重试", None);
                        done(&app, false, "", "", &e);
                    }
                    Err(e) => {
                        done(&app, false, "", "", &format!("后台任务失败：{e}"));
                    }
                }
            } else {
                let msg = params.get("error_description").cloned().unwrap_or_else(|| "授权未返回 code".into());
                done(&app, false, "", "", &msg);
            }
            stop_loopback();
            // 重定向回 portal 结果页（与真实 IDE 插件一致）；失败时若重定向目标不可达，
            // 浏览器仍会尝试加载，用户可返回应用查看错误 toast。
            axum::response::Redirect::temporary(&oauth::portal_result_url(ok)).into_response()
        }
    };

    let addr = format!("127.0.0.1:{CALLBACK_PORT}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("本地回调端口 {CALLBACK_PORT} 绑定失败（可能被占用）：{e}"))?;
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let router = Router::new().route(oauth::REDIRECT_PATH, get(callback));
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    });
    let join = tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("[codearts-oauth] 回环服务器异常: {e}");
        }
    });
    if let Ok(mut slot) = flow_slot().lock() {
        *slot = Some(FlowHandle { shutdown: shutdown_tx });
    }
    // shut down the unused join handle at end (server owns it)
    drop(join);

    let auth_url = oauth::build_authorize_url(CALLBACK_PORT, &ticket_id, &challenge);
    progress(&app, "browser", "正在打开华为云授权页…", Some(&auth_url));
    // 系统默认浏览器打开（无 owner 窗口，不抢占应用）——阻塞 IO 放 spawn_blocking
    let url = auth_url.clone();
    tokio::task::spawn_blocking(move || open_in_browser(&url))
        .await
        .map_err(|e| format!("浏览器打开任务失败：{e}"))??;
    Ok(auth_url)
}

/// 取消登录（停回环服务器）。
#[tauri::command]
pub fn codearts_oauth_cancel() -> Result<(), String> {
    OAUTH_CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
    stop_loopback();
    Ok(())
}
