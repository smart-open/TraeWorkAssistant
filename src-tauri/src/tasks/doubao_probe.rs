//! 豆包 probe 自动保活（方案 A，2026-11-06）：
//! 签到前 campaign_probe 缺失/过期时，自动完成「确保代理运行 → 以账号打开豆包
//! （`--proxy-server` 注入）→ 等客户端首页自动发出的 campaign 请求被抓包入库 →
//! 继续签到」闭环，替代人工「开代理+客户端签到对齐」。
//!
//! 实测依据（2026-11-06 09:59 抓包日志）：客户端打开后 ~16s 自动发出
//! landing/report、landing/get、v3/detail 请求（首页渲染连签状态），无需点击签到按钮；
//! 抓包器命中 `/landing/checkin` 或 `/v3/detail/` 且上游 code==0 时参数入库。
//!
//! 设计要点：
//! - probe 按 sessionid 分键（设备指纹与登录会话绑定），因此保活以 Switch 动作
//!   逐账号切换客户端登录态抓取各自参数；已新鲜的账号跳过。
//! - 代理复用 commands::proxy 的静态句柄：未运行则启动（快照/还原系统代理语义
//!   与手动启停一致），全部账号处理完后若为本次自启则停止还原。
//! - 副作用声明：会短暂关闭/切换豆包客户端登录态（结束停留在最后处理的账号），
//!   与「一键以账号打开」既有语义一致，全程 app_log 留痕。
//! - 红线：仅抓取本工具用户自己的客户端请求参数用于对齐签到，凭证等同密码仅本地使用。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::AppHandle;

use crate::fs_utils;
use crate::state::AppState;
use crate::switcher::{run_action, Action, ProgressSink, RunArgs, StepStatus, TargetApp};

use super::doubao_checkin::{probe_key_for_sid, probe_own_fresh};

/// 进程内保活闸门：手动签到与调度签到并发时只跑一轮保活（WouldBlock 直接跳过）
static KEEPALIVE_GATE: Mutex<()> = Mutex::new(());

/// 单账号等待抓包超时（实测首条 campaign 请求 ~16s，留 3 倍余量）
const WAIT_CAPTURE_SECS: u64 = 75;
/// 轮询间隔
const POLL_INTERVAL_MS: u64 = 3000;

/// app_log 步进 Sink（run_action 进度全部落日志，无 UI 事件依赖）
struct LogSink {
    data_dir: PathBuf,
}

impl LogSink {
    fn new(data_dir: &std::path::Path) -> Self {
        LogSink { data_dir: data_dir.to_path_buf() }
    }
}

impl ProgressSink for LogSink {
    fn step(&self, stage: &str, status: StepStatus, message: &str) {
        fs_utils::app_log(
            &self.data_dir,
            &format!("[probe-keepalive] {stage}/{}: {message}", status.as_str()),
        );
    }
}

/// 读取 vault probe 的 captured_at（新旧判据；键不存在/损坏返回 None）
fn probe_captured_at(data_dir: &std::path::Path, key: &str) -> Option<String> {
    crate::vault::ns_get(data_dir, "doubao", key)
        .and_then(|v| v.get("captured_at").and_then(|t| t.as_str().map(str::to_string)))
}

/// 保活前该账号是否已有新鲜 probe（**仅看本账号自己的键**）：
/// 全局 probe 可能属于其他账号（设备指纹-账号绑定，跨用必被 710010702 拒），
/// 若把它当「已新鲜」保活会永久跳过——2026-11-06 12:1x 实测踩坑后改为严格判定
fn probe_already_fresh(data_dir: &std::path::Path, sid: &str) -> bool {
    probe_own_fresh(data_dir, sid)
}

/// 单账号保活：Switch 注入代理打开客户端 → 轮询等待该 sessionid 的 probe 入库。
/// 返回 Ok(true)=抓到 / Ok(false)=超时未抓到（不视为保活整体失败，继续后续账号）
fn keepalive_one(
    state: &AppState,
    uid: &str,
    sid: &str,
    name: &str,
    proxy_port: u16,
) -> Result<bool, String> {
    let before_sid = probe_captured_at(&state.data_dir, &probe_key_for_sid(sid));
    let before_global = probe_captured_at(&state.data_dir, "campaign_probe");

    let args = RunArgs {
        action: Action::Switch,
        target_app: TargetApp::Doubao,
        user_id: Some(uid.to_string()),
        proxy_port: Some(proxy_port),
        include_indexeddb: state.settings().doubao_snapshot_include_idb,
        // expected_current_uid 留空 = 不启用严格防误覆盖守卫（自动化场景无人工确认；
        // switcher 内置的 current_account.txt 基础守卫仍生效）
        expected_current_uid: String::new(),
        machine_id_override: None,
        data_dir: state.data_dir.clone(),
    };
    let sink = LogSink::new(&state.data_dir);
    fs_utils::app_log(
        &state.data_dir,
        &format!("[probe-keepalive] 以账号打开豆包抓取参数: uid={uid} name={name} 代理端口={proxy_port}"),
    );
    run_action(args, &sink).map_err(|e| {
        fs_utils::app_log(&state.data_dir, &format!("[probe-keepalive] Switch 失败 uid={uid}: {e}"));
        e
    })?;

    // 轮询等待抓包入库（per-sid 键出现/变化，或全局键变化——双键任一命中即可）
    let deadline = Instant::now() + Duration::from_secs(WAIT_CAPTURE_SECS);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        let now_sid = probe_captured_at(&state.data_dir, &probe_key_for_sid(sid));
        if now_sid.is_some() && now_sid != before_sid {
            fs_utils::app_log(
                &state.data_dir,
                &format!("[probe-keepalive] probe 已入库（本账号键）: uid={uid} name={name}"),
            );
            return Ok(true);
        }
        let now_global = probe_captured_at(&state.data_dir, "campaign_probe");
        if now_global.is_some() && now_global != before_global {
            // 全局键变化：客户端请求已被抓包（ Capture 侧同时会写 per-sid 键，
            // 此处命中全局变化通常意味着 per-sid 也已写入，下一轮循环会确认）
            if probe_captured_at(&state.data_dir, &probe_key_for_sid(sid)) == now_global {
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("[probe-keepalive] probe 已入库（本账号键，经全局变化确认）: uid={uid} name={name}"),
                );
                return Ok(true);
            }
        }
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!("[probe-keepalive] 等待抓包超时（{WAIT_CAPTURE_SECS}s）: uid={uid} name={name}"),
    );
    Ok(false)
}

/// 签到前 probe 保活入口（run_batch / checkin_single 共用）：
/// 开关关闭 / 闸门占用 / 全部已新鲜 时静默返回 Ok；逐账号 Switch 抓包，
/// 任一账号抓到即视为成功（其余账号失败信息随返回值记日志）。
/// 返回 Err 仅表示「保活未能进行」（开关关/代理启动失败等），调用方记日志后照常
/// 用既有 probe 链签到（缺失时回退内置稳定参数）。
pub(crate) fn ensure_fresh_probes(
    state: &AppState,
    app: Option<&AppHandle>,
    targets: &[(String, String, Option<String>, String)],
) -> Result<(), String> {
    if targets.is_empty() {
        return Ok(());
    }
    if !state.settings().doubao_probe_auto_keepalive {
        return Ok(());
    }
    let missing: Vec<&(String, String, Option<String>, String)> = targets
        .iter()
        .filter(|(_, sid, _, _)| !probe_already_fresh(&state.data_dir, sid))
        .collect();
    if missing.is_empty() {
        fs_utils::app_log(
            &state.data_dir,
            "[probe-keepalive] 全部账号均持有本账号新鲜 probe，跳过保活",
        );
        return Ok(());
    }

    let Ok(_gate) = KEEPALIVE_GATE.try_lock() else {
        fs_utils::app_log(&state.data_dir, "[probe-keepalive] 另一轮保活进行中，跳过");
        return Ok(());
    };

    // ① 代理：未运行则启动（AppHandle 缺失且代理未运行 → 无法自启，保活放弃）
    let started_here = if crate::commands::proxy::proxy_is_running() {
        false
    } else {
        let Some(app) = app else {
            fs_utils::app_log(
                &state.data_dir,
                "[probe-keepalive] probe 缺失但代理未运行且无 AppHandle（CLI 模式），放弃保活",
            );
            return Err("代理未运行且无法自动启动（CLI 任务模式）".to_string());
        };
        let port = state.settings().proxy_port;
        fs_utils::app_log(&state.data_dir, &format!("[probe-keepalive] 代理未运行，自动启动: port={port}"));
        crate::commands::proxy::start_proxy_blocking(app, state, port)
            .map_err(|e| {
                let msg = format!("[probe-keepalive] 代理自动启动失败: {e}");
                fs_utils::app_log(&state.data_dir, &msg);
                e
            })?;
        true
    };

    // ② 逐账号 Switch 抓包；③ 全部结束后若代理为本次自启则停止还原
    let result = (|| -> Result<(), String> {
        let mut captured = 0usize;
        for (uid, sid, _, name) in &missing {
            match keepalive_one(state, uid, sid, name, state.settings().proxy_port) {
                Ok(true) => captured += 1,
                Ok(false) => {}
                Err(e) => {
                    fs_utils::app_log(
                        &state.data_dir,
                        &format!("[probe-keepalive] 账号 {name} 保活失败（跳过继续）: {e}"),
                    );
                }
            }
        }
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "[probe-keepalive] 保活完成: 需抓 {} · 成功 {} · 客户端停留在最后处理的账号",
                missing.len(),
                captured
            ),
        );
        if captured == 0 {
            return Err("所有账号均未抓到新 probe（客户端未发出 campaign 请求或上游拒绝）".to_string());
        }
        Ok(())
    })();

    if started_here {
        match app {
            Some(app) => {
                fs_utils::app_log(&state.data_dir, "[probe-keepalive] 停止本轮自启代理并还原系统代理");
                if let Err(e) = crate::commands::proxy::stop_proxy_blocking(state, Some(app)) {
                    fs_utils::app_log(
                        &state.data_dir,
                        &format!("[probe-keepalive] 代理停止/还原失败（可手动关闭）: {e}"),
                    );
                }
            }
            None => fs_utils::app_log(
                &state.data_dir,
                "[probe-keepalive] 无 AppHandle，跳过代理停止还原（保持运行）",
            ),
        }
    }
    result
}
