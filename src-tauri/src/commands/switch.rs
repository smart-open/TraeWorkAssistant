//! 登录态切换/保存/设备重置命令（switcher 模块的 Tauri 命令封装）。
//! 原实现经 powershell 子进程管道消费 NDJSON；Rust 化后收敛为
//! `switcher::run_action` 进程内直调 + `TauriSink` 一步到位 emit 事件，
//! 终态 `*-done {success, raw}` 由命令层依据返回值发射（前端契约不变）。

use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::switcher::{Action, RunArgs, TauriSink, TargetApp};

use super::workbuddy::BuddyApp;

/// switch-progress 事件单行 NDJSON（与 switcher::step_line 字段序/语义一致）
fn emit_switch_step(app: &AppHandle, stage: &str, status: &str, message: &str) {
    let line = serde_json::json!({
        "stage": stage,
        "status": status,
        "message": message,
        "time": fs_utils::now_ts(),
    })
    .to_string();
    let _ = app.emit("switch-progress", line);
}

/// F-74：判定 Buddy（WorkBuddy/CodeBuddy）当前的登录账号 id（会话迁移的源账号）。
/// WorkBuddy 由共享 auth 文件驱动 → auth 信号优先；CodeBuddy 由自身 vscdb 驱动
/// （auth 文件可能被 WorkBuddy 覆盖）→ 桥标记优先（F2-2 标记语义）。
fn buddy_current_account_id(state: &AppState, app: BuddyApp) -> Option<String> {
    let by_auth = crate::commands::workbuddy::pool_account_id_by_auth_uid(state);
    let by_marker = crate::commands::workbuddy::current_account_marker(state, app.label());
    match app {
        BuddyApp::WorkBuddy => by_auth.or(by_marker),
        BuddyApp::CodeBuddy => by_marker.or(by_auth),
    }
}

/// F-74：目标账号快照槽存在性（与 switch_flow 预检同条件：主槽或 .bak 回退槽）。
/// 迁移前置作业会先关闭客户端（backup_chats 内 graceful_kill_app），而桥的
/// 「目标账号无快照」预检失败路径在 stop_app 之前返回、无 start_app——目标无
/// 快照时必须跳过迁移，否则客户端被杀后不再重启。
fn buddy_target_slot_exists(data_dir: &std::path::Path, app: BuddyApp, uid: &str) -> bool {
    let target = match app {
        BuddyApp::CodeBuddy => TargetApp::CodeBuddy,
        BuddyApp::WorkBuddy => TargetApp::WorkBuddy,
    };
    target_slot_exists(data_dir, target, uid)
}

/// 快照槽存在性（与 switcher::run_action 内部预检同条件：主槽或 .bak 回退槽任一存在）。
/// switch_account 命令层用它在进入后台流程**之前**同步拦截——无快照时立即 Err 引导文案，
/// 不发 progress 事件、不触发前端 90s 看门狗、不启动会话迁移等前置作业。
fn target_slot_exists(data_dir: &std::path::Path, target: TargetApp, uid: &str) -> bool {
    let profiles_dir = crate::switcher::profile::profile_for(target, data_dir).profiles_dir;
    profiles_dir.join(uid).exists() || profiles_dir.join(format!("{uid}.bak")).exists()
}

/// 构造切/存/恢复类命令的通用入参
fn build_args(
    action: Action,
    target_app: Option<&str>,
    user_id: Option<String>,
    proxy_port: Option<u16>,
    include_indexeddb: bool,
    expected_current_uid: String,
    data_dir: std::path::PathBuf,
) -> RunArgs {
    RunArgs {
        action,
        target_app: TargetApp::parse(target_app.unwrap_or("TraeWork")),
        user_id,
        proxy_port: proxy_port.filter(|p| *p > 0),
        include_indexeddb,
        expected_current_uid,
        data_dir,
    }
}

/// 后台执行 run_action 并发射终态事件；成功后可选回调（dc id 回填等，携带 data_dir）。
/// `pre`：run_action 之前执行的前置作业（F-74 会话迁移；其进度自行 emit，不走 sink）
fn run_in_background(
    app: AppHandle,
    event_progress: &'static str,
    event_done: &'static str,
    args: RunArgs,
    pre: Option<Box<dyn FnOnce(&AppHandle) + Send>>,
    on_success: Option<Box<dyn FnOnce(&AppHandle, &std::path::Path) + Send>>,
) {
    std::thread::spawn(move || {
        let data_dir = args.data_dir.clone();
        if let Some(pre) = pre {
            pre(&app);
        }
        let sink = TauriSink::new(&app, event_progress, &data_dir);
        let result = crate::switcher::run_action(args, &sink);
        let (success, raw) = match &result {
            Ok(line) => (true, line.clone()),
            Err(line) => (false, line.clone()),
        };
        let _ = app.emit(event_done, serde_json::json!({ "success": success, "raw": raw }));
        if success {
            if let Some(cb) = on_success {
                cb(&app, &data_dir);
            }
        }
    });
}

// async：内含会话/JWT 预检子进程（网络 I/O）与守卫 uid 检测子进程，同步命令会冻结 UI
//（项目约定：阻塞型命令一律 #[tauri::command(async)]）
#[tauri::command(async)]
pub fn switch_account(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
    proxy_port: Option<u16>,
    // 续期 JWT 流程专用：目标账号 JWT 本就可能已吊销（续期正是为了重抓），
    // 跳过 TRAE 切换前 JWT 预检，否则预检 401 会把续期链路拦死
    skip_jwt_probe: Option<bool>,
) -> Result<(), String> {
    // 审查修复（入参校验/路径遍历）：user_id 会拼进豆包探测槽路径（probe_slot_session_alive）
    // 与快照槽路径，与其余 uid 入口（profile.rs/doubao.rs 等）统一过白名单
    fs_utils::ensure_uid_safe(user_id.trim())?;
    fs_utils::app_log(&state.data_dir, &format!("开始切换账号: user_id={user_id}"));

    // 无快照提前拦截（P1-3 用户反馈）：目标应用域从未保存过登录态时，切换必然失败。
    // 同步 Err 直接给前端 toast 引导（「切换失败：」前缀由前端拼接，文案避免重复），
    // 不进后台流程、不触发 90s 看门狗、不启动会话迁移等前置作业；run_action 内部预检保留作兜底。
    let preflight_target = TargetApp::parse(target_app.as_deref().unwrap_or("TraeWork"));
    if !target_slot_exists(&state.data_dir, preflight_target, user_id.trim()) {
        return Err(
            "还没有该账号的登录态快照：请先用此账号登录客户端，然后到「账号管理」点击「保存当前登录态」，成功后即可一键切换".into(),
        );
    }

    // C4：豆包快照可选纳入 IndexedDB（设置开关控制，其他应用不受影响）
    let is_doubao = target_app.as_deref() == Some("Doubao");
    // JWT 预检仅 TRAE 双应用（TraeWork/Trae，含默认）：WorkBuddy/CodeBuddy 会话模型不同，
    // 且其 uid 与 TRAE 账号池撞库时会被误探活错误拦截——非 trae 一律放行（CodeBuddy 同 WorkBuddy）
    let is_trae = matches!(target_app.as_deref(), None | Some("TraeWork") | Some("Trae"));
    let include_idb = is_doubao && state.settings().doubao_snapshot_include_idb;
    // 切换前服务端会话预检（仅豆包）：目标槽位快照里的会话若已被服务端吊销——常见于
    // 在豆包客户端内退出登录/重登该账号（passport logout 吊销旧会话，快照文件却完好）——
    // 恢复后客户端一联网即被 SESSION_EXPIRED 强制登出，表现为「切换成功但豆包未登录」。
    // 实测不对称现象根因：2026-09-09 A 槽探测 code=710012001（expired）、B 槽 code=0（ok）。
    // 提前拦截给出补救指引，避免白切一场；探测不可用 fail-open 不阻断（见函数内实现）。
    if is_doubao {
        crate::commands::doubao::probe_slot_session_alive(&state.data_dir, &user_id)?;
    } else if is_trae && !skip_jwt_probe.unwrap_or(false) {
        // TRAE（TraeWork/Trae）：切换前 JWT 服务端预检（issue #9）——目标账号 JWT 被服务端
        // 吊销时本地快照仍完好，切换恢复后 IDE 一联网即被登出，用户感知为「切换了但没反应」。
        // 预检 Err 仅在「判死」时产生（网络故障已在函数内 fail-open 为 Ok）。
        // 此前判死会硬拒绝切换——但签到 401 SessionDead 的账号预检必判死，而「切回该账号
        // 重新登录」正是唯一恢复手段，硬拒绝形成死结（用户反馈：签到失败的账号点切换无反应）。
        // 现改为：放行切换，把失效警示 + 恢复指引写入切换进度流（fail-open，不阻断）。
        if let Err(dead_msg) = crate::commands::accounts::probe_trae_jwt_alive(&state, &user_id) {
            fs_utils::app_log(&state.data_dir, &format!("切换前 JWT 预检判死（已放行）: {dead_msg}"));
            let warn_line = serde_json::json!({
                "stage": "probe",
                "status": "warn",
                "message": dead_msg,
            })
            .to_string();
            let _ = app.emit("switch-progress", &warn_line);
        }
    }

    // 防误覆盖守卫：把关闭客户端前检测到的当前登录 uid 传入 switcher，仅在它与
    // current_account.txt 一致时才把"当前态"回写进该账号槽。豆包走严格版（还要求
    // Live Cookies 里验证到登录会话——uid 检测可能被快照 localStorage 残留骗过，
    // Cookie 存在性无法伪造）；icube 布局（TraeWork/Trae）走本机使用证据推导（见下）
    let is_buddy = matches!(target_app.as_deref(), Some("WorkBuddy") | Some("CodeBuddy"));
    let buddy_app = match target_app.as_deref() {
        Some("CodeBuddy") => BuddyApp::CodeBuddy,
        _ => BuddyApp::WorkBuddy,
    };
    let expected_uid = if is_doubao {
        crate::commands::doubao::detect_guard_uid_strict(&state)
    } else if is_trae {
        // icube 布局（TraeWork/Trae）切换守卫：混合推导（本机使用证据 + 桥标记切换
        // 时刻，F2-6）——纯证据推导会被快照冻结的旧时间戳误导（实测指向一个月前的
        // 历史账号，守卫恒误判不一致而跳过回写）。推导失败（None）→ 维持空串
        // fail-open 不阻断切换。switch_account 为 async 命令，vscdb/storage 同步读取
        // 在工作线程执行，不冻结 UI
        let kind = target_app.as_deref().unwrap_or("TraeWork");
        crate::commands::trae_apps::current_cloud_uid_hybrid(kind, &state.data_dir)
            .unwrap_or_default()
    } else if is_buddy {
        // F1-3/F2-5 authfile 布局切换守卫：按端取「当前登录账号 id」——
        // WorkBuddy 由共享 auth 文件驱动 → auth 文件 uid 在池反查优先，桥标记兜底；
        // CodeBuddy 登录真源信号 = live storage.json genie.userId（共享 auth 文件属
        // WorkBuddy，会被其覆盖，不能作为 CodeBuddy 登录证据；桥标记只反映上次切换
        // 目标，客户端手动重登后失真）→ genie 实测优先，桥标记兜底。
        // 取不到 → 空串 fail-open 不阻断
        match buddy_app {
            BuddyApp::CodeBuddy => crate::commands::workbuddy::codebuddy_live_uid()
                .and_then(|uid| crate::commands::workbuddy::pool_account_id_by_uid(&state, &uid))
                .or_else(|| {
                    crate::commands::workbuddy::current_account_marker(&state, "CodeBuddy")
                })
                .unwrap_or_default(),
            BuddyApp::WorkBuddy => {
                buddy_current_account_id(&state, buddy_app).unwrap_or_default()
            }
        }
    } else {
        String::new()
    };

    // F-74：Buddy 切换前自动迁移会话（设置项 buddy_switch_migrate_chats，默认关）。
    // 迁移必须在桥的 Stop→Restore→Start 窗口之前完成全部 db/文件动作：先备份当前账号
    // 三件套，再以新 id 复制到目标账号名下（复制后 live 同时含 A 原件 + B 副本，桥重启
    // 客户端后目标账号登录即可见，源副本残留由下游清理）。
    // （is_buddy / buddy_app 已在防误覆盖守卫处定义，此处直接复用）
    let migrate_job: Option<Box<dyn FnOnce(&AppHandle) + Send>> = if is_buddy
        && state.settings().buddy_switch_migrate_chats
        && buddy_target_slot_exists(&state.data_dir, buddy_app, user_id.trim())
    {
        match buddy_current_account_id(&state, buddy_app) {
            Some(src) if src != user_id && !src.is_empty() => {
                let data_dir = state.data_dir.clone();
                let uid = user_id.clone();
                let label = buddy_app.label();
                fs_utils::app_log(
                    &data_dir,
                    &format!("切换前会话迁移: {label} {src} → {uid}（自动备份 + 复制）"),
                );
                Some(Box::new(move |app: &AppHandle| {
                    emit_switch_step(
                        app,
                        "migrate",
                        "running",
                        &format!("正在迁移 {label} 会话到目标账号（自动备份 + 新 id 复制）"),
                    );
                    match crate::commands::workbuddy::backup_chats(&data_dir, buddy_app, &src).map(|(n, _)| n) {
                        Ok(n) => emit_switch_step(app, "migrate", "ok", &format!("当前账号会话已备份（{n} 个文件）")),
                        Err(e) => {
                            emit_switch_step(
                                app,
                                "migrate",
                                "warn",
                                &format!("会话备份失败（已跳过迁移，切换继续）: {e}"),
                            );
                            return;
                        }
                    }
                    match crate::commands::workbuddy::copy_chats(&data_dir, buddy_app, &src, &uid) {
                        Ok(v) => emit_switch_step(
                            app,
                            "migrate",
                            "ok",
                            &format!(
                                "会话已迁移到目标账号（会话 {}、sessions 克隆 {}、云端映射 {}）",
                                v.get("copied").and_then(|x| x.as_i64()).unwrap_or(0),
                                v.get("sessions_cloned").and_then(|x| x.as_i64()).unwrap_or(0),
                                v.get("mappings_registered").and_then(|x| x.as_i64()).unwrap_or(0),
                            ),
                        ),
                        Err(e) => emit_switch_step(
                            app,
                            "migrate",
                            "warn",
                            &format!("会话迁移失败（不影响登录态切换，可稍后手动「复制会话」）: {e}"),
                        ),
                    }
                }))
            }
            _ => None,
        }
    } else {
        None
    };

    let args = build_args(
        Action::Switch,
        target_app.as_deref(),
        Some(user_id.clone()),
        proxy_port,
        include_idb,
        expected_uid,
        state.data_dir.clone(),
    );
    // 后台线程执行（流程含最长 ~45s 等待：优雅关闭 8s + auth 静默 10s + verify 30s，
    // 不阻塞命令返回；与原 stdout 读线程一致的异步语义）
    let app2 = app.clone();
    let uid_for_dc = user_id.clone();
    let is_doubao2 = is_doubao;
    run_in_background(app2, "switch-progress", "switch-done", args, migrate_job, Some(Box::new(move |_app, dc_dir| {
        // 切换成功后补充该账号的账户中心（icube-dc）id 预留记录（只记录不展示）
        // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
        if !is_doubao2 {
            let _ = crate::commands::trae_apps::backfill_dc_id_for(dc_dir, &uid_for_dc);
        }
    })));

    Ok(())
}

/// 保存当前登录态：关闭客户端 → 精准备份到 userId 槽位 → 重新启动。
/// 进度经 save-login-progress / save-login-done 事件流式返回，前端订阅契约不变。
// async：豆包分支含会话预检子进程（网络 I/O），同步命令会冻结 UI
#[tauri::command(async)]
pub fn save_current_login(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // 审查修复（入参校验）：同 switch_account，uid 白名单校验与其余入口对齐
    fs_utils::ensure_uid_safe(user_id.trim())?;

    // 保存前预检（仅豆包）：Live profile 必须持有登录会话 Cookie。没有 = 客户端当前未登录，
    // 保存只会把未登录状态存进账号槽（实测 908 槽被未登录态覆盖后"切换成功但永远没登录"），
    // 直接拒绝并告知补救方式。客户端此时仍在运行，Cookies 被锁由 Rust 复制到临时目录读取。
    if target_app.as_deref() == Some("Doubao") {
        crate::commands::doubao::ensure_live_has_login_session()?;
        // 服务端会话预检：本地 Cookie 存在≠会话有效。会话可能早已被服务端吊销
        // （客户端内退出过/被新登录顶替），存进去就是死会话，之后每次切换该账号都未登录
        //（实测 A 槽事故：20:43 保存的快照当时已是/随后被吊销的死会话）。expired 拒绝保存。
        crate::commands::doubao::probe_live_session_alive(&user_id)?;
    }

    // F2-5 保存守卫（authfile 布局，WorkBuddy/CodeBuddy）：校验客户端**实际登录**与
    // 目标账号一致，防止把 A 的登录态存进 B 的槽位——实测根源事故：CodeBuddy 槽位
    // 互相污染后（wb-45c 与 wb-98e 内容完全相同），「切换」怎么切都是同一个账号。
    // 登录信号：WorkBuddy = 共享 auth 文件 uid → 池反查；CodeBuddy = live storage.json
    // genie.userId → 池反查。检测不可用（未登录/解析失败/池中无此 uid）→ fail-open
    // 放行，交由备份流程既有兜底（auth 缺失整体跳过等）。
    if matches!(target_app.as_deref(), Some("WorkBuddy") | Some("CodeBuddy")) {
        let live_id = match target_app.as_deref() {
            Some("CodeBuddy") => crate::commands::workbuddy::codebuddy_live_uid()
                .and_then(|uid| crate::commands::workbuddy::pool_account_id_by_uid(&state, &uid)),
            _ => crate::commands::workbuddy::pool_account_id_by_auth_uid(&state),
        };
        if let Some(live) = live_id {
            if !live.is_empty() && live != user_id.trim() {
                let msg = format!(
                    "客户端当前登录的是账号 {live}，与要保存的账号 {user_id} 不一致，已拒绝保存（防止账号槽位被互相覆盖污染）。\
                     请先「切换」到目标账号并在客户端确认登录，再点「保存当前登录态」。"
                );
                fs_utils::app_log(&state.data_dir, &format!("保存登录态被守卫拦截: {msg}"));
                return Err(msg);
            }
        }
    }

    fs_utils::app_log(&state.data_dir, &format!("开始保存当前登录态: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB
    let include_idb = target_app.as_deref() == Some("Doubao")
        && state.settings().doubao_snapshot_include_idb;

    let args = build_args(
        Action::SaveCurrentLogin,
        target_app.as_deref(),
        Some(user_id.clone()),
        None,
        include_idb,
        String::new(),
        state.data_dir.clone(),
    );
    let is_doubao = target_app.as_deref() == Some("Doubao");
    let app2 = app.clone();
    let uid_for_dc = user_id.clone();
    run_in_background(app2, "save-login-progress", "save-login-done", args, None, Some(Box::new(move |_app, dc_dir| {
        // 保存登录态成功后同样补充 dc id 预留记录（快照刚生成，来源最可靠）
        // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
        if !is_doubao {
            let _ = crate::commands::trae_apps::backfill_dc_id_for(dc_dir, &uid_for_dc);
        }
    })));

    Ok(())
}

/// 6 层设备标识重置（switcher ResetDeviceIds 动作）
/// 进度经 device-reset-progress / device-reset-done 事件流式返回，前端订阅契约不变。
/// target_app：TraeWork（默认，TRAE SOLO CN）/ Trae（Trae CN IDE），决定清理哪个应用的数据目录
#[tauri::command(async)]
pub fn reset_device_ids(
    app: AppHandle,
    state: State<AppState>,
    target_app: Option<String>,
) -> Result<(), String> {
    let target = match target_app.as_deref() {
        Some("Trae") => "Trae",
        _ => "TraeWork",
    };
    fs_utils::app_log(&state.data_dir, "开始 6 层设备标识重置");
    let args = build_args(
        Action::ResetDeviceIds,
        Some(target),
        None,
        None,
        false,
        String::new(),
        state.data_dir.clone(),
    );
    run_in_background(app, "device-reset-progress", "device-reset-done", args, None, None);
    Ok(())
}
