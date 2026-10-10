//! 登录态切换/保存/设备重置命令（switcher 模块的 Tauri 命令封装）。
//! 原实现经 powershell 子进程管道消费 NDJSON；Rust 化后收敛为
//! `switcher::run_action` 进程内直调 + `TauriSink` 一步到位 emit 事件，
//! 终态 `*-done {success, raw}` 由命令层依据返回值发射（前端契约不变）。

use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::switcher::{Action, RunArgs, TauriSink, TargetApp};

use super::workbuddy::BuddyApp;

/// emit 失败兜底（issue #44）：委托 events::emit_logged（失败落日志，不再静默吞掉——
/// 前端收不到 *-done 终态事件时 switchingTo 永久非空，用户感知为「一直显示切换中」）。
fn emit_or_log(app: &AppHandle, event: &str, payload: serde_json::Value, data_dir: &std::path::Path) {
    crate::events::emit_logged(app, event, payload, Some(data_dir));
}

/// switch-progress 事件单行 NDJSON（与 switcher::step_line 字段序/语义一致）
fn emit_switch_step(app: &AppHandle, data_dir: &std::path::Path, stage: &str, status: &str, message: &str) {
    let line = serde_json::json!({
        "stage": stage,
        "status": status,
        "message": message,
        "time": fs_utils::now_ts(),
    })
    .to_string();
    emit_or_log(app, "switch-progress", serde_json::Value::String(line), data_dir);
}

/// 后台线程统一终态出口（issue #44）：run_action panic（如文件操作/数据库异常
/// unwrap）时后台线程直接死亡、*-done 事件永不发射——switchingTo 永久非空，
/// 前端「一直显示切换中」。catch_unwind 捕获后仍发射失败终态；emit 失败落日志。
/// `extra_fields`：附加字段（如 profile-done 的 action），为对象时逐字段并入 payload。
pub(crate) fn finish_action_thread(
    app: &AppHandle,
    event_done: &'static str,
    data_dir: &std::path::Path,
    result: std::thread::Result<Result<String, String>>,
    extra_fields: serde_json::Value,
) {
    let (success, raw) = match result {
        Ok(Ok(line)) => (true, line),
        Ok(Err(line)) => (false, line),
        Err(_) => (
            false,
            "[fatal] 切换流程内部异常终止（后台线程 panic），请重试；若持续复现请携带 logs/switcher.log 反馈".to_string(),
        ),
    };
    let mut payload = serde_json::json!({ "success": success, "raw": raw });
    if let (Some(obj), Some(extra)) = (payload.as_object_mut(), extra_fields.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    // 日志复盘补齐（2026-10-05）：app.log 只有「开始备份/切换」行、结果只在前端
    // toast 与 switcher.log——后台失败（如 mac 灰度拒绝）在 app.log 断链难排查。
    // 统一落终态结果行（事件发射失败也有据可查）
    fs_utils::app_log(
        data_dir,
        &format!("后台动作终态: {event_done} success={success} raw={raw}"),
    );
    emit_or_log(app, event_done, payload, data_dir);
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
#[allow(clippy::too_many_arguments)]
fn build_args(
    action: Action,
    target_app: Option<&str>,
    user_id: Option<String>,
    proxy_port: Option<u16>,
    include_indexeddb: bool,
    expected_current_uid: String,
    machine_id_override: Option<String>,
    data_dir: std::path::PathBuf,
) -> RunArgs {
    RunArgs {
        action,
        target_app: TargetApp::parse(target_app.unwrap_or("TraeWork")),
        user_id,
        proxy_port: proxy_port.filter(|p| *p > 0),
        include_indexeddb,
        expected_current_uid,
        machine_id_override,
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
        // issue #44：catch_unwind 包裹全部前置作业与主流程——任何 panic 都保证
        // *-done 终态事件仍被发射，前端不会永久停留在「切换中」
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(pre) = pre {
                pre(&app);
            }
            let sink = TauriSink::new(&app, event_progress, &data_dir);
            crate::switcher::run_action(args, &sink)
        }));
        let ok = matches!(&result, Ok(Ok(_)));
        finish_action_thread(&app, event_done, &data_dir, result, serde_json::Value::Null);
        if ok {
            if let Some(cb) = on_success {
                // 回调自身 panic 不影响已发出的终态事件，仅记录
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(&app, &data_dir);
                }));
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
    // F-80 §5.10.2：Qoder 切号需携带账号绑定 machine_id 做本地存储指纹覆写
    let is_qoder = target_app.as_deref() == Some("Qoder");
    let is_qoder_work = target_app.as_deref() == Some("QoderWork");
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
            emit_or_log(&app, "switch-progress", serde_json::Value::String(warn_line), &state.data_dir);
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
        // icube 布局（TraeWork/Trae）切换守卫（issue #55 审查修复）：live_cloud_uid
        // 日志探测优先（当前会话 dynamicConfig.log uid 是「现在登录的是谁」的直接
        // 证据），hybrid 仅在日志不可用时回退——回写来源槽的判定不再被桥标记/冻结
        // 证据带偏。推导失败（None）→ 维持空串 fail-open 不阻断切换
        let kind = target_app.as_deref().unwrap_or("TraeWork");
        crate::commands::trae_apps::live_cloud_uid(kind, &state.data_dir).unwrap_or_default()
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
    } else if is_qoder {
        // F-80 §5.10 切换守卫：Qoder（icube 布局）当前登录真源 = IDE state.vscdb
        // secret://userInfo（DPAPI 解密，与 ide_store 扫描同链路，仅 Windows）→
        // uid 在池反查账号 id。uid 在池外时原样返回（守卫消息如实提示「与标记
        // 账号不一致」，且 uid 与 qd- 池 id 无碰撞）；未登录/解密失败 → 空串
        // fail-open（仅跳过回写，不阻断切换）。此前 Qoder 落入空串兜底：守卫
        // 每次误报「未识别登录会话」且来源账号槽永不回写（合并审查修复）。
        // 2026-10-02 审查：逻辑收编为 commands::qoder::live_account_id，与
        // profile_backup 保存守卫预探测共用同一实现
        crate::commands::qoder::live_account_id(&state).unwrap_or_default()
    } else if is_qoder_work {
        // 2026-10-02 审查补齐：Qoder Work 切换守卫数据源 = 客户端 Cookies qoderuid
        // cookie 解密（uuid 与池 uid 同源）→ 池反查账号 id；Cookie 缺失/解密失败 →
        // 空串 fail-open（仅跳过回写，不阻断切换）。非空时 switch_flow 守卫可正常
        // 把当前登录态回写到来源账号槽（此前 Work 恒空串，来源槽永不回写）
        crate::commands::qoder::live_work_account_id(&state).unwrap_or_default()
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
                        &data_dir,
                        "migrate",
                        "running",
                        &format!("正在迁移 {label} 会话到目标账号（自动备份 + 新 id 复制）"),
                    );
                    match crate::commands::workbuddy::backup_chats(&data_dir, buddy_app, &src).map(|(n, _)| n) {
                        Ok(n) => emit_switch_step(app, &data_dir, "migrate", "ok", &format!("当前账号会话已备份（{n} 个文件）")),
                        Err(e) => {
                            emit_switch_step(
                                app,
                                &data_dir,
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
                            &data_dir,
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
                            &data_dir,
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

    // F-80 §5.10.2：Qoder 切号取账号绑定 machine_id（池内 device_profile；无档案 → None 跳过覆写）
    let machine_id = if is_qoder {
        crate::commands::qoder::machine_id_of(&state, user_id.trim())
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
        machine_id,
        state.data_dir.clone(),
    );
    // 后台线程执行（流程含最长 ~45s 等待：优雅关闭 8s + auth 静默 10s + verify 30s，
    // 不阻塞命令返回；与原 stdout 读线程一致的异步语义）
    let app2 = app.clone();
    let uid_for_dc = user_id.clone();
    let is_doubao2 = is_doubao;
    let is_qoder2 = is_qoder;
    let is_qoder_work2 = is_qoder_work;
    run_in_background(app2, "switch-progress", "switch-done", args, migrate_job, Some(Box::new(move |_app, dc_dir| {
        // 切换成功后补充该账号的账户中心（icube-dc）id 预留记录（只记录不展示）
        // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过；
        // Qoder/QoderWork 无 icube-dc 通道（qoder_uid 另行回填），同样跳过
        if !is_doubao2 && !is_qoder2 && !is_qoder_work2 {
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
                // issue #55 同型修复（issue #78 复审补齐）：拒绝文案按槽位状态分流
                // （无快照首存→指引客户端手动登录；槽位污染→指引退出重登；其余→
                // 通用文案），替换原一刀切「先切换」对首存场景不可执行的对仗文案
                let profiles_dir = crate::switcher::profile::profile_for(
                    TargetApp::parse(target_app.as_deref().unwrap_or("WorkBuddy")),
                    &state.data_dir,
                )
                .profiles_dir;
                let msg = crate::switcher::save_reject_message(&profiles_dir, user_id.trim(), &live);
                fs_utils::app_log(&state.data_dir, &format!("保存登录态被守卫拦截: {msg}"));
                return Err(msg);
            }
        }
    }

    // L1 保存守卫（icube 布局，TraeWork/Trae）：与 F2-5 同型。数据源改为
    // live_cloud_uid（issue #55 审查修复：客户端日志探测优先，hybrid 回退）——
    // hybrid 在手动重登后会被桥标记/快照冻结旧证据带偏，误拦合法保存，且拦截发生在
    // 数据源更可靠的 L2 日志校验之前，曾造成「OAuth 新账号无法建立首个快照」死锁
    // （切换要快照 → 快照要保存 → 保存被误拒）。与 L2 构成双层防护：L1 命令层
    // fail-fast（客户端尚未被关停，体验最好），L2 兜底防 L1 数据源失真后误放行。
    // 检测不可用（None，如未登录/无日志无证据无标记）→ fail-open 放行。
    if matches!(target_app.as_deref(), None | Some("TraeWork") | Some("Trae")) {
        let kind = target_app.as_deref().unwrap_or("TraeWork");
        if let Some(live) = crate::commands::trae_apps::live_cloud_uid(kind, &state.data_dir) {
            if !live.is_empty() && live != user_id.trim() {
                // issue #55 审查修复：拒绝文案按槽位状态分流（首存/污染/通用）
                let profiles_dir = crate::switcher::profile::profile_for(
                    TargetApp::parse(kind),
                    &state.data_dir,
                )
                .profiles_dir;
                let msg = crate::switcher::save_reject_message(&profiles_dir, user_id.trim(), &live);
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
        None,
        state.data_dir.clone(),
    );
    let is_doubao = target_app.as_deref() == Some("Doubao");
    let is_qoder = target_app.as_deref() == Some("Qoder");
    let is_qoder_work = target_app.as_deref() == Some("QoderWork");
    let app2 = app.clone();
    let uid_for_dc = user_id.clone();
    run_in_background(app2, "save-login-progress", "save-login-done", args, None, Some(Box::new(move |_app, dc_dir| {
        // 保存登录态成功后同样补充 dc id 预留记录（快照刚生成，来源最可靠）
        // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过；
        // Qoder/QoderWork 无 icube-dc 通道，同样跳过
        if !is_doubao && !is_qoder && !is_qoder_work {
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
        None,
        state.data_dir.clone(),
    );
    run_in_background(app, "device-reset-progress", "device-reset-done", args, None, None);
    Ok(())
}
