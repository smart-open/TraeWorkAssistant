//! 登录态切换器（原 trae-switch-bridge.ps1 全量 Rust 化入口）。
//!
//! 架构变更（vs PS 桥）：进度从「powershell 子进程 stdout NDJSON 管道」收敛为
//! 进程内 [`ProgressSink`] 回调——消除 GBK/OEM 乱码根因、CREATE_NO_WINDOW、
//! stderr 防死锁线程、done 探测兜底与 ~300-800ms 进程冷启动。
//!
//! 红线对齐：
//! - 前端零改动：NDJSON 行 `{stage,status,message,time}` 与 `*-done {success,raw}`
//!   语义逐字段兼容，全部 stage 消息文案逐字保留；
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
    /// 2026-10-16：BuddyAI 国际版 WorkBuddy AI 客户端（authfile 布局变体：
    /// 数据目录 ~/.workbuddy-ai，auth 文件同目录异名 workbuddy-desktop-ai.info；
    /// 池为 buddy_ai_accounts，凭证合成走 buddy-ai token store）
    WorkBuddyAI,
    /// 2026-10-16：BuddyAI 国际版 CodeBuddy AI 客户端（与国内 CodeBuddy CN 为
    /// 两个独立安装：国际 = Programs\CodeBuddy + %APPDATA%\CodeBuddy；国内 =
    /// Programs\CodeBuddy CN + %APPDATA%\CodeBuddy CN）。登录真源同为客户端
    /// vscdb（国际 roaming 目录），槽目录独立 profiles_codebuddy_ai
    CodeBuddyAI,
}

impl TargetApp {
    /// PS ValidateSet 对译：未知值返回 Err（调用方为自家前端恒传合法值）。
    /// 2026-10-14 Trae Work 与 Trae CN 合并为单一客户端后退役 TraeWork 变体，
    /// 原「未知值回退 TraeWork」兜底随之移除，不留兼容别名（存量持久化字符串
    /// 由 store 迁移一次性改写）
    pub fn parse(s: &str) -> Result<TargetApp, String> {
        match s {
            "Trae" => Ok(TargetApp::Trae),
            "Doubao" => Ok(TargetApp::Doubao),
            "WorkBuddy" => Ok(TargetApp::WorkBuddy),
            "CodeBuddy" => Ok(TargetApp::CodeBuddy),
            "Qoder" => Ok(TargetApp::Qoder),
            "QoderWork" => Ok(TargetApp::QoderWork),
            // buddy-ai 国际版：WorkBuddyAI / CodeBuddyAI 均为独立档案
            //（CodeBuddyAI 修复：此前归一国内 CodeBuddy 档案导致切换走了国内
            // CN 客户端；两客户端 exe/进程/roaming 数据目录均不同，见 profile.rs）
            "WorkBuddyAI" => Ok(TargetApp::WorkBuddyAI),
            "CodeBuddyAI" => Ok(TargetApp::CodeBuddyAI),
            // Err 文案带合法值清单 + 旧值指引（审查 P3）：调用方（CLI/持久化
            // 读取）只报「未知应用类型」时无从排障，TraeWork 旧值需指路
            other => Err(format!(
                "未知应用类型: {other}（合法值: Trae | Doubao | WorkBuddy | CodeBuddy | WorkBuddyAI | CodeBuddyAI | Qoder | QoderWork；TraeWork 已于 v3.7.12 并入 Trae，存量数据由启动迁移自动改写，外部配置请改存为 Trae）"
            )),
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
    /// 合成快照的真实 uid（会话暂存，与落槽 meta.json 的 uid 同源）：meta.json
    /// 写失败不阻塞合成结果（仅告警），confirm_switch 期望 uid 回退链
    /// meta.json → 此值 → 槽名——槽名 wb-<hash> 是池 id ≠ uuid 体系真实 uid，
    /// 无此暂存时 meta 写失败场景三信号确认恒不命中、每次切换空转 30 秒
    pub bootstrap_uid: Option<String>,
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
            // 国际版 WorkBuddy AI：同目录异名 auth 文件（workbuddy-desktop-ai.info），
            // 与国内 WorkBuddy 的 workbuddy-desktop.info 共存互不干扰
            auth_file: match args.target_app {
                TargetApp::WorkBuddyAI => authfile::wb_ai_auth_file(),
                // 审查修复（M7）：国际版 CodeBuddy AI 用独立 auth 文件，避免把
                // 国内 WorkBuddy auth 备份进国际槽位造成语义污染
                TargetApp::CodeBuddyAI => authfile::cb_ai_auth_file(),
                _ => authfile::wb_auth_file(),
            },
            detected_live_uid: None,
            expected_current_uid: args.expected_current_uid.clone(),
            bootstrap_done: false,
            bootstrap_uid: None,
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
pub fn glob_match_ci(pattern: &str, text: &str) -> bool {
    glob_match_ci_inner(pattern, text)
}

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

/// 当前登录态回写标记槽的决策（纯函数，供 switch_flow 与单测；2026-10-14 审查
/// 修复抽出）。`probed_uid` 为 stop 后权威重探的 uid（hybrid 值或空），
/// `not_logged_in` = 三态探测判 SessionNoUid。
#[derive(Debug, PartialEq, Eq)]
enum CurrentWriteback {
    /// 探测 uid 与标记账号一致 → 回写该账号槽
    Backup(String),
    /// 客户端本次运行未登录 → 绝不回写（防未登录现场毁槽；优先级最高，
    /// 即便过期 hybrid 值恰好等于标记账号也不回写）
    SkipNotLoggedIn,
    /// 探测 uid 与标记不一致或无探测值 → 跳过（防误覆盖）
    SkipMismatch,
}

fn classify_current_writeback(probed_uid: &str, not_logged_in: bool, cur: &str) -> CurrentWriteback {
    if not_logged_in {
        return CurrentWriteback::SkipNotLoggedIn;
    }
    if !probed_uid.is_empty() && probed_uid == cur {
        return CurrentWriteback::Backup(cur.to_string());
    }
    CurrentWriteback::SkipMismatch
}

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
/// 写进 4487 槽）。仅 Trae（icube 布局）有可靠日志数据源。
///
/// 必须在 stop_app 之后（日志已完整落盘）、backup_current 之前（后者内部
/// rotate_bak 会覆盖槽位，拦截必须发生在任何写操作前）调用。探测与目标不符 →
/// 重启客户端还原现场并拒绝，绝不 rotate。
/// L2 保存身份守卫（SaveCurrentLogin/BackupCurrent 流内，stop 后执行）：探测客户端
/// 当前登录账号，与目标槽位不一致 → 拒绝保存并重启客户端还原现场，防 A 的登录态
/// 覆盖污染 B 的槽位（icube 布局 1335/4487 同型事故）。
/// live 探测按布局分派（2026-10-02 审查重构）：
/// - icube：命令层预探测优先（Qoder 保存链 vscdb secret://userInfo 解密传入，唯一
///   可靠源）；空则日志探测回退——仅 Trae（Qoder 池 id 为 qd-<hex> 形态，
///   与日志数字 uid 永不相等，回退会把「格式不同」误判为「登录了别的账号」恒拒保存）
/// - electron_root（QoderWork）：预探测（命令层 live_work_account_id 解密 auth.v1.dat
///   user.id；2026-10-04 实测修正——Cookies 库无 qoderuid cookie，原探测恒空守卫失效）
///   非空才校验；空（未登录/文件缺失/解密失败）= fail-open 放行
/// - 其余布局（chromium/authfile）无 icube 型身份防线，直接放行
fn save_identity_guard(
    sess: &mut Session,
    uid: &str,
    sink: &dyn ProgressSink,
) -> Result<(), String> {
    let probe = match sess.prof.layout {
        Layout::Icube => {
            if !sess.expected_current_uid.is_empty() {
                Some(icube::LiveUidProbe::Uid(sess.expected_current_uid.clone()))
            } else if sess.target == TargetApp::Trae {
                Some(icube::probe_live_uid(&sess.prof.data_dir))
            } else {
                None
            }
        }
        Layout::ElectronRoot => (!sess.expected_current_uid.is_empty())
            .then(|| icube::LiveUidProbe::Uid(sess.expected_current_uid.clone())),
        _ => return Ok(()),
    };
    // Trae 未登录三态（2026-10-14 合并版实测）：最新会话日志存在但无任何带 uid 的
    // 请求 = 客户端本次运行期间未登录——fail-closed 拒绝保存。原 fail-open 会把
    // 未登录现场写进目标槽位（槽位毁灭：之后「切换」到该账号恒恢复出未登录态）。
    if matches!(probe, Some(icube::LiveUidProbe::SessionNoUid)) {
        let msg = format!(
            "客户端当前未登录（最新会话日志无登录请求记录），已拒绝保存，\
             避免把未登录现场写入账号 {uid} 的快照；请先在客户端登录账号 {uid} 后再「保存当前登录态」。"
        );
        // 审查复审修复：不在守卫内还原现场（原此处 start_app + 外层 W10 Err 分支
        // start_app 会双重 spawn 客户端）——被 stop 的客户端统一由外层 Err 分支拉回
        sink.step("fatal", StepStatus::Error, &format!("失败: {msg}"));
        return Err(fatal_line(&format!("[fatal] {msg}")));
    }
    let live = match probe {
        Some(icube::LiveUidProbe::Uid(live)) => Some(live),
        _ => None,
    };
    match live {
        Some(live) if live != uid => {
            // issue #55 审查修复：文案按槽位状态分流（首存指引/污染自愈指引/通用），
            // 不再一律指回「切换」（对无快照账号不可执行、对污染槽位是死循环）
            let msg = save_reject_message(&sess.prof.profiles_dir, uid, &live);
            // 审查复审修复：同上，还原现场统一由外层 Err 分支负责，守卫内不再
            // start_app（避免双重 spawn）
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
    crate::commands::profile::invalidate_profile_stats(&slot_dir, &sess.prof.data_dir);
    r
}

fn restore_profile(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    // 恢复项计数（Switch 恢复后校验用）：每次恢复先重置为 -1；icube/electron_root
    // 结尾写实际值。校验处用 -le 0 拦截：等价于 0 项拷贝=快照空/损坏；-1 作为防御一并拦下。
    sess.last_restored_count = -1;
    // F-68：icube 布局（Trae/Qoder）恢复前抽出 state.vscdb 的两个全局键
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
            // 审查修复（W10）：save/backup 失败 `?` 直接传播会把停在关机状态的
            // 客户端丢给用户——失败时先尽力拉回客户端再传播原始错误
            match save_identity_guard(&mut sess, uid.trim(), sink)
                .and_then(|()| backup_current(&sess, uid.trim(), sink))
            {
                Ok(()) => {
                    set_current_account(&sess, uid.trim());
                    // 审查修复：保存已完成，重启失败仅 Warn 提醒手动打开后照常 done
                    // 收尾——不回滚、不整体报错（报错会诱导用户重试造成重复 save/backup；
                    // 与失败分支的还原现场 Warn 处理对称）
                    if let Err(se) = proc::start_app(&mut sess, sink) {
                        sink.step(
                            "start",
                            StepStatus::Warn,
                            &format!("客户端重启失败，请手动打开: {se}"),
                        );
                    }
                    done(sink, &format!("已保存账号 {} 的当前登录态", uid.trim()))
                }
                Err(e) => {
                    // W10：save/backup 失败（含守卫拒绝）时把被 stop 的客户端拉回；
                    // 还原现场统一在此收敛，避免与守卫内部双重 spawn。失败仅 Warn
                    if let Err(se) = proc::start_app(&mut sess, sink) {
                        sink.step(
                            "guard",
                            StepStatus::Warn,
                            &format!("客户端重启失败，请手动打开: {se}"),
                        );
                    }
                    Err(e)
                }
            }
        }
        Action::BackupCurrent => {
            // 审查修复：与 SaveCurrentLogin 对齐——先关客户端再读 auth/leveldb/vscdb，
            // 防止文件锁下拷贝静默缺文件生成"看似成功"的坏快照；备份完成后拉回
            proc::stop_app(&mut sess, sink)?;
            // 审查修复（W10）：同上，失败时先尽力拉回客户端再传播原始错误
            match save_identity_guard(&mut sess, uid.trim(), sink)
                .and_then(|()| backup_current(&sess, uid.trim(), sink))
            {
                Ok(()) => {
                    set_current_account(&sess, uid.trim());
                    // 审查修复：同 SaveCurrentLogin——备份已完成，重启失败仅 Warn
                    // 提醒手动打开后照常 done 收尾，不回滚、不整体报错
                    if let Err(se) = proc::start_app(&mut sess, sink) {
                        sink.step(
                            "start",
                            StepStatus::Warn,
                            &format!("客户端重启失败，请手动打开: {se}"),
                        );
                    }
                    done(sink, "备份完成")
                }
                Err(e) => {
                    // W10：同 SaveCurrentLogin，还原现场统一在此收敛（含守卫拒绝）
                    if let Err(se) = proc::start_app(&mut sess, sink) {
                        sink.step(
                            "guard",
                            StepStatus::Warn,
                            &format!("客户端重启失败，请手动打开: {se}"),
                        );
                    }
                    Err(e)
                }
            }
        }
        Action::RestoreOnly => {
            // 与 Switch 的区别：只做「以目标快照覆盖当前」，不把当前登录态备份到
            // 原账号槽。当前登录态仍会先备份到 "last" 槽（安全回退）。
            proc::stop_app(&mut sess, sink)?;
            backup_current(&sess, "last", sink)?;
            restore_profile(&mut sess, uid.trim(), sink)?;
            apply_fingerprint_override(&sess, sink);
            set_current_account(&sess, uid.trim());
            // 审查修复（L11）：恢复已完成，start_app 失败不应 `?` 整体报错掩盖恢复
            // 成果——对齐 SaveCurrentLogin/BackupCurrent Ok 分支：Warn 提示手动打开
            if let Err(se) = proc::start_app(&mut sess, sink) {
                sink.step("start", StepStatus::Warn, &format!("客户端重启失败，请手动打开: {se}"));
            }
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
                    if !sess.prof.data_dir.join(f).exists() {
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
/// 返回 Some(真实 uid) = 合成成功（预检放行，继续切换）；None = 无法合成（调用方
/// fatal）。uid 与落槽 meta.json 的 uid 同源，暂存会话供 confirm_switch 在 meta
/// 写失败时回退（槽名是池 id ≠ 真实 uid，不能作确认依据）。
fn bootstrap_snapshot_from_token_store(
    sess: &Session,
    uid: &str,
    sink: &dyn ProgressSink,
) -> Option<String> {
    // 仅 authfile 布局的 WorkBuddy 系可合成（CodeBuddy 登录真源在客户端加密
    // vscdb，工具侧无法离线合成；国际 WorkBuddy AI 走 buddy-ai token store）
    if !matches!(sess.prof.app_name, "WorkBuddy" | "WorkBuddy AI") {
        return None;
    }
    sink.step(
        "bootstrap",
        StepStatus::Info,
        &format!("目标账号 {uid} 无快照，尝试用工具侧凭证副本合成最小登录态…"),
    );
    // 国际版凭证在 buddy-ai token store（独立表 + buddy-ai vault 命名空间）
    let store = if sess.target == TargetApp::WorkBuddyAI {
        crate::tasks::wb_common::buddy_ai_token_store_load(&sess.data_dir)
    } else {
        crate::tasks::wb_common::token_store_load_secure(&sess.data_dir)
    };
    let Some(rec) = store.get("tokens").and_then(|t| t.get(uid)) else {
        sink.step(
            "bootstrap",
            StepStatus::Warn,
            &format!(
                "凭证副本（token store）中无账号 {uid} 的记录，无法合成。\
                 请在客户端手动登录账号 {uid} 后点「保存当前登录态」建立首个快照"
            ),
        );
        return None;
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
        return None;
    }
    // 真实 uid 以账号池条目为准（uid 是 uuid 体系，槽名 wb-<hash> 只是池 id）；
    // 池缺失时回退凭证记录里的 uid。国际版查 buddy-ai 独立池
    let pool = if sess.target == TargetApp::WorkBuddyAI {
        crate::store::docs::buddy_ai_pool_load(&crate::store::db(&sess.data_dir))
    } else {
        crate::store::docs::wb_pool_load(&crate::store::db(&sess.data_dir))
    };
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
        return None;
    };
    let mut creds = creds;
    creds.uid = real_uid;
    authfile::synthesize_auth_snapshot(sess, uid, &creds, sink)
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
        let bootstrap_uid = if sess.prof.layout == Layout::Authfile {
            bootstrap_snapshot_from_token_store(sess, uid, sink)
        } else {
            None
        };
        let Some(bootstrap_uid) = bootstrap_uid else {
            let msg = match (sess.prof.layout, sess.prof.app_name) {
                (Layout::Authfile, "CodeBuddy" | "CodeBuddy AI") => format!(
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
        };
        sess.bootstrap_uid = Some(bootstrap_uid);
        sess.bootstrap_done = true;
    }
    proc::stop_app(sess, sink)?;

    // 保存当前登录态到 "last" 槽位（安全备份）
    backup_current(sess, "last", sink)?;

    // 权威重探（2026-10-14 合并版实测增强）：stop 后最新会话日志已完整，Trae 对
    // live uid 重探三态分派——
    // - Uid：权威证据，覆盖 pre-stop 探测值；
    // - SessionNoUid：客户端本次运行期间未登录（会话日志无任何带 uid 的请求），
    //   禁止回写标记槽——实测 093318 会话（恢复 2803 后客户端 21 秒内未登录退出）
    //   曾因 fail-open 把未登录现场刷进 2803 槽，造成「切回来恒未登录」的槽位毁灭；
    // - NoEvidence：维持 pre-stop 探测值（原 fail-open 语义，全新安装等场景）。
    let mut expected_uid = expected_uid.to_string();
    let mut not_logged_in = false;
    if sess.target == TargetApp::Trae {
        match icube::probe_live_uid(&sess.prof.data_dir) {
            icube::LiveUidProbe::Uid(u) => expected_uid = u,
            // 清空 pre-stop hybrid 值：它可能恰为标记账号（上次切换失败残留），
            // 不清空会让下方第一分支命中、未登录现场照样刷进标记槽（实测 2803 槽
            // 污染复刻路径）。SessionNoUid = 权威证据「本次运行未登录」，
            // pre-stop 的任何推断值一律作废
            icube::LiveUidProbe::SessionNoUid => {
                not_logged_in = true;
                expected_uid.clear();
            }
            icube::LiveUidProbe::NoEvidence => {}
        }
    }

    // 防误覆盖守卫：仅当桌面端检测到的当前登录（expected_uid）与标记文件一致时才
    // 回写账号槽。不一致/未检测到 = 客户端很可能已手动重登或处于未登录态，此时把
    // "当前态"刷进标记槽会把错误内容覆盖掉该账号的快照（实测 B 被覆盖根因），
    // 故跳过并警告——last 槽始终有完整备份可回退。
    if let Some(cur) = get_current_account(sess) {
        if cur != uid {
            // 手动登录兜底：客户端被用户手动切成池内另一个账号 X 时，标记仍是旧账号 cur。
            // 旧逻辑只写 last、跳过账号槽，导致 X 的登录态永远存不进它自己的快照。
            // 这里在「探测到的实际登录 ≠ 标记 ≠ 本次目标」且 X 的槽位已存在时，
            // 把当前现场回写到 X 的槽位（身份来自 stop 后权威探测；未入池/无槽位仍走原跳过逻辑）。
            let detected_slot = (!expected_uid.is_empty()
                && expected_uid != uid
                && !not_logged_in
                && (sess.prof.profiles_dir.join(&expected_uid).is_dir()
                    || sess
                        .prof
                        .profiles_dir
                        .join(format!("{expected_uid}.bak"))
                        .is_dir()))
            .then(|| expected_uid.clone());
            if let Some(slot) = detected_slot {
                backup_current(sess, &slot, sink)?;
                sink.step(
                    "backup",
                    StepStatus::Ok,
                    &format!(
                        "检测到客户端实际登录为 {slot}（原标记为 {cur}），已把当前登录态回写到该账号槽位"
                    ),
                );
            } else {
                match classify_current_writeback(&expected_uid, not_logged_in, &cur) {
                    CurrentWriteback::Backup(slot) => {
                        backup_current(sess, &slot, sink)?;
                        sink.step(
                            "backup",
                            StepStatus::Ok,
                            &format!("当前账号 {slot} 的登录态已备份"),
                        );
                    }
                    CurrentWriteback::SkipNotLoggedIn => {
                        sink.step(
                            "backup",
                            StepStatus::Warn,
                            &format!(
                                "客户端本次运行期间未登录（最新会话日志无登录请求记录），已跳过回写该账号槽位，\
                                 防止把未登录现场写入账号 {cur} 的快照（当前态仍备份到 last）"
                            ),
                        );
                    }
                    CurrentWriteback::SkipMismatch => {
                        sink.step(
                            "backup",
                            StepStatus::Warn,
                            &format!(
                                "检测到的当前登录（{}）与标记账号 {cur} 不一致，已跳过回写该账号槽位（当前态仍备份到 last），防止误覆盖",
                                if expected_uid.is_empty() {
                                    "未识别或未检测到登录会话"
                                } else {
                                    &expected_uid
                                }
                            ),
                        );
                    }
                }
            }
        }
    }

    // 恢复目标账号的登录态
    // 审查修复（W9）：主恢复 Err 直接 `?` 传播时，客户端已在前置步骤被 stop 关闭、
    // last 备份已完成——把停在关机状态的现场丢给用户。失败时从 last 槽回滚 +
    // 重启客户端再报 fatal（对齐下方快照无效路径的处理）
    if let Err(e) = restore_profile(sess, uid, sink) {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!("恢复账号 {uid} 登录态失败，将从 last 槽回滚到切换前状态…: {e}"),
        );
        // 审查复审修复（W9）：回滚自身失败时不能只传播回滚错误——原始错误并入
        // 消息，两条错误都可见（原实现原始错误仅留在上方 Warn 行）
        restore_profile(sess, "last", sink).map_err(|re| {
            format!("恢复账号 {uid} 登录态失败（{e}），且回滚到切换前状态也失败: {re}")
        })?;
        // 审查修复（L11）：回滚已完成，start_app 失败不应 `?` 短路丢失回滚上下文
        //——Warn 后继续原 fatal 收尾（回滚结果不掩盖，失败详情仍然可见）
        if let Err(se) = proc::start_app(sess, sink) {
            sink.step("start", StepStatus::Warn, &format!("客户端重启失败，请手动打开: {se}"));
        }
        let msg = format!(
            "恢复账号 {uid} 登录态失败，已回滚到切换前状态并重启客户端；请重试或重新「保存当前登录态」: {e}"
        );
        sink.step("fatal", StepStatus::Error, &msg);
        return Err(fatal_line(&msg));
    }

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
        // 审查修复（L11）：回滚已完成，start_app 失败不应 `?` 短路丢失回滚上下文
        //——Warn 后继续原 fatal 收尾（回滚结果不掩盖，失败详情仍然可见）
        if let Err(se) = proc::start_app(sess, sink) {
            sink.step("start", StepStatus::Warn, &format!("客户端重启失败，请手动打开: {se}"));
        }
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
    // baseline：start 前的最新会话，供切换后身份确认只认「比它更新的会话」
    let baseline = icube::newest_session_dir(&sess.prof.data_dir);
    proc::start_app(sess, sink)?;

    // Trae 登录身份确认（2026-10-14 合并版实测增强）：合并版客户端启动即产生新
    // 会话与 dynamicConfig.log，其 uid 是「实际登录是谁」的直接证据。实测：恢复
    // 被污染的槽位会静默登录为其他账号（2328 槽恢复后实际登录 1448）、恢复失效
    // 会话则停在未登录界面——如实在 done 文案中告知，不再静默报「切换成功」。
    if sess.target == TargetApp::Trae {
        match icube::confirm_switch_uid(
            &sess.prof.data_dir,
            uid,
            baseline,
            std::time::Duration::from_secs(20),
        ) {
            icube::SwitchVerify::Mismatch(actual) => {
                // 桥标记纠偏（2026-10-14 审查修复）：1030 行刚把标记写成 uid，但日志
                // 权威证据表明实际登录为 actual——维持 uid 会让 hybrid 回退把「标记
                // 说 uid」误报为当前登录（保存守卫文案失真、扫描误报已入池）。
                // set_current_account 会同步刷新 switchedAt，hybrid 读侧据此采信
                set_current_account(sess, &actual);
                // 账号列表红色「快照异常」徽标：本次切换未登进目标账号
                icube::mark_snapshot_verify_failed(&sess.prof.profiles_dir, uid);
                sink.step(
                    "switch",
                    StepStatus::Warn,
                    &format!("检测到客户端实际登录为 {actual}，桥标记已同步纠正为 {actual}"),
                );
                return done(
                    sink,
                    &format!(
                        "已切换至账号 {uid}（警告：客户端实际登录为 {actual}，该快照可能已被其他账号污染；\
                         请在客户端手动登录账号 {uid} 后点「保存当前登录态」覆盖修复）"
                    ),
                );
            }
            icube::SwitchVerify::Timeout => {
                // 标记维持 uid：超时可能是 NEED_REFRESH 慢登录（稍后仍会登入 uid），
                // 也可能确实未登录（此时标记过期，但保存侧 L2 三态探测 fail-closed
                // 已兜底，不会据此误写入槽位），无更优单值可写
                // 账号列表红色「快照异常」徽标：本次切换未确认登录（橡皮4 式失效快照）
                icube::mark_snapshot_verify_failed(&sess.prof.profiles_dir, uid);
                return done(
                    sink,
                    &format!(
                        "已切换至账号 {uid}（警告：20 秒内未确认登录身份，该快照的登录会话可能已失效；\
                         若客户端未登录，请手动登录后点「保存当前登录态」重建快照）"
                    ),
                );
            }
            icube::SwitchVerify::Ok => {
                // 确认登录成功 → 清除该槽位的历史验证失败徽标
                icube::clear_snapshot_verify_failed(&sess.prof.profiles_dir, uid);
            }
        }
    }

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
                // 升级条件除当次会话的 bootstrap_done 外，还查槽位持久化 bootstrap
                // 标记（slot_marked_bootstrap）：bootstrap_done 仅当次会话有效，
                // 合成后切换未走完（进程退出/失败重试）的跨会话重试也要能升级
                if sess.bootstrap_done || authfile::slot_marked_bootstrap(sess, uid) {
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
        let mut sess = Session::new(&args);
        save_identity_guard(&mut sess, "qd-x", &MemSink::new()).unwrap();
        assert_eq!(sess.detected_live_uid.as_deref(), Some("qd-x"));
        // fail-open：expected_current_uid 为空（RunArgs 未预探测到登录账号）
        let args2 = RunArgs { expected_current_uid: String::new(), ..args };
        let mut sess2 = Session::new(&args2);
        let sink = MemSink::new();
        save_identity_guard(&mut sess2, "qd-x", &sink).unwrap();
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
        assert!(glob_match_ci("Trae*", "TRAE CN"));
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
        let prof_dir = dir.join("data").join("profiles_trae");
        std::fs::create_dir_all(&prof_dir).unwrap();
        std::fs::write(prof_dir.join("current_account.txt"), "\u{feff}4487568582777872").unwrap();
        let mut args = RunArgs {
            action: Action::KeepAlive,
            target_app: TargetApp::Trae,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: dir.clone(),
        };
        args.target_app = TargetApp::Trae;
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
                target_app: TargetApp::Trae,
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
                target_app: TargetApp::Trae,
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
        let line = step_line("stop", StepStatus::Running, "正在关闭 Trae");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["stage"], "stop");
        assert_eq!(v["status"], "running");
        assert_eq!(v["message"], "正在关闭 Trae");
        assert!(v["time"].as_str().unwrap().contains(':'));
    }

    #[test]
    fn parse_未知值返回err且不设traework别名() {
        assert!(matches!(TargetApp::parse("Trae"), Ok(TargetApp::Trae)));
        assert!(matches!(TargetApp::parse("QoderWork"), Ok(TargetApp::QoderWork)));
        // 合并退役：不允许 "TraeWork" 兼容别名，未知值一律 Err
        assert!(TargetApp::parse("TraeWork").is_err());
        assert!(TargetApp::parse("trae").is_err());
        assert!(TargetApp::parse("").is_err());
    }

    #[test]
    fn 回写决策_未登录优先级最高且过期hybrid值不参与() {
        use CurrentWriteback::{Backup, SkipMismatch, SkipNotLoggedIn};
        // 未登录：即使过期 hybrid 值恰好等于标记账号（2803 槽污染复刻路径），也绝不回写
        assert_eq!(
            classify_current_writeback("2803", true, "2803"),
            SkipNotLoggedIn
        );
        // 探测 uid 与标记一致 → 回写
        assert_eq!(
            classify_current_writeback("2328", false, "2328"),
            Backup("2328".into())
        );
        // 探测值与标记不一致 → 跳过
        assert_eq!(
            classify_current_writeback("1448", false, "2328"),
            SkipMismatch
        );
        // 无探测值（NoEvidence fail-open 维持）→ 跳过
        assert_eq!(classify_current_writeback("", false, "2328"), SkipMismatch);
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

    /// 无记录 / 记录无 access_token / CodeBuddy 三类拒绝：None 且不建槽
    #[test]
    fn bootstrap_无记录或无access_token或codebuddy_拒绝() {
        let _guard = test_io_lock();
        let sink = MemSink::new();
        // ① token store 无该账号记录 → None
        let sess = bootstrap_session("norec", TargetApp::WorkBuddy);
        assert!(bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink).is_none());
        assert!(!sess.prof.profiles_dir.join(BOOT_SLOT).exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
        // ② 记录存在但无 access_token → None
        let sess = bootstrap_session("notok", TargetApp::WorkBuddy);
        crate::store::docs::wb_token_store_upsert(
            &crate::store::db(&sess.data_dir),
            BOOT_SLOT,
            &serde_json::json!({"account": {"uid": "real-uuid-1"}}),
        )
        .unwrap();
        assert!(bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink).is_none());
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
        assert!(bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink).is_none());
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
        // 返回值 = 池条目权威真实 uid（透传自 synthesize_auth_snapshot，与 meta 同源）
        let boot_uid = bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &sink);
        assert_eq!(boot_uid.as_deref(), Some("pool-uuid-9"));
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
        // 池缺失 → 回退凭证记录内的 uid（兜底链路），返回值同为该兜底真实 uid
        let boot_uid = bootstrap_snapshot_from_token_store(&sess, BOOT_SLOT, &MemSink::new());
        assert_eq!(boot_uid.as_deref(), Some("real-uuid-1"));
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
