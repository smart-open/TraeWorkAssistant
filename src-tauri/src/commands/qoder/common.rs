//! Qoder 共享底层（F-80 M1）：账号池/设置读写、账号 id、环境检测。
//!
//! 【跨平台审查 2026-10-03】本模块 Windows 依赖点与 macOS 适配预留一览：
//! - 原生 API：tasklist 进程探测 / creation_flags（`is_running`/`work_running`）；
//! - 路径体系：%LOCALAPPDATA%（exe 候选）、%APPDATA%（IDE 数据目录）、%USERPROFILE%（CLI 目录）；
//! - 解密链路：`live_account_id`/`live_work_account_id` 依赖 DPAPI（经 ide_store）。
//! 全部 Windows 专属实现已按「逐函数 cfg 门控 + 非 Windows 同名占位」隔离，
//! macOS 分支只需替换占位实现，不动调用方。检索标记：`macOS 适配预留`。

use std::path::{Path, PathBuf};
use tauri::State;

use crate::state::AppState;

// ── 数据结构（§5.4 数据模型；对照 WorkBuddyAccount 裁剪）────────────────────

/// 账号池记录。id = "qd-" + sha256(token)[..12]（同 token 稳定同 id，防换发重复入池）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct QoderAccount {
    #[serde(default)]
    pub id: String,
    /// 全家桶 uid（/api/v1/userinfo 回填；PAT 导入时 userinfo 失败可空）
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    /// 手机号掩码（userinfo 未提供时留空，M1 不做猜测）
    #[serde(default)]
    pub phone_masked: String,
    /// free | pro | pro+ | teams（只读提示）；userinfo 未提供时留空
    #[serde(default)]
    pub plan: String,
    /// 凭证来源：pat | ide_store | qoderwork_store | mitm | cli（M1 实装 pat）
    #[serde(default)]
    pub credential_source: String,
    /// token 过期时间（Unix 秒；PAT 长期凭证为 None）
    #[serde(default)]
    pub token_expires_at: Option<i64>,
    #[serde(default)]
    pub needs_relogin: bool,
    #[serde(default)]
    pub relogin_reason: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub note: String,
    /// 余额缓存（credits_fetch 成功后回写）
    #[serde(default)]
    pub credits_balance: Option<f64>,
    #[serde(default)]
    pub credits_fetched_at: Option<String>,
    /// 每账号稳定设备指纹（§5.10 多账号并发；入池时生成一次永不轮换，
    /// 存量账号经 ensure_pool_profiles 惰性回填）
    #[serde(default)]
    pub device_profile: Option<crate::tasks::qoder_device::QoderDeviceProfile>,
}

/// Qoder 设置（kv `qoder_settings`；对照 WorkBuddySettings 裁剪）
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct QoderSettings {
    /// 启动自动补签 + 应用内调度器 qoder-checkin 启用判定（默认开，§5.7）
    #[serde(default = "default_true")]
    pub auto_checkin: bool,
}

fn default_true() -> bool {
    true
}

// ── 账号池 / 设置读写 ───────────────────────────────────────────────────────

pub(crate) fn load_pool(state: &AppState) -> Vec<QoderAccount> {
    // 只读路径容错：逐行解析，损坏行跳过（原实现整组解析失败 → 静默空池）
    load_pool_rows(state).0
}

/// 逐行解析池：返回 (正常账号, 损坏行原始值)。写入路径必须走 load_pool_checked
/// 拒绝损坏池，避免「load 丢行 → save 整池覆盖」静默永久丢账号。
fn load_pool_rows(state: &AppState) -> (Vec<QoderAccount>, Vec<serde_json::Value>) {
    let v = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let Some(rows) = v.get("accounts").and_then(|a| a.as_array()) else {
        return (Vec::new(), Vec::new());
    };
    let mut ok = Vec::new();
    let mut corrupt = Vec::new();
    for r in rows {
        match serde_json::from_value::<QoderAccount>(r.clone()) {
            Ok(a) => ok.push(a),
            Err(_) => corrupt.push(r.clone()),
        }
    }
    (ok, corrupt)
}

/// 严格版池读（供 with_pool_mut 写路径把关）：存在损坏行时先把损坏行原文备份到
/// data_dir/qoder_pool.corrupt.json，再拒绝返回——整池覆盖前必须显式处理。
pub(crate) fn load_pool_checked(state: &AppState) -> Result<Vec<QoderAccount>, String> {
    let (accounts, corrupt) = load_pool_rows(state);
    if corrupt.is_empty() {
        return Ok(accounts);
    }
    let backup = state.data_dir.join("qoder_pool.corrupt.json");
    let _ = crate::fs_utils::write_json(&backup, &serde_json::json!({ "corrupt_rows": corrupt }));
    crate::fs_utils::app_log(
        &state.data_dir,
        "qoder 账号池存在损坏行，已备份到 qoder_pool.corrupt.json，拒绝整池覆盖以防丢账号",
    );
    Err(
        "账号池数据存在损坏行，已备份到 qoder_pool.corrupt.json；本次修改已取消以保护其余账号，请处理备份文件后重试"
            .into(),
    )
}

pub(crate) fn save_pool(state: &AppState, accounts: &[QoderAccount]) -> Result<(), String> {
    let v = serde_json::to_value(accounts).map_err(|e| format!("序列化失败: {e}"))?;
    crate::store::docs::qoder_pool_save(
        &crate::store::db(&state.data_dir),
        &serde_json::json!({ "accounts": v }),
    )
}

/// 池读-改-写互斥执行（F-80 I09）：OAuth/PAT/IDE 导入、改名/移除、签到回写、
/// 指纹回填等路径并发时整池覆盖会丢更新，统一经此函数持锁执行。
/// 注意：tasks 层 sync_pool_expiry/ensure_pool_profiles 直操原始 JSON 保留未知字段，
/// 不可走本函数（会解析成 Vec<QoderAccount> 丢字段），须自行持 state.qoder_pool_lock。
pub(crate) fn with_pool_mut<T>(
    state: &AppState,
    f: impl FnOnce(&mut Vec<QoderAccount>) -> Result<T, String>,
) -> Result<T, String> {
    // 中毒锁恢复（into_inner）：单次 panic 不应永久阻塞后续池写
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut accounts = load_pool_checked(state)?;
    let out = f(&mut accounts)?;
    save_pool(state, &accounts)?;
    Ok(out)
}

pub(crate) fn load_settings(state: &AppState) -> QoderSettings {
    crate::store::db(&state.data_dir).kv_get("qoder_settings")
}

#[tauri::command]
pub fn qoder_settings_get(state: State<AppState>) -> QoderSettings {
    load_settings(&state)
}

#[tauri::command]
pub fn qoder_settings_set(state: State<AppState>, patch: QoderSettings) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("qoder_settings", &patch)
}

// ── 账号 id（qd- + sha256[..12]，单一实现在 tasks::qoder_common，此处 re-export）──

pub(crate) use crate::tasks::qoder_common::account_id_of;

/// 账号绑定 machine_id（F-80 §5.10.2 本地存储覆写取值源）：账号池按 id 查
/// device_profile.machine_id；无档案或值为空 → None（调用方跳过覆写）
pub(crate) fn machine_id_of(state: &AppState, account_id: &str) -> Option<String> {
    load_pool(state)
        .into_iter()
        .find(|a| a.id == account_id)
        .and_then(|a| a.device_profile)
        .map(|p| p.machine_id)
        .filter(|m| !m.is_empty())
}

/// F-80 §5.10 守卫数据源（切换/保存共用，2026-10-02 审查从 switch.rs 内联收编）：
/// Qoder（icube 布局）当前登录真源 = IDE state.vscdb secret://userInfo（os_crypt 解密，
/// 与 ide_store 扫描同链路；Windows=DPAPI+AES-GCM / mac=Keychain+AES-CBC，2026-10-05
/// mac 实装）→ uid 在池反查账号 id。uid 在池外时原样返回（守卫消息如实提示
/// 「与标记账号不一致」，uid 与 qd- 池 id 无碰撞）；未登录/解密失败 → None
///（调用方 fail-open：仅跳过回写/守卫放行，不阻断流程）。
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn live_account_id(state: &AppState) -> Option<String> {
    let uid = ide_data_dir()
        .and_then(|dir| super::ide_store::scan_ide_login(&dir).ok())
        .map(|l| l.uid)
        .filter(|u| !u.is_empty())?;
    Some(
        load_pool(state)
            .into_iter()
            .find(|a| a.uid == uid)
            .map(|a| a.id)
            .unwrap_or(uid),
    )
}

/// 非 Windows/macOS 平台：Qoder 守卫无数据源（占位桩，发布域外）
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn live_account_id(_state: &AppState) -> Option<String> {
    None
}

/// Qoder Work 守卫数据源（2026-10-04 修正）：Work 客户端登录真源 = 数据目录根级
/// auth.v1.dat（"v10" os_crypt 密文 JSON，user.id 与池账号 uid 同源）→ 池反查账号 id。
/// 原 Cookies qoderuid 探测已证伪（Work Cookies 库不存在该 cookie，恒 None → 守卫
/// 恒 fail-open、来源槽永不回写）。uid 在池外原样返回；文件缺失/解密失败 → None
/// （fail-open：保存守卫放行、切换来源槽不回写）。
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn live_work_account_id(state: &AppState) -> Option<String> {
    let data_dir =
        crate::switcher::profile::profile_for(crate::switcher::TargetApp::QoderWork, &state.data_dir)
            .data_dir;
    let uid = super::ide_store::scan_work_login_uid(&data_dir)?;
    Some(
        load_pool(state)
            .into_iter()
            .find(|a| a.uid == uid)
            .map(|a| a.id)
            .unwrap_or(uid),
    )
}

/// 非 Windows/macOS 平台：Qoder Work 守卫无数据源（占位桩，发布域外）
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn live_work_account_id(_state: &AppState) -> Option<String> {
    None
}

/// 本机双端当前登录账号（账号管理「登录中」徽标数据源，对齐 Trae localEntitlement /
/// Buddy is_current 徽标）：IDE = state.vscdb secret://userInfo 解密（live_account_id），
/// Work = auth.v1.dat 解密（live_work_account_id）。解密失败/未登录 → null（fail-open，
/// 前端不展示徽标）。
#[derive(serde::Serialize, Clone, Default)]
pub struct QoderLiveLogins {
    /// Qoder IDE 当前登录的账号 id（池反查；uid 在池外时为原样 uid）
    pub ide: Option<String>,
    /// Qoder Work 当前登录的账号 id（池反查；uid 在池外时为原样 uid）
    pub work: Option<String>,
}

#[tauri::command]
pub async fn qoder_live_logins(state: State<'_, AppState>) -> Result<QoderLiveLogins, String> {
    // DPAPI + vscdb SQLite/文件 IO 为阻塞操作：spawn_blocking 移出 async worker
    //（审查 P3 修复，对齐 qoder_accounts_list 惯例；join 失败回退全 None = 不展示徽标）
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || QoderLiveLogins {
        ide: live_account_id(&st).filter(|s| !s.is_empty()),
        work: live_work_account_id(&st).filter(|s| !s.is_empty()),
    })
    .await
    .map_err(|e| format!("登录状态读取任务失败: {e}"))
}

// ── 环境检测（M0 侦察结论固化为候选路径；M0 R-1/R-2 缺口闭合后扩展）─────────

/// 0.4.3+ 拆分形态检测：真 IDE 已独立安装于 `%LOCALAPPDATA%\Programs\Qoder CN IDE\`
/// macOS 适配预留：对应形态需实测（预期为 `/Applications/Qoder CN IDE.app` 是否存在；
/// Windows 用 exe 存在性判定，macOS 改为 `.app` bundle 目录存在性判定，函数签名不变）
pub(crate) fn ide_split_installed() -> bool {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|l| {
            PathBuf::from(l)
                .join("Programs")
                .join("Qoder CN IDE")
                .join("Qoder CN IDE.exe")
                .exists()
        })
        .unwrap_or(false)
}

/// 过期 IDE 手动路径：指向 `Programs\Qoder CN\Qoder CN.exe`（0.4.3 前该路径是
/// IDE，现在语义已是 Work 本体）。拆分形态已装时判定过期，忽略手动值走默认候选
fn is_stale_ide_manual(p: &Path) -> bool {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|l| {
            p == PathBuf::from(l)
                .join("Programs")
                .join("Qoder CN")
                .join("Qoder CN.exe")
        })
        .unwrap_or(false)
}

/// Work 本体已安装检测（0.4.3+ 拆分形态 `%LOCALAPPDATA%\Programs\Qoder CN\`）
fn work_body_installed() -> bool {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|l| {
            PathBuf::from(l)
                .join("Programs")
                .join("Qoder CN")
                .join("Qoder CN.exe")
                .exists()
        })
        .unwrap_or(false)
}

/// mac 应用检索根（与 icube_auth::mac_app_version / env 定位链同源双根）
#[cfg(target_os = "macos")]
fn mac_app_bases() -> Vec<PathBuf> {
    let mut bases = vec![PathBuf::from("/Applications")];
    let home = crate::platform::home_dir();
    if !home.as_os_str().is_empty() {
        bases.push(home.join("Applications"));
    }
    bases
}

/// IDE exe 候选：settings.qoder_ide_path 人工指定优先，否则按平台默认安装布局。
/// Windows 实测（2026-09-30，Qoder CN 0.4.3 起 IDE 与 Work 拆分安装目录）：
/// 真 IDE = `%LOCALAPPDATA%\Programs\Qoder CN IDE\Qoder CN IDE.exe`（VS Code fork），
/// `Programs\Qoder CN\Qoder CN.exe` 已是 Work 本体（Electron），仅作旧版一体化形态兼容；
/// `.qoder-versions\<ver>\` 形态为国际版布局，一并保留兼容。
/// macOS 实装（2026-10-05 实测）：候选为 /Applications、~/Applications 双根下的
/// `Qoder CN IDE.app`（bundle 目录本身，消费方全链路按 bundle 工作，见函数内注释）。
pub(crate) fn ide_exe_candidates(state: &AppState) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = state.settings().qoder_ide_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            let pb = PathBuf::from(p);
            // 过期手动值降级：拆分形态已装且手动值指向 Work 本体旧语义路径 →
            // 忽略手动值走默认候选（用户曾保存 0.4.3 前语义的 IDE 路径）
            if !(ide_split_installed() && is_stale_ide_manual(&pb)) {
                out.push(pb);
                return out;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        // mac 实装（2026-10-05 实测）：安装形态为 /Applications（或 ~/Applications）
        // 下 Qoder CN IDE.app（VS Code fork，bundle id com.aliyun.lingma.ide）。
        // 候选返回 bundle 目录本身：exists() 判定、open 直启（spawn_first_existing
        // mac 分支）、version_of（.app 上溯读 CFBundleShortVersionString）全链路按
        // bundle 工作；旧版一体化/.qoder-versions 版本化目录为 Windows 布局，无对应形态
        for base in mac_app_bases() {
            out.push(base.join("Qoder CN IDE.app"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let programs = PathBuf::from(&local).join("Programs");
            // 0.4.3+ 拆分形态：真 IDE 独立目录
            out.push(
                programs
                    .join("Qoder CN IDE")
                    .join("Qoder CN IDE.exe"),
            );
            // 旧版一体化形态：Programs\Qoder CN\Qoder CN.exe（现已是 Work 本体，兜底）
            let base = programs.join("Qoder CN");
            out.push(base.join("Qoder CN.exe"));
            // 国际版形态：.qoder-versions\<ver>\Qoder CN.exe（版本化目录枚举，最多 4 个）
            if let Ok(entries) = std::fs::read_dir(base.join(".qoder-versions")) {
                let mut vers: Vec<PathBuf> = entries
                    .into_iter()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                vers.sort();
                for v in vers.into_iter().rev().take(4) {
                    out.push(v.join("Qoder CN.exe"));
                }
            }
        }
    }
    out
}

/// IDE 数据目录（M0 实测修正：Windows=`%APPDATA%\QoderCN`，与 switcher Qoder 档案
/// data_dir 同源）。macOS 实装（2026-10-05 合并审查 + 本机实测证实）：
/// `~/Library/Application Support/QoderCN`——IDE 编译产物 main.js 的 userData 解析
/// 函数 PP() 按 `join(getAppDataPath(), product.nameShort="QoderCN")` 取值（与
/// Windows 同名不同根），经 platform 基根收口（与 profile.rs roaming_dir("QoderCN")
/// 同源）；目录尚未出现在本机仅因 IDE 未启动过，不影响切换灰度（mac_supported=false）
pub(crate) fn ide_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| PathBuf::from(d).join("QoderCN"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(crate::platform::app_support_root_lossy().join("QoderCN"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| PathBuf::from(d).join("QoderCN"))
    }
}

/// Work exe 候选：settings.qoderwork_path 人工指定优先，否则按平台默认安装布局。
/// Windows 实测（2026-09-30，0.4.3）：Work 本体 = `%LOCALAPPDATA%\Programs\Qoder CN\Qoder CN.exe`
/// （Electron，带 Launcher 转发能力），`%LOCALAPPDATA%\Qoder CN\Qoder CN Launcher\` 为
/// 旧版 Launcher 形态兜底。macOS 实装（2026-10-05 实测）：/Applications、~/Applications
/// 双根下 `Qoder CN.app`（bundle 目录本身）；无 Windows Launcher 形态，
/// launcher_version 天然返回 None，版本号走 version_of 读 Info.plist。
pub(crate) fn work_exe_candidates(state: &AppState) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = state.settings().qoderwork_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            let pb = PathBuf::from(p);
            // 过期手动值降级：指向旧版 Launcher 且 Work 本体已安装 → 忽略手动值
            // 走默认候选（用户曾保存 0.4.3 前 Launcher 路径，本体优先）
            let stale_launcher = pb
                .file_name()
                .map(|f| f == "Qoder CN Launcher.exe")
                .unwrap_or(false)
                && work_body_installed();
            if !stale_launcher {
                out.push(pb);
                return out;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for base in mac_app_bases() {
            out.push(base.join("Qoder CN.app"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            out.push(
                PathBuf::from(&local)
                    .join("Programs")
                    .join("Qoder CN")
                    .join("Qoder CN.exe"),
            );
            out.push(
                PathBuf::from(&local)
                    .join("Qoder CN")
                    .join("Qoder CN Launcher")
                    .join("Qoder CN Launcher.exe"),
            );
        }
    }
    out
}

// tasklist/creation_flags 为 Windows 专属（与 ide_store.rs 同款逐函数门控）
// macOS 适配预留：tasklist 探测在 macOS 不可用，见下方 is_running/work_running
// 占位注释——建议统一收敛到 sysinfo 实现后，本 Windows 实现可与 macOS 版共用签名
#[cfg(windows)]
fn proc_running(image: &str) -> bool {
    // 原 tasklist 子进程（单次实测 ~260ms）改为 sysinfo 进程表枚举 + 短 TTL 复用
    // （见 switcher::proc::any_running）；`.exe` 后缀由该函数统一剥离
    crate::switcher::proc::any_running(&[image])
}

#[cfg(windows)]
fn is_running() -> bool {
    // 0.4.3+ 拆分形态装了独立 IDE：只认 IDE 进程（Qoder CN.exe 是 Work 本体，
    // 不能作为 IDE 运行信号）；旧版一体化形态 Qoder CN.exe 即 IDE
    if ide_split_installed() {
        proc_running("Qoder CN IDE.exe")
    } else {
        proc_running("Qoder CN.exe")
    }
}

#[cfg(not(windows))]
fn is_running() -> bool {
    // mac 实装（2026-10-05）：按 exe 路径 bundle 段检测（commands/process.rs
    // mac_app_running，M-1 ⑥ 同款语义）——实测双客户端 CFBundleExecutable 同名
    // "Qoder CN"，映像名无法区分 IDE/Work，bundle 目录名才是身份信号
    #[cfg(target_os = "macos")]
    {
        crate::commands::process::mac_app_running(&["Qoder CN IDE"])
    }
    #[cfg(not(target_os = "macos"))]
    {
        // 其余平台占位：恒 false（不阻断任何流程）
        false
    }
}

#[cfg(windows)]
fn work_running() -> bool {
    // Work 本体（0.4.3+）进程名 Qoder CN.exe；旧版 Launcher 形态兜底
    proc_running("Qoder CN.exe") || proc_running("Qoder CN Launcher.exe")
}

#[cfg(not(windows))]
fn work_running() -> bool {
    // mac 实装（2026-10-05）：Work 本体 = Qoder CN.app（bundle 段检测，与 IDE 的
    // Qoder CN IDE.app 区分；两 exe 同名 "Qoder CN"，映像名比对不可行）
    #[cfg(target_os = "macos")]
    {
        crate::commands::process::mac_app_running(&["Qoder CN"])
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// QoderWork 真实产品版本：Launcher exe 本体未打版本号（ProductVersion 恒 0.0.0），
/// 实际版本由 Launcher 写入同目录 state.ini 的 [launcher] targetVersion（如 0.4.3，
/// 对应 appExecutable=.qoder-versions\0.4.3\Qoder CN.exe）；读不到时回退 install.ini
/// 的 targetVersion，最后由调用方回退 exe 版本探测。
fn launcher_version(launcher_exe: &std::path::Path) -> Option<String> {
    let dir = launcher_exe.parent()?;
    for name in ["state.ini", "install.ini"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("targetVersion=") {
                    let v = v.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 环境检测（环境配置页/概述页/顶栏数据源）：IDE / 数据目录 / CLI / QoderWork。
/// async 命令：内部 tasklist 子进程 + 磁盘探测会阻塞数百毫秒，同步命令会卡死主线程
#[tauri::command(async)]
pub fn qoder_env_check(state: State<AppState>) -> serde_json::Value {
    let exe = ide_exe_candidates(&state)
        .into_iter()
        .find(|p| p.exists());
    let work_exe = work_exe_candidates(&state)
        .into_iter()
        .find(|p| p.exists());
    let data_dir = ide_data_dir();
    let data_dir_str = data_dir.as_ref().map(|p| p.to_string_lossy().to_string());
    // CLI 目录（~/.qoder-cn，路径名跨平台同构）：主目录经 platform::home_dir 收口
    // （Windows=USERPROFILE / mac=HOME）——原直读 USERPROFILE 在 mac 恒缺失，
    // cli_dir_exists 恒 false（2026-10-05 合并审查实装）
    let home = crate::platform::home_dir();
    let cli_dir = home.join(".qoder-cn");
    // 版本号（对齐 Trae/Buddy 顶栏 hover：exe ProductVersion，PowerShell 子进程读取）
    let ide_version = exe
        .as_ref()
        .and_then(|p| crate::commands::env::version_of(&p.to_string_lossy()));
    let work_version = work_exe.as_ref().and_then(|p| {
        // Work 本体 exe 直接读 ProductVersion；旧版 Launcher 形态 ProductVersion 恒 0.0.0，
        // 优先读同目录 state.ini/install.ini 的 targetVersion（launcher_version 对本体
        // 目录读不到 ini 自然回退 exe 版本探测）
        launcher_version(p).or_else(|| crate::commands::env::version_of(&p.to_string_lossy()))
    });
    serde_json::json!({
        "ide_installed": exe.is_some(),
        "ide_running": is_running(),
        "ide_exe": exe.map(|p| p.to_string_lossy().to_string()),
        "ide_version": ide_version,
        "ide_data_dir": data_dir_str,
        "ide_data_dir_exists": data_dir.map(|p| p.exists()).unwrap_or(false),
        "cli_dir": cli_dir.to_string_lossy().to_string(),
        "cli_dir_exists": !home.as_os_str().is_empty() && cli_dir.exists(),
        // QoderWork：Qoder CN 本体（Electron，0.4.3 起与 IDE 拆分，userData=
        // %APPDATA%\com.qodercn.app.stable），exe 装机检测走 Launcher 形态；
        // ide_data_dir（%APPDATA%\QoderCN）专属拆分后的 IDE
        "qoderwork_installed": work_exe.is_some(),
        "qoderwork_running": work_running(),
        "qoderwork_exe": work_exe.map(|p| p.to_string_lossy().to_string()),
        "qoderwork_version": work_version,
    })
}

// ── 打开客户端（对照 commands/env.rs open_buddy_app 极简版：spawn 不等待）──

fn spawn_first_existing(candidates: Vec<PathBuf>, display: &str) -> Result<(), String> {
    let exe = candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| format!("未检测到 {display} 客户端，请先安装或在「环境配置」手动指定路径"))?;
    // mac（2026-10-05 实装）：候选为 .app bundle（目录）时须走 LaunchServices `open`
    // 直启（对齐 env.rs::launch_buddy）——Command::new 直接 exec 目录会因不可执行
    // 而失败；手动路径指向 bundle 内裸二进制时仍走直接 spawn
    #[cfg(target_os = "macos")]
    let is_bundle = exe.is_dir() && exe.extension().map(|e| e == "app").unwrap_or(false);
    #[cfg(target_os = "macos")]
    if is_bundle {
        crate::platform::cmd::sys_command("open")
            .arg(&exe)
            .spawn()
            .map_err(|e| format!("启动 {display} 失败: {e}"))?;
        return Ok(());
    }
    // sys_command_path 跨平台等价（mac=裸 Command 透传 / Windows=加 CREATE_NO_WINDOW），F-75 收敛
    crate::platform::cmd::sys_command_path(&exe)
        .spawn()
        .map_err(|e| format!("启动 {display} 失败: {e}"))?;
    Ok(())
}

/// 打开 Qoder CN IDE
#[tauri::command(async)]
pub fn qoder_open_ide(state: State<AppState>) -> Result<(), String> {
    spawn_first_existing(ide_exe_candidates(&state), "Qoder CN IDE")
}

/// 打开 QoderWork CN
#[tauri::command(async)]
pub fn qoder_open_work(state: State<AppState>) -> Result<(), String> {
    spawn_first_existing(work_exe_candidates(&state), "QoderWork CN")
}
