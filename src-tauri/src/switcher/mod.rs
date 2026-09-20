//! 登录态切换器（原 trae-switch-bridge.ps1 全量 Rust 化入口）。
//!
//! 架构变更（vs PS 桥）：进度从「powershell 子进程 stdout NDJSON 管道」收敛为
//! 进程内 [`ProgressSink`] 回调——消除 GBK/OEM 乱码根因、CREATE_NO_WINDOW、
//! stderr 防死锁线程、done 探测兜底与 ~300-800ms 进程冷启动。
//!
//! 红线对齐：
//! - 前端零改动：NDJSON 行 `{stage,status,message,time}` 与 `*-done {success,raw}`
//!   语义逐字段兼容，全部 stage 消息文案逐字保留；
//! - 快照数据零迁移：profiles*/<slot>{,.bak} 结构、current_account.txt、
//!   meta.json、snapshot_meta.json 格式不变，新旧版本快照互认；
//! - 日志落点不变：`<data_dir>/logs/switcher.log` 追加格式
//!   `[yyyy-MM-dd HH:mm:ss] [stage] message`。

pub mod authfile;
pub mod chromium;
pub mod copy;
pub mod icube;
pub mod locate;
pub mod machine;
pub mod proc;
pub mod profile;
pub mod vscdb;

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Emitter};

use crate::fs_utils;

use self::profile::{AppProfile, Layout};

// ── 入参类型 ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Switch,
    SaveCurrentLogin,
    BackupCurrent,
    RestoreOnly,
    /// PS 桥对等动作保留（仅重置 HKLM MachineGuid）；当前前端无入口，
    /// 供 CLI/后续接入使用——分支逻辑完整可执行
    #[allow(dead_code)]
    ResetMachineId,
    ResetDeviceIds,
    KeepAlive,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Switch => "Switch",
            Action::SaveCurrentLogin => "SaveCurrentLogin",
            Action::BackupCurrent => "BackupCurrent",
            Action::RestoreOnly => "RestoreOnly",
            Action::ResetMachineId => "ResetMachineId",
            Action::ResetDeviceIds => "ResetDeviceIds",
            Action::KeepAlive => "KeepAlive",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetApp {
    TraeWork,
    Trae,
    Doubao,
    WorkBuddy,
    CodeBuddy,
}

impl TargetApp {
    /// PS ValidateSet 对译：未知值回退 TraeWork（调用方为自家前端恒传合法值，
    /// 回退比硬失败更稳；PS ValidateSet 直接拒绝，此为 8.2 #10 有意差异）
    pub fn parse(s: &str) -> TargetApp {
        match s {
            "Trae" => TargetApp::Trae,
            "Doubao" => TargetApp::Doubao,
            "WorkBuddy" => TargetApp::WorkBuddy,
            "CodeBuddy" => TargetApp::CodeBuddy,
            _ => TargetApp::TraeWork,
        }
    }
}

/// 一次桥动作的完整入参（PS 命令行参数 → 结构体字段一一对应）
pub struct RunArgs {
    pub action: Action,
    pub target_app: TargetApp,
    /// -UserId（Reset*/KeepAlive 可空）
    pub user_id: Option<String>,
    /// -ProxyPort（>0 注入 --proxy-server，C1 一键以账号打开）
    pub proxy_port: Option<u16>,
    /// -IncludeIndexedDB（豆包 C4）
    pub include_indexeddb: bool,
    /// -ExpectedCurrentUid（防误覆盖守卫：桌面端关闭前检测到的当前登录 uid）
    pub expected_current_uid: String,
    /// AIWORKDATA_DIR 等价物（进程内直传；CLI 模式来自 AppState）
    pub data_dir: PathBuf,
}

/// 执行会话：一次 run_action 内共享的可变状态（PS $Script: 作用域 → 显式结构体）
pub struct Session {
    pub prof: AppProfile,
    pub data_dir: PathBuf,
    /// PS $Script:_TraeExeCache（exe 发现第 6 级兜底 + Stop 前缓存）
    pub exe_cache: Option<PathBuf>,
    /// PS $Script:_LastRestoredCount（icube 恢复后校验；每次恢复先置 -1，
    /// 仅 icube 结尾写实际值）
    pub last_restored_count: i64,
    /// PS $Script:LaunchProxyPort（>0 启动注入 --proxy-server）
    pub launch_proxy_port: Option<u16>,
    pub include_indexeddb: bool,
    /// authfile 布局：共享 auth 文件目录与文件（$Script:WbAuthDir/WbAuthFile；
    /// 独立字段便于测试注入临时路径）
    pub auth_dir: PathBuf,
    pub auth_file: PathBuf,
}

impl Session {
    pub fn new(args: &RunArgs) -> Session {
        Session {
            prof: profile::profile_for(args.target_app, &args.data_dir),
            data_dir: args.data_dir.clone(),
            exe_cache: None,
            last_restored_count: -1,
            launch_proxy_port: args.proxy_port,
            include_indexeddb: args.include_indexeddb,
            auth_dir: authfile::wb_auth_dir(),
            auth_file: authfile::wb_auth_file(),
        }
    }
}

/// 测试专用：switcher 各模块的文件操作测试共用一把锁串行执行——
/// Windows 上并行创建/删除含大量文件的临时目录会互相触发文件锁（实测 code 32）
#[cfg(test)]
pub(crate) fn test_io_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

// ── 进度通道 ────────────────────────────────────────────────────────────────

/// 步骤状态（与 PS Write-Step 六值逐一对应）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepStatus {
    Info,
    Ok,
    Warn,
    Error,
    Running,
    Skip,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Info => "info",
            StepStatus::Ok => "ok",
            StepStatus::Warn => "warn",
            StepStatus::Error => "error",
            StepStatus::Running => "running",
            StepStatus::Skip => "skip",
        }
    }
}

/// 进度出口：一行 NDJSON 的三个语义字段 + 落日志
pub trait ProgressSink: Send + Sync {
    fn step(&self, stage: &str, status: StepStatus, message: &str);
}

/// 组装 NDJSON 行（字段序字母化 = serde_json 默认；前端 JSON.parse 与字段序无关）
fn step_line(stage: &str, status: StepStatus, message: &str) -> String {
    serde_json::json!({
        "stage": stage,
        "status": status.as_str(),
        "message": message,
        "time": fs_utils::now_ts(),
    })
    .to_string()
}

/// 追加 switcher.log（格式与 PS Add-Content 逐字符一致；先建父目录——
/// PS 358 显式 New-Item，OpenOptions create 不建父目录，缺失时日志会静默丢失）
fn append_switcher_log(log_file: &Path, stage: &str, message: &str) {
    let _ = std::fs::create_dir_all(log_file.parent().unwrap_or_else(|| Path::new(".")));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_file) {
        use std::io::Write as _;
        let _ = writeln!(f, "[{}] [{}] {}", fs_utils::now_ts(), stage, message);
    }
}

fn log_file_of(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("switcher.log")
}

/// 应用内进度出口：emit 事件（payload 为 NDJSON 行字符串，前端 listen<string>
/// 契约不变）+ 追加 switcher.log
pub struct TauriSink {
    app: AppHandle,
    event: &'static str,
    log_file: PathBuf,
}

impl TauriSink {
    /// event: "switch-progress" / "save-login-progress" / "profile-progress" /
    /// "device-reset-progress" / "keepalive-progress"
    pub fn new(app: &AppHandle, event: &'static str, data_dir: &Path) -> TauriSink {
        TauriSink { app: app.clone(), event, log_file: log_file_of(data_dir) }
    }
}

impl ProgressSink for TauriSink {
    fn step(&self, stage: &str, status: StepStatus, message: &str) {
        let _ = self.app.emit(self.event, step_line(stage, status, message));
        append_switcher_log(&self.log_file, stage, message);
    }
}

/// CLI 任务进度出口：println NDJSON 到 stdout（计划任务日志可查）+ 追加 switcher.log
pub struct CliSink {
    log_file: PathBuf,
}

impl CliSink {
    pub fn new(data_dir: &Path) -> CliSink {
        CliSink { log_file: log_file_of(data_dir) }
    }
}

impl ProgressSink for CliSink {
    fn step(&self, stage: &str, status: StepStatus, message: &str) {
        println!("{}", step_line(stage, status, message));
        append_switcher_log(&self.log_file, stage, message);
    }
}

// ── 通用小工具 ──────────────────────────────────────────────────────────────

/// glob 通配匹配（PS -like 语义：仅 `*` 通配，大小写不敏感——如 *Doubao* 须命中 doubao.lnk）
#[cfg_attr(not(windows), allow(dead_code))] // Windows 六级发现消费；测试跨平台复用
pub fn glob_match_ci(pattern: &str, text: &str) -> bool {
    glob_match_ci_inner(pattern, text)
}

#[cfg_attr(not(windows), allow(dead_code))] // 同上：Windows 生产链专用
fn glob_match_ci_inner(pattern: &str, text: &str) -> bool {
    let p = pattern.to_lowercase();
    let t = text.to_lowercase();
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == t;
    }
    let mut pos = 0usize;
    // 首段必须前缀匹配
    if !t[pos..].starts_with(parts[0]) {
        return false;
    }
    pos += parts[0].len();
    // 中间段按序贪心查找
    for mid in &parts[1..parts.len() - 1] {
        if mid.is_empty() {
            continue;
        }
        match t[pos..].find(mid) {
            Some(i) => pos += i + mid.len(),
            None => return false,
        }
    }
    // 尾段必须后缀匹配
    let last = parts[parts.len() - 1];
    t.len() - pos >= last.len() && t.ends_with(last)
}

/// 规范化路径等值：双侧 canonicalize（Windows 展开短名 PROGRA~1 + 大小写语义；
/// 任一侧 canonicalize 失败（文件已删/占用）回退 lossy 小写字符串比较）。
/// 所有 exe 路径比对必须经此——裸字符串比较会把同一路径判为不同（exe 缓存守卫失效）。
pub fn path_eq(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
    }
}

// ── current_account.txt 读写 ────────────────────────────────────────────────

/// PS Get-CurrentAccount 对译。PS 5.1 Set-Content -Encoding UTF8 写入带 BOM
///（doubao.rs / workbuddy accounts 既有读取同为三段式剥 BOM）。Rust trim() 不剥
/// U+FEFF（非 White_Space 字符），存量文件不显式剥离会读出 "\u{FEFF}uid" →
/// 防误覆盖守卫 cur != uid 恒真 → 首次 Rust 版切换误跳过账号槽回写并弹误导警告。
pub fn get_current_account(sess: &Session) -> Option<String> {
    std::fs::read_to_string(sess.prof.current_account_file())
        .ok()
        .map(|s| {
            s.trim()
                .trim_start_matches('\u{feff}')
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
}

/// PS Set-CurrentAccount 对译（-NoNewline -Encoding UTF8 等价：Rust fs::write 无 BOM 无换行）
pub fn set_current_account(sess: &Session, uid: &str) {
    let f = sess.prof.current_account_file();
    let dir = f.parent().unwrap_or_else(|| Path::new("."));
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&f, uid);
    // F2-6 sidecar（审查修复 2026-09-15）：同步记录切换时刻。背景：切换器恢复快照后，
    // live 数据目录的 per-uid 使用证据是快照冻结时的旧数据，trae_apps 纯证据推导会指向
    // 历史账号；客户端手动重登又会写新证据让标记失真。有了切换时刻，读侧可判别
    // 「标记 vs 证据谁更新」（见 trae_apps::current_cloud_uid_hybrid）。
    // 新增文件不改动既有 current_account.txt 格式，快照/标记零迁移红线不破。
    let meta = dir.join("current_account.meta.json");
    let _ = std::fs::write(&meta, format!("{{\"switchedAtMs\":{}}}", chrono::Utc::now().timestamp_millis()));
}

// ── 终态行构造 ──────────────────────────────────────────────────────────────

/// 输出 done 步骤并返回其 NDJSON 行（供 *-done 事件 raw 字段）
fn done(sink: &dyn ProgressSink, message: &str) -> Result<String, String> {
    sink.step("done", StepStatus::Ok, message);
    Ok(message.to_string())
}

/// PS throw → 顶层 catch 的等价物：补发 fatal 行「失败: {msg}」并返回 Err 载荷。
/// 所有对应 PS throw 的错误点必须经此返回（否则 NDJSON 流缺最后一行 fatal）。
fn thrown(sink: &dyn ProgressSink, msg: &str) -> String {
    sink.step("fatal", StepStatus::Error, &format!("失败: {msg}"));
    msg.to_string()
}

/// PS 直接 exit 1 的等价物：fatal 行已由调用点输出，仅构造 Err 载荷（不补发，防双发）。
fn fatal_line(msg: &str) -> String {
    msg.to_string()
}

// ── 布局分派（PS Backup-CurrentProfile / Restore-Profile 分派头）────────────

fn backup_current(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    match sess.prof.layout {
        Layout::Authfile => authfile::backup_authfile(sess, slot, sink),
        Layout::Chromium => chromium::backup_chromium(sess, slot, sink),
        Layout::Icube => icube::backup_icube(sess, slot, sink),
    }
}

fn restore_profile(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    // 恢复项计数（Switch 恢复后校验用）：每次恢复先重置为 -1；仅 icube 结尾写实际值。
    // 校验处用 -le 0 拦截：icube 下等价于 0 项拷贝=快照空/损坏；-1 作为防御一并拦下。
    sess.last_restored_count = -1;
    // F-68：icube 布局（TraeWork/Trae）恢复前抽出 state.vscdb 的两个全局键
    //（项目列表 / 最近打开），恢复后按条目合并回写——账号分区键零改动。
    // 仅在恢复成功时合并；失败已由布局实现侧处理，合并失败按 warn 处理不阻断。
    let keep_keys = if sess.prof.layout == Layout::Icube {
        let p = icube_vscdb_path(&sess.prof.data_dir);
        let snap = vscdb::snapshot_global_keys(&p);
        if snap.project_folders.is_some() || snap.recent_paths.is_some() {
            Some((p, snap))
        } else {
            None
        }
    } else {
        None
    };
    let r = match sess.prof.layout {
        Layout::Authfile => authfile::restore_authfile(sess, slot, sink),
        Layout::Chromium => chromium::restore_chromium(sess, slot, sink),
        Layout::Icube => icube::restore_icube(sess, slot, sink),
    };
    if r.is_ok() {
        if let Some((p, snap)) = keep_keys {
            match vscdb::merge_global_keys(&p, &snap) {
                Ok(Some(summary)) => sink.step(
                    "restore",
                    StepStatus::Ok,
                    &format!("项目列表/最近打开已跨账号保留（{summary}）"),
                ),
                Ok(None) => {}
                Err(e) => sink.step(
                    "restore",
                    StepStatus::Warn,
                    &format!("项目列表/最近打开保留失败（已回滚，不影响登录态）: {e}"),
                ),
            }
        }
    }
    r
}

/// icube 布局的 state.vscdb 路径（F-68 合并目标）
fn icube_vscdb_path(data_dir: &Path) -> PathBuf {
    data_dir.join("User").join("globalStorage").join("state.vscdb")
}

// ── Action 编排 ─────────────────────────────────────────────────────────────

/// 全局串行互斥：PS 桥允许多实例并发，但并发切换本身有竞态（同一数据目录交错
/// 备份/恢复），前端亦仅允许一个进行中。新增防护：进行中直接拒绝（8.2 #3）。
fn action_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Mutex::new(()))
}

/// 执行一个桥动作。返回 Ok = 最终 done 步骤的消息（供 *-done 事件 raw 字段）；
/// Err = 失败终态消息（fatal 步骤已由内部按原时机输出，调用点不得再补发）。
pub fn run_action(args: RunArgs, sink: &dyn ProgressSink) -> Result<String, String> {
    let _guard = action_gate()
        .try_lock()
        .map_err(|_| "已有切换/备份操作进行中，请稍后再试".to_string())?;

    // ── 入参校验（PS 1402-1411：UserId 必填/格式白名单，双保险与命令层独立）──
    let needs_uid = !matches!(
        args.action,
        Action::ResetMachineId | Action::ResetDeviceIds | Action::KeepAlive
    );
    let uid = args.user_id.clone().unwrap_or_default();
    if needs_uid && uid.trim().is_empty() {
        sink.step("init", StepStatus::Error, "缺少 -UserId 参数");
        return Err(fatal_line("缺少 -UserId 参数"));
    }
    if !uid.is_empty() {
        // PS 白名单 ^[A-Za-z0-9_\-]{4,64}$ 语义（ensure_uid_safe 更严：另拒 ..），
        // 文案逐字保留
        if let Err(_) = crate::fs_utils::ensure_uid_safe(uid.trim()) {
            let msg = format!(
                "UserId 参数格式非法（仅允许 A-Z a-z 0-9 _ -，长度 4~64 位）: {}",
                uid.trim()
            );
            sink.step("fatal", StepStatus::Error, &msg);
            return Err(fatal_line(&msg));
        }
    }

    let mut sess = Session::new(&args);

    // F-75 M0-0.6 mac 灰度门控：档案表 mac_supported=false 的应用域，mac 版数据布局
    // 未经 M-1 侦察确认，切换/备份/恢复全链路直接拒绝（Windows 恒放行，行为零变化）。
    // 不用 #[cfg] 门控是为让字段在 Windows 构建也有消费方（无 dead_code 警告）。
    if !sess.prof.mac_supported && crate::platform::is_macos() {
        let msg = format!(
            "{} 的 macOS 版数据布局尚未侦察确认，切换/备份/恢复暂不可用（macOS 支持域按应用逐步放开）",
            sess.prof.app_name
        );
        sink.step("fatal", StepStatus::Error, &msg);
        return Err(fatal_line(&msg));
    }

    sink.step(
        "init",
        StepStatus::Info,
        &format!(
            "开始操作: {} (userId={}, targetApp={} → {})",
            args.action.as_str(),
            uid.trim(),
            format!("{:?}", args.target_app),
            sess.prof.app_name
        ),
    );

    // PS 的 init 行 targetApp=$TargetApp（字符串形态），此处对齐为档案字符串
    let r = match args.action {
        Action::Switch => switch_flow(&mut sess, uid.trim(), args.expected_current_uid.trim(), sink),
        Action::SaveCurrentLogin => {
            proc::stop_app(&mut sess, sink)?;
            backup_current(&sess, uid.trim(), sink)?;
            set_current_account(&sess, uid.trim());
            proc::start_app(&mut sess, sink)?;
            done(sink, &format!("已保存账号 {} 的当前登录态", uid.trim()))
        }
        Action::BackupCurrent => {
            // 审查修复：与 SaveCurrentLogin 对齐——先关客户端再读 auth/leveldb/vscdb，
            // 防止文件锁下拷贝静默缺文件生成"看似成功"的坏快照；备份完成后拉回
            proc::stop_app(&mut sess, sink)?;
            backup_current(&sess, uid.trim(), sink)?;
            set_current_account(&sess, uid.trim());
            proc::start_app(&mut sess, sink)?;
            done(sink, "备份完成")
        }
        Action::RestoreOnly => {
            // 与 Switch 的区别：只做「以目标快照覆盖当前」，不把当前登录态备份到
            // 原账号槽。当前登录态仍会先备份到 "last" 槽（安全回退）。
            proc::stop_app(&mut sess, sink)?;
            backup_current(&sess, "last", sink)?;
            restore_profile(&mut sess, uid.trim(), sink)?;
            set_current_account(&sess, uid.trim());
            proc::start_app(&mut sess, sink)?;
            done(
                sink,
                &format!(
                    "已恢复账号 {} 的登录态（当前登录态已备份到 last 槽）",
                    uid.trim()
                ),
            )
        }
        Action::ResetMachineId => {
            machine::reset_machine_id(sink)?;
            done(sink, "机器码已重置")
        }
        Action::ResetDeviceIds => {
            machine::reset_device_ids_only(&sess, sink)?;
            done(sink, "6 层设备标识重置完成")
        }
        Action::KeepAlive => keepalive_flow(&mut sess, sink),
    };
    // PS 顶层 catch 对译约定：PS throw 等价路径（restore 无快照/完整性校验失败/
    // 备份失败/设备重置未接入等）已在错误点经 thrown() 补发 fatal「失败: {msg}」；
    // PS 直接 exit 1 路径（预检失败/恢复后校验回滚完成）自带 fatal 行。此处透传。
    r
}

/// Switch 主流程（PS 1415-1477 逐段对译，含防误覆盖守卫与恢复后校验回滚）
fn switch_flow(
    sess: &mut Session,
    uid: &str,
    expected_uid: &str,
    sink: &dyn ProgressSink,
) -> Result<String, String> {
    // 预检查：目标账号是否有快照（在关闭应用之前检查；主槽缺失时允许 .bak 回退槽）
    let target = sess.prof.profiles_dir.join(uid);
    let target_bak = sess.prof.profiles_dir.join(format!("{uid}.bak"));
    if !target.exists() && !target_bak.exists() {
        let msg = format!("目标账号 {uid} 无快照，请先登录该账号并点击「保存当前登录态」");
        sink.step("fatal", StepStatus::Error, &msg);
        return Err(fatal_line(&msg));
    }
    proc::stop_app(sess, sink)?;

    // 保存当前登录态到 "last" 槽位（安全备份）
    backup_current(sess, "last", sink)?;

    // 防误覆盖守卫：仅当桌面端检测到的当前登录（expected_uid）与标记文件一致时才
    // 回写账号槽。不一致/未检测到 = 客户端很可能已手动重登或处于未登录态，此时把
    // "当前态"刷进标记槽会把错误内容覆盖掉该账号的快照（实测 B 被覆盖根因），
    // 故跳过并警告——last 槽始终有完整备份可回退。
    if let Some(cur) = get_current_account(sess) {
        if cur != uid {
            if !expected_uid.is_empty() && expected_uid == cur {
                backup_current(sess, &cur, sink)?;
                sink.step(
                    "backup",
                    StepStatus::Ok,
                    &format!("当前账号 {cur} 的登录态已备份"),
                );
            } else {
                sink.step(
                    "backup",
                    StepStatus::Warn,
                    &format!(
                        "检测到的当前登录（{}）与标记账号 {cur} 不一致，已跳过回写该账号槽位（当前态仍备份到 last），防止误覆盖",
                        if expected_uid.is_empty() {
                            "未识别或未检测到登录会话"
                        } else {
                            expected_uid
                        }
                    ),
                );
            }
        }
    }

    // 恢复目标账号的登录态
    restore_profile(sess, uid, sink)?;

    // 恢复后校验（issue #9，仅 icube）：①0 项恢复=快照空/损坏；②恢复后数据目录缺
    // storage.json / state.vscdb = 快照不含关键登录态或 TRAE 布局漂移。两种情况
    // 启动都只会「切了个寂寞」——从 last 槽回滚到切换前状态并重启，报 fatal 明示原因。
    if sess.prof.layout == Layout::Icube {
        let mut missing: Vec<String> = Vec::new();
        if sess.last_restored_count <= 0 {
            missing.push("（快照为空或损坏，0 项恢复）".to_string());
        } else {
            for f in ["User\\globalStorage\\storage.json", "User\\globalStorage\\state.vscdb"] {
                if !sess.prof.data_dir.join(f).exists() {
                    missing.push(f.to_string());
                }
            }
        }
        if !missing.is_empty() {
            sink.step(
                "restore",
                StepStatus::Warn,
                &format!(
                    "目标快照无效（{}），正在从 last 槽回滚到切换前状态…",
                    missing.join("；")
                ),
            );
            restore_profile(sess, "last", sink)?;
            proc::start_app(sess, sink)?;
            let msg = format!(
                "账号 {uid} 的快照无效（{}），已回滚到切换前状态。请登录该账号后重新「保存当前登录态」；若重新保存后仍报此错，可能是 TRAE 新版登录态布局变化，请携带日志反馈",
                missing.join("；")
            );
            sink.step("fatal", StepStatus::Error, &msg);
            return Err(fatal_line(&msg));
        }
    }

    set_current_account(sess, uid);
    proc::start_app(sess, sink)?;

    // authfile 布局：verify 超时 ≠ 切换失败（快照已恢复、客户端已启动），但必须在
    // done 里如实告知「登录身份未确认」，否则前端报「切换成功」掩盖未登录事实
    //（switcher.log 实测 4/4 超时）；Reverted = live 身份被客户端回退到旧账号（信号④），
    // 同样如实告知
    if sess.prof.layout == Layout::Authfile {
        match authfile::confirm_switch(sess, uid, sink) {
            authfile::VerifyResult::Timeout => {
                return done(
                    sink,
                    &format!(
                        "已切换至账号 {uid}（警告：30 秒内未确认登录身份，请打开客户端核实；若客户端未登录，请重新登录后「保存当前登录态」）"
                    ),
                );
            }
            authfile::VerifyResult::Reverted => {
                return done(
                    sink,
                    &format!(
                        "已切换至账号 {uid}（警告：客户端实际登录身份已回退到其他账号，本次切换可能未生效，请打开客户端核实；若未登录，请重新切换或在客户端登录后「保存当前登录态」）"
                    ),
                );
            }
            authfile::VerifyResult::Ok => {}
        }
    }
    done(sink, &format!("已切换至账号 {uid}"))
}

/// KeepAlive（PS 1514-1528）：运行中跳过；否则启动 → 8s 联网刷新 → 优雅关闭。
/// sid_guard 30 天滑动续期由豆包客户端自己完成（cookie 值为客户端级加密，外部无法
/// 离线续写），本动作仅负责"启动→等待联网刷新→关闭"。
fn keepalive_flow(sess: &mut Session, sink: &dyn ProgressSink) -> Result<String, String> {
    if proc::is_running(sess) {
        sink.step(
            "keepalive",
            StepStatus::Ok,
            &format!("{} 正在运行，客户端会话活跃，本次跳过", sess.prof.app_name),
        );
        return done(sink, "保活检查完成（应用运行中）");
    }
    proc::start_app(sess, sink)?;
    sink.step(
        "keepalive",
        StepStatus::Running,
        "已启动，等待会话联网刷新（8 秒）",
    );
    std::thread::sleep(std::time::Duration::from_secs(8));
    proc::stop_app(sess, sink)?;
    done(sink, "保活完成（启动 8 秒 → 优雅关闭，sid_guard 已滑动续期）")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 收集步骤的内存 Sink（测试断言用）
    struct MemSink {
        steps: std::sync::Mutex<Vec<(String, String, String)>>,
    }
    impl MemSink {
        fn new() -> MemSink {
            MemSink { steps: std::sync::Mutex::new(Vec::new()) }
        }
    }
    impl ProgressSink for MemSink {
        fn step(&self, stage: &str, status: StepStatus, message: &str) {
            self.steps
                .lock()
                .unwrap()
                .push((stage.to_string(), status.as_str().to_string(), message.to_string()));
        }
    }

    #[test]
    fn glob_大小写不敏感与ps_like语义一致() {
        assert!(glob_match_ci("*Doubao*", "doubao.lnk"));
        assert!(glob_match_ci("*TRAE*", "Trae Setup.lnk"));
        assert!(glob_match_ci("Trae*", "TRAE SOLO CN"));
        assert!(glob_match_ci("*豆包*", "豆包.lnk"));
        assert!(!glob_match_ci("*Doubao*", "wechat.lnk"));
        assert!(glob_match_ci("exact", "exact"));
        assert!(!glob_match_ci("exact", "exactly"));
        // 中缀模式
        assert!(glob_match_ci("*Trae*CN*", "Setup Trae CN x64.lnk"));
    }

    #[test]
    fn path_eq_大小写与回退语义() {
        let a = Path::new("c:\\windows\\system32\\drivers\\etc\\hosts");
        let b = Path::new("C:\\WINDOWS\\SYSTEM32\\DRIVERS\\ETC\\HOSTS");
        assert!(path_eq(a, b));
        // 双侧不存在：回退小写字符串比较
        assert!(path_eq(Path::new("D:\\No\\Such\\File.EXE"), Path::new("d:\\no\\such\\file.exe")));
        assert!(!path_eq(Path::new("D:\\A.txt"), Path::new("D:\\B.txt")));
    }

    #[test]
    fn get_current_account_剥存量bom() {
        let dir = std::env::temp_dir().join(format!("sw-mod-test-{}", std::process::id()));
        let prof_dir = dir.join("data").join("profiles");
        std::fs::create_dir_all(&prof_dir).unwrap();
        std::fs::write(prof_dir.join("current_account.txt"), "\u{feff}4487568582777872").unwrap();
        let mut args = RunArgs {
            action: Action::KeepAlive,
            target_app: TargetApp::TraeWork,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            data_dir: dir.clone(),
        };
        args.target_app = TargetApp::TraeWork;
        let sess = Session::new(&args);
        assert_eq!(get_current_account(&sess).as_deref(), Some("4487568582777872"));
        // 无 BOM 正常读取
        std::fs::write(prof_dir.join("current_account.txt"), "2117003799429594").unwrap();
        assert_eq!(get_current_account(&sess).as_deref(), Some("2117003799429594"));
        // 空文件 → None
        std::fs::write(prof_dir.join("current_account.txt"), "").unwrap();
        assert_eq!(get_current_account(&sess), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_action_缺少userid_输出init错误并返回fatal载荷() {
        // 两个 run_action 测试并行执行会争抢全局 action_gate（try_lock 失败路径
        // 不发任何步骤 → steps[0] 越界），用测试串行锁排队
        let _guard = test_io_lock();
        let sink = MemSink::new();
        let r = run_action(
            RunArgs {
                action: Action::Switch,
                target_app: TargetApp::TraeWork,
                user_id: None,
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                data_dir: std::env::temp_dir(),
            },
            &sink,
        );
        assert!(r.is_err());
        let steps = sink.steps.lock().unwrap();
        assert_eq!(steps[0], ("init".into(), "error".into(), "缺少 -UserId 参数".into()));
        assert_eq!(r.as_ref().unwrap_err(), "缺少 -UserId 参数");
    }

    #[test]
    fn run_action_uid非法_文案逐字保留() {
        // 同上：与另一 run_action 测试经串行锁互斥，避免 action_gate 争抢
        let _guard = test_io_lock();
        let sink = MemSink::new();
        let r = run_action(
            RunArgs {
                action: Action::Switch,
                target_app: TargetApp::TraeWork,
                user_id: Some("../evil".into()),
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                data_dir: std::env::temp_dir(),
            },
            &sink,
        );
        assert!(r.is_err());
        let steps = sink.steps.lock().unwrap();
        assert_eq!(
            steps[0],
            (
                "fatal".into(),
                "error".into(),
                "UserId 参数格式非法（仅允许 A-Z a-z 0-9 _ -，长度 4~64 位）: ../evil".into()
            )
        );
    }

    #[test]
    fn step_line_四字段齐全() {
        let line = step_line("stop", StepStatus::Running, "正在关闭 Trae Work");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["stage"], "stop");
        assert_eq!(v["status"], "running");
        assert_eq!(v["message"], "正在关闭 Trae Work");
        assert!(v["time"].as_str().unwrap().contains(':'));
    }
}
