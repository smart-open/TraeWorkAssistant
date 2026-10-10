//! 登录态切换器（原 trae-switch-bridge.ps1 全量 Rust 化入口）。
//!
//! 架构变更（vs PS 桥）：进度从「powershell 子进程 stdout NDJSON 管道」收敛为
//! 进程内 [`ProgressSink`] 回调——消除 GBK/OEM 乱码根因、CREATE_NO_WINDOW、
//! stderr 防死锁线程、done 探测兜底与 ~300-800ms 进程冷启动。
//!
//! 红线对齐：
//! - 前端零改动：NDJSON 行 `{stage,status,message,time}` 与 `*-done {success,raw}`
//!   语义逐字段兼容，全部 stage 消息文案逐字保留（3.6.5 起 backup 消息随 .bak2
//!   两代轮转更新文案——前端仅透传展示、无精确匹配，已核实）；
//! - 快照数据零迁移：profiles*/<slot>{,.bak,.bak2} 结构、current_account.txt、
//!   meta.json、snapshot_meta.json 格式不变，新旧版本快照互认（.bak2 与
//!   <slot>.meta.json 身份 sidecar 为新增，旧版本可无视）；
//! - 日志落点不变：`<data_dir>/logs/switcher.log` 追加格式
//!   `[yyyy-MM-dd HH:mm:ss] [stage] message`。

pub mod authfile;
pub mod chromium;
pub mod copy;
pub mod electron_root;
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
    /// F-80 M3：Qoder CN IDE（VS Code fork，icube 布局，数据目录 %APPDATA%\QoderCN）
    Qoder,
    /// 2026-10-02：Qoder Work 独立客户端（Electron，0.4.3+；数据目录
    /// %APPDATA%\com.qodercn.app.stable，根级 Chromium 会话，与豆包多 Profile
    /// 布局不同构 → electron_root 管线）。与 Qoder IDE 为两套独立登录态。
    QoderWork,
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
            "Qoder" => TargetApp::Qoder,
            "QoderWork" => TargetApp::QoderWork,
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
    /// F-80 §5.10.2 Qoder 专用：账号绑定 machine_id（切换/恢复成功后对本地存储
    /// 三层覆写）；None = 不覆写（其余应用恒 None）
    pub machine_id_override: Option<String>,
    /// AIWORKDATA_DIR 等价物（进程内直传；CLI 模式来自 AppState）
    pub data_dir: PathBuf,
}

/// 执行会话：一次 run_action 内共享的可变状态（PS $Script: 作用域 → 显式结构体）
pub struct Session {
    /// 所属目标应用（守卫按 target 收窄数据源：日志探测仅 Trae 系有意义）
    pub target: TargetApp,
    pub prof: AppProfile,
    pub data_dir: PathBuf,
    /// PS $Script:_TraeExeCache（exe 发现第 6 级兜底 + Stop 前缓存）
    pub exe_cache: Option<PathBuf>,
    /// PS $Script:_LastRestoredCount（icube/electron_root 恢复后校验；每次恢复先置 -1，
    /// 对应布局结尾写实际值）
    pub last_restored_count: i64,
    /// PS $Script:LaunchProxyPort（>0 启动注入 --proxy-server）
    pub launch_proxy_port: Option<u16>,
    pub include_indexeddb: bool,
    /// F-80 §5.10.2：账号绑定 machine_id（Qoder 切换/恢复后本地指纹覆写取值）
    pub machine_id_override: Option<String>,
    /// authfile 布局：共享 auth 文件目录与文件（$Script:WbAuthDir/WbAuthFile；
    /// 独立字段便于测试注入临时路径）
    pub auth_dir: PathBuf,
    pub auth_file: PathBuf,
    /// L2 守卫探测到的客户端当前登录 uid（icube/electron_root 布局，stop 后守卫写入）；
    /// 由 save_identity_guard 写入，backup_icube 据此写槽位 sidecar
    pub detected_live_uid: Option<String>,
    /// 命令层预探测的当前登录账号（2026-10-02 审查新增）：Qoder 保存链由
    /// profile_backup 经 vscdb secret://userInfo 解密传入，QoderWork 由客户端
    /// Cookies qoderuid 解密传入（Work/其他为空）。L2 守卫优先采用；为空时按布局
    /// 回退（icube 仅 Trae 系走日志探测，见 save_identity_guard）
    pub expected_current_uid: String,
    /// issue #78 方案 A：本次切换由「凭证副本合成最小快照」引导（切换前该账号
    /// 无任何快照）。confirm_switch 确认成功后据此升级完整快照；每次 run_action
    /// 独立会话，无需跨动作传递
    pub bootstrap_done: bool,
}

impl Session {
    pub fn new(args: &RunArgs) -> Session {
        Session {
            target: args.target_app,
            prof: profile::profile_for(args.target_app, &args.data_dir),
            data_dir: args.data_dir.clone(),
            exe_cache: None,
            last_restored_count: -1,
            launch_proxy_port: args.proxy_port,
            include_indexeddb: args.include_indexeddb,
            machine_id_override: args.machine_id_override.clone(),
            auth_dir: authfile::wb_auth_dir(),
            auth_file: authfile::wb_auth_file(),
            detected_live_uid: None,
            expected_current_uid: args.expected_current_uid.clone(),
            bootstrap_done: false,
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
        // issue #44：emit 失败不再静默——落 switcher.log，避免前端收不到进度/
        // 终态事件却无任何线索（表现为「一直显示切换中」）
        if let Err(e) = self.app.emit(self.event, step_line(stage, status, message)) {
            append_switcher_log(
                &self.log_file,
                "emit-error",
                &format!("事件 {} 发送失败（前端可能收不到进度/结束信号）: {e}", self.event),
            );
        }
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

/// L2 身份硬校验（2026-09-29 实测事故：1335 登录态被「保存当前登录态→4487」
/// 写进 4487 槽）。仅 icube 布局（TraeWork/Trae）有可靠日志数据源。
///
/// 必须在 stop_app 之后（日志已完整落盘）、backup_current 之前（后者内部
/// rotate_bak 会覆盖槽位，拦截必须发生在任何写操作前）调用，且调用方须在
/// stop 前以 [`proc::is_running`] 记录 `was_running`。探测与目标不符 →
/// 仅当 `was_running` 为真才重启客户端还原现场（原本没开的不拉起），拒绝，
/// 绝不 rotate。
/// L2 保存身份守卫（SaveCurrentLogin/BackupCurrent 流内，stop 后执行）：探测客户端
/// 当前登录账号，与目标槽位不一致 → 拒绝保存并重启客户端还原现场，防 A 的登录态
/// 覆盖污染 B 的槽位（icube 布局 1335/4487 同型事故）。
/// live 探测按布局分派（2026-10-02 审查重构）：
/// - icube：命令层预探测优先（Qoder 保存链 vscdb secret://userInfo 解密传入，唯一
///   可靠源）；空则日志探测回退——仅 TraeWork/Trae（Qoder 池 id 为 qd-<hex> 形态，
///   与日志数字 uid 永不相等，回退会把「格式不同」误判为「登录了别的账号」恒拒保存）
/// - electron_root（QoderWork）：预探测（命令层 live_work_account_id 解密 auth.v1.dat
///   user.id；2026-10-04 实测修正——Cookies 库无 qoderuid cookie，原探测恒空守卫失效）
///   非空才校验；空（未登录/文件缺失/解密失败）= fail-open 放行
/// - 其余布局（chromium/authfile）无 icube 型身份防线，直接放行
fn save_identity_guard(
    sess: &mut Session,
    uid: &str,
    was_running: bool,
    sink: &dyn ProgressSink,
) -> Result<(), String> {
    let live = match sess.prof.layout {
        Layout::Icube => {
            if !sess.expected_current_uid.is_empty() {
                Some(sess.expected_current_uid.clone())
            } else if matches!(sess.target, TargetApp::TraeWork | TargetApp::Trae) {
                icube::detect_live_uid(&sess.prof.data_dir)
            } else {
                None
            }
        }
        Layout::ElectronRoot => {
            (!sess.expected_current_uid.is_empty()).then(|| sess.expected_current_uid.clone())
        }
        _ => return Ok(()),
    };
    match live {
        Some(live) if live != uid => {
            // issue #55 审查修复：文案按槽位状态分流（首存指引/污染自愈指引/通用），
            // 不再一律指回「切换」（对无快照账号不可执行、对污染槽位是死循环）
            let msg = save_reject_message(&sess.prof.profiles_dir, uid, &live);
            // 还原现场：仅当客户端原本在运行（调用方 stop 前经 is_running 确认）
            // 才拉回，避免把原本没开的客户端意外拉起；失败仅 Warn，不影响拒绝结果
            if was_running {
                if let Err(e) = proc::start_app(sess, sink) {
                    sink.step(
                        "guard",
                        StepStatus::Warn,
                        &format!("客户端重启失败，请手动打开: {e}"),
                    );
                }
            }
            // 流内 fatal 行（NDJSON 语义：错误终态必有最后一行 fatal）+
            // Err 载荷带 [fatal] 前缀（前端 saveCurrentLogin 失败 toast 按
            // /\[fatal\]\s*(.+)$/ 提取完整原因，与 panic 兜底既有约定一致）
            sink.step("fatal", StepStatus::Error, &format!("失败: {msg}"));
            Err(fatal_line(&format!("[fatal] {msg}")))
        }
        Some(live) => {
            sink.step(
                "guard",
                StepStatus::Ok,
                &format!("身份校验通过：客户端当前登录的是账号 {live}"),
            );
            // 一致：记录身份供 backup_icube 写入槽位 sidecar（restore 前校验用）
            sess.detected_live_uid = Some(live);
            Ok(())
        }
        None => {
            // 探测失败（无日志/最新会话无 uid 记录/命令层未预探测）：fail-open 放行，
            // 与 F2-5 authfile 守卫的 None 放行策略一致
            sink.step(
                "guard",
                StepStatus::Warn,
                "未能从客户端日志判定当前登录账号，跳过身份校验",
            );
            Ok(())
        }
    }
}

/// 保存守卫拒绝文案分派（issue #55 审查修复 2026-10-03）：L1 命令层与 L2 switcher
/// 共用，按槽位状态给出**可执行**的补救指引。通用文案的「先切换到该账号」对两类
/// 场景不可执行/死循环（issue #55 实测）：
/// ① 槽位不存在（OAuth/扫描新入池账号首次保存）：切换无快照可恢复 → 指引客户端手动登录；
/// ② 槽位 sidecar 记录的保存身份与槽位不符（历史污染）：「切换」只会恢复出被污染的
///    会话（A 槽存了 B 的会话后，切到 A 恒登录为 B）→ 指引客户端退出重登；
/// ③ 其余：通用文案（保留原语义 + 手动登录兜底提示）
pub(crate) fn save_reject_message(profiles_dir: &Path, uid: &str, live: &str) -> String {
    let slot = profiles_dir.join(uid);
    let slot_bak = profiles_dir.join(format!("{uid}.bak"));
    if !slot.exists() && !slot_bak.exists() {
        return format!(
            "客户端当前登录的是账号 {live}，与要保存的账号 {uid} 不一致，已拒绝保存。\
账号 {uid} 还没有本地快照（如刚通过 OAuth/扫描入池），「切换」无法恢复出该账号。\
请在客户端手动登录账号 {uid}（勿用「切换」），登录成功后再点「保存当前登录态」。"
        );
    }
    let sidecar = profiles_dir.join(format!("{uid}.meta.json"));
    if let Ok(text) = std::fs::read_to_string(&sidecar) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(saved) = v.get("detectedUid").and_then(|x| x.as_str()) {
                if !saved.is_empty() && saved != uid {
                    return format!(
                        "客户端当前登录的是账号 {live}，与要保存的账号 {uid} 不一致，已拒绝保存。\
账号 {uid} 的快照疑似已被账号 {saved} 的登录态污染（该快照保存时检测到的客户端身份是 {saved}），\
此时「切换」到账号 {uid} 也只会恢复出账号 {saved} 的会话，反复操作无法自愈。\
请在客户端退出登录并手动重新登录账号 {uid}，再点「保存当前登录态」覆盖修复。"
                    );
                }
            }
        }
    }
    format!(
        "客户端当前登录的是账号 {live}，与要保存的账号 {uid} 不一致，已拒绝保存\
（防止账号 {uid} 的槽位被账号 {live} 的登录态覆盖污染）。\
请先「切换」到账号 {uid} 并在客户端确认登录身份是 {uid}（若不是，请退出登录后手动登录），\
再点「保存当前登录态」。"
    )
}

fn backup_current(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let slot_dir = sess.prof.profiles_dir.join(slot);
    let r = match sess.prof.layout {
        Layout::Authfile => authfile::backup_authfile(sess, slot, sink),
        Layout::Chromium => chromium::backup_chromium(sess, slot, sink),
        Layout::ElectronRoot => electron_root::backup_electron_root(sess, slot, sink),
        Layout::Icube => icube::backup_icube(sess, slot, sink),
    };
    // 无论成败都在出口失效该槽位体积缓存（失败也可能已 rotate/写入部分文件）——
    // 在此收口可一次覆盖 SaveCurrentLogin / BackupCurrent / Switch / RestoreOnly(last)
    // 全部写槽路径，不再隐式依赖「备份重建槽目录刷新 mtime」这一实现细节
    crate::commands::profile::invalidate_profile_stats(&slot_dir);
    r
}

fn restore_profile(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    // 恢复项计数（Switch 恢复后校验用）：每次恢复先重置为 -1；icube/electron_root
    // 结尾写实际值。校验处用 -le 0 拦截：等价于 0 项拷贝=快照空/损坏；-1 作为防御一并拦下。
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
        Layout::ElectronRoot => electron_root::restore_electron_root(sess, slot, sink),
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
    // issue #44 审查修复：try_lock 的 Err 有两种——WouldBlock（确实忙碌，拒绝）
    // 与 Poisoned（上次持有者 panic 中毒）。毒化只说明上一次动作异常终止，数据本身
    // 是 () 无不变量可破坏，into_inner() 恢复即可；否则一次 panic 后所有后续
    // 切换/备份永久报「已有切换/备份操作进行中」。
    let _guard = match action_gate().try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err("已有切换/备份操作进行中，请稍后再试".to_string());
        }
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
    };

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
            let was_running = proc::is_running(&sess);
            proc::stop_app(&mut sess, sink)?;
            save_identity_guard(&mut sess, uid.trim(), was_running, sink)?;
            backup_current(&sess, uid.trim(), sink)?;
            set_current_account(&sess, uid.trim());
            proc::start_app(&mut sess, sink)?;
            done(sink, &format!("已保存账号 {} 的当前登录态", uid.trim()))
        }
        Action::BackupCurrent => {
            // 审查修复：与 SaveCurrentLogin 对齐——先关客户端再读 auth/leveldb/vscdb，
            // 防止文件锁下拷贝静默缺文件生成"看似成功"的坏快照；备份完成后拉回
            let was_running = proc::is_running(&sess);
            proc::stop_app(&mut sess, sink)?;
            save_identity_guard(&mut sess, uid.trim(), was_running, sink)?;
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
            apply_fingerprint_override(&sess, sink);
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

/// F-80 §5.10.2：Qoder 本地存储设备指纹覆写挂点（切号/恢复成功后、启动前）。
/// 仅 icube 布局且 Session 携带 machine_id_override 时执行：machineid 文件 +
/// storage.json 遥测三键 + state.vscdb storage.serviceMachineId 三层统一覆写为
/// 账号绑定值。失败仅 Warn 不阻断切换（API 请求侧指纹由 tasks::qoder_common 独立
/// 生效）；快照无效回滚路径在挂点之前 return，不会污染切换前现场。
fn apply_fingerprint_override(sess: &Session, sink: &dyn ProgressSink) {
    if sess.prof.layout != Layout::Icube {
        return;
    }
    let Some(mid) = sess.machine_id_override.as_deref() else {
        return;
    };
    match machine::apply_qoder_fingerprint(&sess.prof.data_dir, mid) {
        Ok(n) => sink.step(
            "fingerprint",
            StepStatus::Ok,
            &format!("本地设备指纹已按账号绑定覆写（{n} 处）"),
        ),
        Err(e) => sink.step(
            "fingerprint",
            StepStatus::Warn,
            &format!("本地设备指纹覆写失败（不阻断切换）: {e}"),
        ),
    }
}

/// 恢复后校验的 missing 判定（switch_flow 主快照与 .bak 回退重试共用；issue #9）：
/// ①0 项恢复 = 快照空/损坏；②缺关键登录态文件 = 快照不含登录态或布局漂移。
/// 仅 icube/electron_root 有可靠判定数据源，其余布局恒空。
fn post_restore_missing(sess: &Session) -> Vec<String> {
    match sess.prof.layout {
        Layout::Icube => {
            let mut m: Vec<String> = Vec::new();
            if sess.last_restored_count <= 0 {
                m.push("（快照为空或损坏，0 项恢复）".to_string());
            } else {
                for f in ["User\\globalStorage\\storage.json", "User\\globalStorage\\state.vscdb"] {
                    // 清单为 Windows 反斜杠形态：join 前必须组件化——mac 上 join 整串会把
                    // 反斜杠当作字面文件名，校验恒失败 → 恒回滚 last 槽 =「无论怎么切换
                    // 都是最后一个登录的账号」（2026-09-20 mac 实测根因，switcher.log 佐证）
                    if !sess.prof.data_dir.join(icube::rel_path(f)).exists() {
                        m.push(f.to_string());
                    }
                }
            }
            m
        }
        // 2026-10-02 审查：QoderWork（electron_root）同型校验——Local State 是
        // cookie 解密密钥元数据，缺失则 Work 恢复后必然未登录，按 fatal 回滚；
        // Cookies 缺失仅 Warn（快照可能是「启动过但未登录」的合法状态）
        Layout::ElectronRoot => {
            let mut m: Vec<String> = Vec::new();
            if sess.last_restored_count <= 0 {
                m.push("（快照为空或损坏，0 项恢复）".to_string());
            } else if !sess.prof.data_dir.join("Local State").exists() {
                m.push("Local State".to_string());
            }
            m
        }
        _ => Vec::new(),
    }
}

/// issue #78 方案 A：目标账号无任何快照时，尝试用工具侧凭证副本（token store
/// wb_tokens 表，账号登录/OAuth 时自动留存）合成最小 authfile 快照，解除
/// 「切换要快照 → 快照要保存 → 保存被守卫拒绝（客户端当前登录是别的账号）→
/// 旧账号不会被自动登出」的冷启动死锁。合成最小快照恢复出登录态后，客户端
/// 接受并确认，再由 switch_flow 在 confirm Ok 后备份完整登录态升级快照。
/// 仅 WorkBuddy：共享 auth 文件是 WorkBuddy 登录驱动源（F2-4）；CodeBuddy 登录
/// 真源在客户端加密 vscdb 内，工具侧无法离线合成。
/// 返回 true = 合成成功（预检放行，继续切换）；false = 无法合成（调用方 fatal）。
fn bootstrap_snapshot_from_token_store(sess: &Session, uid: &str, sink: &dyn ProgressSink) -> bool {
    if sess.prof.app_name != "WorkBuddy" {
        return false;
    }
    sink.step(
        "bootstrap",
        StepStatus::Info,
        &format!("目标账号 {uid} 无快照，尝试用工具侧凭证副本合成最小登录态…"),
    );
    let store = crate::tasks::wb_common::token_store_load_secure(&sess.data_dir);
    let Some(rec) = store.get("tokens").and_then(|t| t.get(uid)) else {
        sink.step(
            "bootstrap",
            StepStatus::Warn,
            &format!(
                "凭证副本（token store）中无账号 {uid} 的记录，无法合成。\
                 请在客户端手动登录账号 {uid} 后点「保存当前登录态」建立首个快照"
            ),
        );
        return false;
    };
    let creds = crate::tasks::wb_common::creds_of(rec);
    if creds.access_token.is_empty() {
        sink.step(
            "bootstrap",
            StepStatus::Warn,
            &format!(
                "凭证副本中账号 {uid} 无有效 access_token，无法合成。\
                 请在客户端手动登录账号 {uid} 后点「保存当前登录态」建立首个快照"
            ),
        );
        return false;
    }
    // 真实 uid 以账号池条目为准（uid 是 uuid 体系，槽名 wb-<hash> 只是池 id）；
    // 池缺失时回退凭证记录里的 uid
    let pool = crate::store::docs::wb_pool_load(&crate::store::db(&sess.data_dir));
    let pool_uid = pool
        .get("accounts")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(uid))
                .and_then(|a| a.get("uid"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .filter(|s| !s.is_empty());
    let real_uid = pool_uid.or_else(|| {
        if creds.uid.is_empty() {
            None
        } else {
            Some(creds.uid.clone())
        }
    });
    let Some(real_uid) = real_uid else {
        sink.step(
            "bootstrap",
            StepStatus::Warn,
            "无法确定账号真实 uid（账号池与凭证记录均缺失），无法合成",
        );
        return false;
    };
    let mut creds = creds;
    creds.uid = real_uid;
    authfile::synthesize_auth_snapshot(sess, uid, &creds, sink).is_some()
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
        // issue #78 方案 A：authfile 布局先尝试凭证副本合成最小快照（冷启动死锁
        // 解除），合成成功继续正常切换；失败走改进 fatal 文案（明示守卫闭环与
        // 手动出路，替换原「请先登录并保存」对冷启动场景不可执行的对仗文案）
        let bootstrapped = if sess.prof.layout == Layout::Authfile {
            bootstrap_snapshot_from_token_store(sess, uid, sink)
        } else {
            false
        };
        if !bootstrapped {
            let msg = match (sess.prof.layout, sess.prof.app_name) {
                (Layout::Authfile, "CodeBuddy") => format!(
                    "目标账号 {uid} 无快照，且 CodeBuddy 登录真源在客户端加密数据库（vscdb），\
                     工具无法自动合成。这是「切换要快照、快照要保存、保存又被当前登录不一致拒绝」\
                     的冷启动闭环：请先在客户端手动登录账号 {uid}（旧账号暂不退出也可直接切换登录），\
                     再点「保存当前登录态」建立首个快照"
                ),
                (Layout::Authfile, _) => format!(
                    "目标账号 {uid} 无快照，凭证副本中也没有该账号的有效登录凭证，无法自动合成。\
                     请先在客户端手动登录账号 {uid}，再点「保存当前登录态」建立首个快照"
                ),
                _ => format!("目标账号 {uid} 无快照，请先登录该账号并点击「保存当前登录态」"),
            };
            sink.step("fatal", StepStatus::Error, &msg);
            return Err(fatal_line(&msg));
        }
        sess.bootstrap_done = true;
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

    // 恢复后校验（issue #9，仅 icube/electron_root）：①0 项恢复=快照空/损坏；
    // ②恢复后数据目录缺关键登录态文件 = 快照不含登录态或布局漂移。两种情况
    // 启动都只会「切了个寂寞」——从 last 槽回滚到切换前状态并重启，报 fatal 明示原因。
    let mut missing = post_restore_missing(sess);
    // 缺陷4（issue #55 实测「traecode 切换失败」根因=主快照无效）：主快照损坏时
    // 先尝试 <uid>.bak 回退槽（rotate_bak 保留的上一次覆盖前旧快照），有效则改用，
    // 仍无效才回滚——旧逻辑直接回滚 fatal，用户必须手动重登重建快照；.bak 里往往
    // 还留着一份完好的历史快照，白白浪费。
    // 前置条件：主槽目录存在（若主槽本就缺失，resolve_slot 已自动回退 .bak，无需重试）
    if !missing.is_empty() {
        let bak_slot = format!("{uid}.bak");
        if sess.prof.profiles_dir.join(uid).exists()
            && sess.prof.profiles_dir.join(&bak_slot).exists()
        {
            sink.step(
                "restore",
                StepStatus::Warn,
                &format!(
                    "主快照无效（{}），尝试回退槽 {bak_slot}…",
                    missing.join("；")
                ),
            );
            // 恢复失败不中止（审查修复）：此时 live 已是坏的主快照内容，必须落回
            // 下方 last 槽回滚 + 重启客户端，不能把停在关机状态的现场丢给用户
            match restore_profile(sess, &bak_slot, sink) {
                Ok(()) => {
                    missing = post_restore_missing(sess);
                    if missing.is_empty() {
                        sink.step(
                            "restore",
                            StepStatus::Ok,
                            &format!("回退槽 {bak_slot} 有效，已改用其恢复登录态"),
                        );
                    }
                }
                Err(e) => sink.step(
                    "restore",
                    StepStatus::Warn,
                    &format!("回退槽 {bak_slot} 恢复失败（将回滚到切换前状态）: {e}"),
                ),
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
            "账号 {uid} 的快照无效（{}），已回滚到切换前状态。请在客户端手动登录账号 {uid}\
             （登录成功后点「保存当前登录态」重建快照；勿再「切换」到该账号，会重复此错误）；\
             若重新保存后仍报此错，可能是 {} 新版登录态布局变化，请携带日志反馈",
            missing.join("；"),
            sess.prof.app_name
        );
        sink.step("fatal", StepStatus::Error, &msg);
        return Err(fatal_line(&msg));
    }
    // QoderWork 软校验：Cookies 缺失（快照为「启动过但未登录」态）不回滚，如实告知
    if sess.prof.layout == Layout::ElectronRoot
        && !sess.prof.data_dir.join("Network").join("Cookies").exists()
    {
        sink.step(
            "restore",
            StepStatus::Warn,
            "目标快照不含 Cookies——恢复后 Qoder Work 可能为未登录状态，请登录后重新保存快照",
        );
    }

    apply_fingerprint_override(sess, sink);
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
            authfile::VerifyResult::Ok => {
                // issue #78 方案 A：本次切换由合成最小快照引导（此前该账号无任何
                // 快照）。登录身份已确认切到目标账号，客户端此刻落盘的登录态已是
                // 完整现场——立即备份升级快照（rotate_bak 顺带把合成槽轮转为 .bak
                // 留作回退）。best-effort：失败仅告警，下次「保存当前登录态」可补。
                if sess.bootstrap_done {
                    sess.bootstrap_done = false;
                    match backup_current(sess, uid, sink) {
                        Ok(()) => sink.step(
                            "backup",
                            StepStatus::Ok,
                            &format!("客户端完整登录态已升级至账号 {uid} 快照"),
                        ),
                        Err(e) => sink.step(
                            "backup",
                            StepStatus::Warn,
                            &format!(
                                "完整登录态升级失败（不影响本次切换，之后点「保存当前登录态」可补）: {e}"
                            ),
                        ),
                    }
                }
            }
        }
    }
    done(sink, &format!("已切换至账号 {uid}"))
}

/// KeepAlive（PS 1514-1528）：运行中跳过；否则启动 → 8s 联网刷新 → 优雅关闭。
/// sid_guard 30 天滑动续期由豆包客户端自己完成（cookie 值为客户端级加密，外部无法
/// 离线续写），本动作仅负责"启动→等待联网刷新→关闭"。
/// 关闭阶段按 spawn 返回的主进程 PID 精确命中本次实例（proc::stop_spawned）：
/// 8 秒等待窗口内用户手动开启的应用实例不受影响，消除 TOCTOU 误关风险。
fn keepalive_flow(sess: &mut Session, sink: &dyn ProgressSink) -> Result<String, String> {
    if proc::is_running(sess) {
        sink.step(
            "keepalive",
            StepStatus::Ok,
            &format!("{} 正在运行，客户端会话活跃，本次跳过", sess.prof.app_name),
        );
        return done(sink, "保活检查完成（应用运行中）");
    }
    let spawned_pid = proc::start_app_pid(sess, sink)?;
    sink.step(
        "keepalive",
        StepStatus::Running,
        "已启动，等待会话联网刷新（8 秒）",
    );
    std::thread::sleep(std::time::Duration::from_secs(8));
    proc::stop_spawned(sess, sink, &[spawned_pid])?;
    done(
        sink,
        "保活完成（启动 8 秒 → 按 PID 精确关闭，sid_guard 已滑动续期）",
    )
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

    /// save_identity_guard 分派（2026-10-02 审查补齐 QoderWork 守卫的集成验证）：
    /// electron_root 布局下预探测非空且与槽位一致 → Ok 并记录 detected_live_uid；
    /// 预探测为空（未登录/Cookie 缺失/解密失败）→ Ok fail-open 且不记录。
    /// 注：不一致分支含 proc::start_app（本机若装有 Work 会真实拉起客户端），
    /// 出于副作用安全不在测试范围，其拒绝语义由消息拼接单测覆盖。
    #[test]
    fn save_identity_guard_electron_root_预探测分派() {
        let base = std::env::temp_dir().join(format!("sw-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let args = RunArgs {
            action: Action::BackupCurrent,
            target_app: TargetApp::QoderWork,
            user_id: Some("qd-x".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: "qd-x".into(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        // 一致：Ok + detected_live_uid 记录（backup 流随后据此回写来源标记）
        // was_running=false：测试环境不拉起客户端（mismatch 分支 proc::start_app 有副作用）
        let mut sess = Session::new(&args);
        save_identity_guard(&mut sess, "qd-x", false, &MemSink::new()).unwrap();
        assert_eq!(sess.detected_live_uid.as_deref(), Some("qd-x"));
        // fail-open：expected_current_uid 为空（RunArgs 未预探测到登录账号）
        let args2 = RunArgs { expected_current_uid: String::new(), ..args };
        let mut sess2 = Session::new(&args2);
        let sink = MemSink::new();
        save_identity_guard(&mut sess2, "qd-x", false, &sink).unwrap();
        assert!(sess2.detected_live_uid.is_none());
        assert!(
            sink.steps
                .lock()
                .unwrap()
                .iter()
                .any(|(s, st, _)| s == "guard" && st == "warn"),
            "fail-open 应留 Warn 步骤"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// save_reject_message 三类场景分流（issue #55）：无槽位 → 首存指引（勿用切换）；
    /// sidecar detectedUid ≠ 槽位 → 污染自愈指引（切换是死循环）；其余 → 通用文案
    #[test]
    fn save_reject_message_三类场景分流() {
        let base = std::env::temp_dir().join(format!(
            "sw-reject-msg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let profiles = base.join("data").join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();

        // ① 无槽位（OAuth/扫描新入池账号首次保存）
        let m1 = save_reject_message(&profiles, "3401253136383392", "2011463847263801");
        assert!(m1.contains("还没有本地快照"), "m1={m1}");
        assert!(m1.contains("手动登录"), "m1={m1}");

        // ② 槽位存在 + sidecar 记录的保存身份是别的账号（历史污染）
        std::fs::create_dir_all(profiles.join("1335050000000000")).unwrap();
        std::fs::write(
            profiles.join("1335050000000000.meta.json"),
            r#"{"slot":"1335050000000000","savedAtMs":1,"detectedUid":"4487568582777872"}"#,
        )
        .unwrap();
        let m2 = save_reject_message(&profiles, "1335050000000000", "4487568582777872");
        assert!(m2.contains("污染"), "m2={m2}");
        assert!(m2.contains("退出登录并手动重新登录"), "m2={m2}");

        // ③ 槽位存在、无 sidecar / sidecar 身份一致 → 通用文案
        std::fs::create_dir_all(profiles.join("2117003799429594")).unwrap();
        let m3 = save_reject_message(&profiles, "2117003799429594", "4487568582777872");
        assert!(m3.contains("已拒绝保存"), "m3={m3}");
        assert!(m3.contains("确认登录身份"), "m3={m3}");
        // sidecar 身份与槽位一致 → 不走污染文案
        std::fs::write(
            profiles.join("2117003799429594.meta.json"),
            r#"{"slot":"2117003799429594","savedAtMs":1,"detectedUid":"2117003799429594"}"#,
        )
        .unwrap();
        let m4 = save_reject_message(&profiles, "2117003799429594", "4487568582777872");
        // 通用文案含「覆盖污染」字样，污染专案文案的特征是「疑似已被…污染」+「退出登录并手动重新登录」
        assert!(!m4.contains("疑似已被"), "m4={m4}");
        assert!(!m4.contains("反复操作无法自愈"), "m4={m4}");
        let _ = std::fs::remove_dir_all(&base);
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
            machine_id_override: None,
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
                machine_id_override: None,
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
                machine_id_override: None,
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

    // ── bootstrap_snapshot_from_token_store（issue #78 方案 A）─────────────────

    const BOOT_SLOT: &str = "wb-abc123";
    /// 工具侧凭证副本明文记录（与 save_token_store 写入形态同构：DB 明文路径，
    /// vault 无记录时保留明文，token_store_load_secure 可直接读出）
    const BOOT_REC: &str = r#"{"account":{"uid":"real-uuid-1"},"auth":{"accessToken":"tok-a","refreshToken":"tok-r","expiresAtMs":1760000000000,"refreshExpiresAtMs":1790000000000},"domain":"example.com"}"#;

    /// bootstrap 测试用 Session：全部 IO 路径注入纳秒级临时目录（store db、
    /// profiles_dir、auth 文件均不触碰真实机器路径）
    fn bootstrap_session(tag: &str, app: TargetApp) -> Session {
        let base = std::env::temp_dir().join(format!(
            "sw-boot-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let args = RunArgs {
            action: Action::Switch,
            target_app: app,
            user_id: Some(BOOT_SLOT.into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        let mut sess = Session::new(&args);
        sess.prof.data_dir = base.join("appdata");
        let fake_auth = base.join("fake-auth");
        sess.auth_dir = fake_auth;
        sess.auth_file = base.join("fake-auth").join("workbuddy-desktop.info");
        sess
    }

    /// 无记录 / 记录无 access_token / CodeBuddy 三类拒绝：false 且不建槽
    #[test]
    fn bootstrap_无记录或无access_token或codebuddy_拒绝() {
        let _guard = test_io_lock();
        let sink = MemSink::new();
        // ① token store 无该账号记录 → false
        let sess = bootstrap_session("norec", TargetApp::WorkBuddy);
        assert!(!bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink));
        assert!(!sess.prof.profiles_dir.join(BOOT_SLOT).exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
        // ② 记录存在但无 access_token → false
        let sess = bootstrap_session("notok", TargetApp::WorkBuddy);
        crate::store::docs::wb_token_store_upsert(
            &crate::store::db(&sess.data_dir),
            BOOT_SLOT,
            &serde_json::json!({"account": {"uid": "real-uuid-1"}}),
        )
        .unwrap();
        assert!(!bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink));
        assert!(!sess.prof.profiles_dir.join(BOOT_SLOT).exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
        // ③ CodeBuddy：即便记录齐全也不合成（登录真源在客户端加密 vscdb）
        let sess = bootstrap_session("cb", TargetApp::CodeBuddy);
        crate::store::docs::wb_token_store_upsert(
            &crate::store::db(&sess.data_dir),
            BOOT_SLOT,
            &serde_json::from_str::<serde_json::Value>(BOOT_REC).unwrap(),
        )
        .unwrap();
        assert!(!bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink));
        assert!(!sess.prof.profiles_dir.join(BOOT_SLOT).exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 明文凭证记录 + 池条目 → 合成成功：uid 以池条目为准（uuid 权威体系），
    /// 槽内 auth 文件可读出池 uid，meta 带 bootstrap 标记，sink 留 Ok 步骤
    #[test]
    fn bootstrap_明文记录加池条目_合成成功且uid以池为准() {
        let _guard = test_io_lock();
        let sess = bootstrap_session("ok", TargetApp::WorkBuddy);
        crate::store::docs::wb_token_store_upsert(
            &crate::store::db(&sess.data_dir),
            BOOT_SLOT,
            &serde_json::from_str::<serde_json::Value>(BOOT_REC).unwrap(),
        )
        .unwrap();
        crate::store::docs::wb_pool_save(
            &crate::store::db(&sess.data_dir),
            &serde_json::json!({"accounts": [{"id": BOOT_SLOT, "uid": "pool-uuid-9"}]}),
        )
        .unwrap();
        let sink = MemSink::new();
        assert!(bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink));
        let slot = sess.prof.profiles_dir.join(BOOT_SLOT);
        let j: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(slot.join("auth").join("workbuddy-desktop.info")).unwrap(),
        )
        .unwrap();
        assert_eq!(j["account"]["uid"], "pool-uuid-9");
        assert_eq!(j["auth"]["accessToken"], "tok-a");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(slot.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["uid"], "pool-uuid-9");
        assert_eq!(meta["bootstrap"], true);
        assert!(sink
            .steps
            .lock()
            .unwrap()
            .iter()
            .any(|(s, st, _)| s == "bootstrap" && st == "ok"));
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 池缺失 → 回退凭证记录内的 uid（兜底链路）
    #[test]
    fn bootstrap_池缺失_凭证uid兜底() {
        let _guard = test_io_lock();
        let sess = bootstrap_session("fallback", TargetApp::WorkBuddy);
        crate::store::docs::wb_token_store_upsert(
            &crate::store::db(&sess.data_dir),
            BOOT_SLOT,
            &serde_json::from_str::<serde_json::Value>(BOOT_REC).unwrap(),
        )
        .unwrap();
        assert!(bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &MemSink::new()));
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                sess.prof
                    .profiles_dir
                    .join(BOOT_SLOT)
                    .join("meta.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(meta["uid"], "real-uuid-1");
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }
}
