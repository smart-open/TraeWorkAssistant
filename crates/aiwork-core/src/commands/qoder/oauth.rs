//! Qoder OAuth 设备流命令（F-80；R-10 抓包固化，自 main 平移）：
//! 设备授权会话构造 → Web 前端展示授权页链接（qoder.cn/device/selectAccounts）
//! → 轮询 deviceToken/poll → dt- 令牌入池。
//! 事件契约对齐 wb-oauth（qoder-oauth-progress / qoder-oauth-done）。
//!
//! Web 化改造：open_in_browser 删除（Web 前端自行打开 auth_url，
//! 对齐 workbuddy::oauth 同款裁剪）；桌面事件推送改 Emitter 回调。

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;

use crate::fs_utils;
use crate::state::AppState;
use crate::tasks::qoder_oauth;
use crate::tasks::{http_agent, qoder_common};

use super::common::{account_id_of, with_pool_mut, QoderAccount};

/// OAuth 防重入（与 wb oauth 同款 AtomicBool；RAII guard 保证异常路径复位）
static OAUTH_RUNNING: AtomicBool = AtomicBool::new(false);
/// 取消标志（用户在弹框点「取消授权」）：轮询线程检测到即发失败终态并退出
static OAUTH_CANCEL: AtomicBool = AtomicBool::new(false);

struct OAuthGuard;
impl Drop for OAuthGuard {
    fn drop(&mut self) {
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// OAuth 进度事件回调（Web 化替代 AppHandle::emit）：(事件名, 载荷)。
/// 事件名 = "qoder-oauth-progress"（进度/授权链接）| "qoder-oauth-done"（终态），
/// 与桌面 emit 事件同名。
pub type QoderOauthEmitter = std::sync::Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

fn emit_progress(emit: &QoderOauthEmitter, stage: &str, message: &str, auth_url: Option<&str>) {
    let _ = emit(
        "qoder-oauth-progress",
        json!({ "stage": stage, "message": message, "auth_url": auth_url }),
    );
}

/// 终态事件：emit 失败落日志（issue #44 约定对齐，此前 `let _` 静默吞错——
/// 前端 oauthRunning 弹窗将永挂且无任何日志线索）
fn emit_done(
    emit: &QoderOauthEmitter,
    data_dir: &std::path::Path,
    ok: bool,
    id: &str,
    nickname: &str,
    message: &str,
) {
    let payload = json!({ "ok": ok, "id": id, "nickname": nickname, "message": message });
    // docker 侧 Emitter 回调返回 ()（SSE 桥 send 错误已在桥内吞掉），无法像桌面
    // AppHandle::emit 那样探测失败；payload 经序列化自检，异常时落日志兜底
    if serde_json::to_string(&payload).is_err() {
        fs_utils::app_log(data_dir, "qoder OAuth 终态事件载荷序列化失败");
    }
    emit("qoder-oauth-done", payload);
}

/// 发起 Qoder OAuth 设备流登录：
/// ① 构造 PKCE 会话并经进度事件下发授权页链接（Web 前端自行打开）→
/// ② 后台线程 1s 轮询（404=pending，200=授权完成）→ ③ dt- 令牌入池
/// （幂等：同 token 稳定同 id）并回填 uid/昵称/套餐 + 网关池热重载。
/// `compat`（2026-10-02 审查预案）：兼容模式——授权 URL 不带 client_id（社区实现
/// 验证可用）。官方常量被 Qoder 轮换导致授权页「参数无效」时，前端在授权超时后
/// 自动改用本模式重试。
pub fn qoder_oauth_login(
    state: &AppState,
    emit: QoderOauthEmitter,
    compat: Option<bool>,
) -> Result<String, String> {
    if OAUTH_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("已有 OAuth 登录在执行中，请等待完成".into());
    }
    // 复位取消标志（上一轮会话的取消请求不应影响本次登录）
    OAUTH_CANCEL.store(false, Ordering::SeqCst);
    let flow = if compat.unwrap_or(false) {
        qoder_oauth::DeviceFlow::new_compat()
    } else {
        qoder_oauth::DeviceFlow::new()
    };
    let auth_url = flow.auth_url.clone();
    emit_progress(&emit, "init", "请打开 Qoder 授权页完成账号授权", Some(&auth_url));
    emit_progress(&emit, "browser", "请在浏览器中完成 Qoder 账号授权（登录并确认）", Some(&auth_url));

    let state2 = state.clone();
    // flow 整体移交工作线程（nonce/verifier 会话一致性）
    // I17：命名线程；spawn 失败必须复位防重入标志，否则后续登录永久被拒
    let auth_url_ret = auth_url.clone();
    let spawned = std::thread::Builder::new()
        .name("qoder-oauth".into())
        .spawn(move || {
        let _guard = OAuthGuard;
        // panic 不外泄线程：捕获后补发失败终态（OAUTH_RUNNING 由 OAuthGuard drop 复位），
        // 否则前端 oauth 弹窗运行态永挂
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let agent = http_agent(15);
            emit_progress(&emit, "polling", "等待授权完成…", Some(&auth_url));
            let started = std::time::Instant::now();
            loop {
                if started.elapsed().as_millis() as u64 > qoder_oauth::POLL_TIMEOUT_MS {
                    fs_utils::app_log(&state2.data_dir, "qoder OAuth 登录超时：180s 内未完成授权");
                    emit_done(
                        &emit,
                        &state2.data_dir,
                        false,
                        "",
                        "",
                        "授权超时：请在浏览器完成授权后重试；若浏览器页面曾提示「参数无效」，再次点击 OAuth登录 将自动改用兼容模式",
                    );
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(qoder_oauth::POLL_INTERVAL_MS));
                if OAUTH_CANCEL.load(Ordering::SeqCst) {
                    fs_utils::app_log(&state2.data_dir, "qoder OAuth 登录已由用户取消");
                    emit_done(&emit, &state2.data_dir, false, "", "", "已取消授权");
                    return;
                }
                let (status, body) = qoder_oauth::poll_once(&agent, &flow);
                match status {
                    // pending：尚未授权（R-10 实测 404 NotFound）
                    404 => continue,
                    200 => {
                        let Some(b) = body else {
                            emit_done(&emit, &state2.data_dir, false, "", "", "授权响应非 JSON，请重试");
                            return;
                        };
                        // nonce 回验（审查 L）：响应必须属于本次会话，防 poll 响应
                        // 被替换为其他会话的授权结果
                        let Some((creds, uid)) = qoder_oauth::parse_poll_success(&b, &flow.nonce) else {
                            emit_done(&emit, &state2.data_dir, false, "", "", "授权响应校验失败（nonce 不匹配或缺少令牌字段），请重试");
                            return;
                        };
                        // 导入阶段进度事件（审查 P3）：import_device_creds 含网络请求
                        //（userinfo/plan 拉取）与 vault 落库，慢网下 30s+ 无事件会被
                        // 用户当作卡死而反复重试/取消——先报「已授权」再导入
                        emit_progress(&emit, "importing", "授权成功，正在导入账号凭证…", None);
                        match import_device_creds(&state2, creds, &uid) {
                            Ok((id, nickname)) => {
                                fs_utils::app_log(&state2.data_dir, &format!("qoder OAuth 登录成功: {id}"));
                                // 网关池热重载：重登清除 needs_relogin / 新账号入池后
                                // 即时恢复调度（否则禁用态残留到重启/手动 pool_set；
                                // 服务未运行时 no-op）
                                crate::api_server::runtime::reload_pools_after_change(&state2);
                                emit_done(
                                    &emit,
                                    &state2.data_dir,
                                    true,
                                    &id,
                                    &nickname,
                                    &format!("授权成功，账号 {nickname} 已入池"),
                                );
                            }
                            Err(e) => emit_done(&emit, &state2.data_dir, false, "", "", &format!("凭证入库失败: {e}")),
                        }
                        return;
                    }
                    // 授权会话过期/被撤销等异常状态：立即终止（避免轮询轰炸）
                    400 | 401 | 403 | 410 => {
                        let msg = match body
                            .as_ref()
                            .and_then(|b| crate::fs_utils::dig(b, &["errorMessage", "error_message", "message"]))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                        {
                            "" => format!("授权会话失效（HTTP {status}），请重新发起登录"),
                            m => format!("授权失败（HTTP {status}）：{m}"),
                        };
                        fs_utils::app_log(&state2.data_dir, &format!("qoder OAuth 终止: {msg}"));
                        emit_done(&emit, &state2.data_dir, false, "", "", &msg);
                        return;
                    }
                    // 网络抖动等其他状态：继续轮询直至超时
                    _ => continue,
                }
            }
        }));
        if result.is_err() {
            fs_utils::app_log(&state2.data_dir, "qoder OAuth 线程 panic（已捕获，补发失败终态）");
            emit_done(&emit, &state2.data_dir, false, "", "", "OAuth 登录线程异常终止，请重试");
        }
    });
    if spawned.is_err() {
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
        return Err("OAuth 后台线程启动失败，请重试".into());
    }
    // 同步返回授权页链接：前端在点击手势的 transient activation 窗口内 window.open
    // 自行打开授权页（Web 化替代桌面版 open_in_browser；进度事件仍兜底下发同一链接）
    Ok(auth_url_ret)
}

/// 取消进行中的 OAuth 轮询（弹框「取消授权」）：置标志后轮询线程自行收尾。
pub fn qoder_oauth_cancel() -> Result<(), String> {
    OAUTH_CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

/// 设备流凭证入库：幂等入池（uid 优先匹配，防重复）+ token store + userinfo/plan 回填。
/// 返回 (id, 展示名)。命中已有账号保留原 id（I10：换发派生 id 漂移会使
/// 快照/分组/外部引用悬空，对照蓝本 merge_auth_entry 的取舍）。
fn import_device_creds(
    state: &AppState,
    mut creds: qoder_common::QoderCreds,
    uid: &str,
) -> Result<(String, String), String> {
    let id = account_id_of(&creds.access_token);
    // userinfo/plan 回填（失败容错：dt- 对 openapi 端点的可用性随客户端一致）
    let agent = http_agent(15);
    let probe = qoder_common::QoderCreds {
        access_token: creds.access_token.clone(),
        kind: "client".into(),
        ..Default::default()
    };
    let (info_uid, nickname) = qoder_common::fetch_userinfo(&agent, &probe);
    let tier = qoder_common::fetch_plan(&agent, &probe).0.unwrap_or_default();
    let uid = match info_uid.filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => uid.to_string(),
    };
    creds.uid = uid.clone();
    creds.nickname = nickname.clone().unwrap_or_default();

    let display = if let Some(n) = nickname.filter(|n| !n.is_empty()) {
        n
    } else if !uid.is_empty() {
        uid.chars().take(12).collect()
    } else {
        format!("Qoder {}", &id[3..9])
    };

    // 先持锁入池拿到最终 id，再写 token store（I10：避免先写凭证后改 id 的孤儿记录）
    let final_id = with_pool_mut(state, |accounts| {
        if let Some(a) = accounts
            .iter_mut()
            .find(|a| a.id == id || (!uid.is_empty() && a.uid == uid))
        {
            // 原位更新并保留原 id
            if a.nickname.is_empty() {
                a.nickname = display.clone();
            }
            if !tier.is_empty() {
                a.plan = tier;
            }
            // P2 审查修复：credential_source 保守更新——仅字段为空，或凭证本体实际
            // 变化（按 uid 命中且新令牌派生 id 与原 id 不同 = 换发）时才更新，
            // 防同账号多通道导入时徽标随「最后导入者」漂移
            if a.credential_source.is_empty() || a.id != id {
                a.credential_source = "client".into();
            }
            // uid 空时不清空既有绑定（userinfo 失败且设备流未返回 uid 的降级场景）
            if !uid.is_empty() {
                a.uid = uid.clone();
            }
            a.needs_relogin = false;
            a.relogin_reason = String::new();
            // 指纹回填（幂等：已有稳定绑定不覆盖，§5.10）
            if a.device_profile.is_none() {
                a.device_profile = Some(crate::tasks::qoder_device::QoderDeviceProfile::generate());
            }
            Ok(a.id.clone())
        } else {
            accounts.push(QoderAccount {
                id: id.clone(),
                uid: uid.clone(),
                nickname: display.clone(),
                plan: tier,
                credential_source: "client".into(),
                // 入池即生成稳定指纹（§5.10：一次生成永不轮换）
                device_profile: Some(crate::tasks::qoder_device::QoderDeviceProfile::generate()),
                ..Default::default()
            });
            Ok(id.clone())
        }
    })?;
    qoder_common::save_token_store(state, &final_id, &creds)?;
    Ok((final_id, display))
}
