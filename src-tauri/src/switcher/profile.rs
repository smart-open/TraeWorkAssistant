//! 应用档案表（F-48 表驱动）：5 应用 × 3 快照布局（原 trae-switch-bridge.ps1
//! 82-198 行对译）。icube 布局（TraeWork/Trae）同为 icube 内核的 VSCode fork，
//! 登录态文件结构完全同构，按档案参数化复用全部切换逻辑；chromium（豆包）/
//! authfile（WorkBuddy/CodeBuddy）布局各有独立快照管线。

use std::path::PathBuf;

use super::TargetApp;

// ── 档案路径平台基根（F-75 M1-1.4：档案表 os 维度）──────────────────────────
// Windows 输出与原裸读 env 完全一致（行为零变化红线）；mac 走 platform 层根目录。

/// Windows %APPDATA%\ / mac ~/Library/Application Support/
fn roaming_dir(name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join(name)
    }
    #[cfg(target_os = "macos")]
    {
        crate::platform::app_support_root_lossy().join(name)
    }
}

/// Windows %LOCALAPPDATA%\ / mac ~/Library/Application Support/
fn local_dir(name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join(name)
    }
    #[cfg(target_os = "macos")]
    {
        crate::platform::app_support_root_lossy().join(name)
    }
}

/// Windows %USERPROFILE%\ / mac $HOME/（dotfile 惯例：~/.workbuddy 等）
fn home_join(rel: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(rel)
    }
    #[cfg(target_os = "macos")]
    {
        crate::platform::home_dir().join(rel)
    }
}

/// 豆包数据目录（M-1 侦察 ③ 实测差异）：Windows 带 `User Data` 层，
/// mac 为 Chromium 直挂 `~/Library/Application Support/Doubao`（Local State/Default 在根）
fn doubao_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        local_dir("Doubao").join("User Data")
    }
    #[cfg(target_os = "macos")]
    {
        local_dir("Doubao")
    }
}

/// 快照布局（PS $Script:SnapshotLayout）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Icube,
    Chromium,
    Authfile,
}

impl Layout {
    /// 字符串形态（与 PS 值一致，用于日志/错误消息）
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Icube => "icube",
            Layout::Chromium => "chromium",
            Layout::Authfile => "authfile",
        }
    }
}

pub struct AppProfile {
    /// 应用显示名（PS $Script:AppName，进入全部进度文案）
    pub app_name: &'static str,
    pub layout: Layout,
    /// 应用真实数据目录（PS $Script:TraeDataDir）
    pub data_dir: PathBuf,
    /// 快照槽根目录（PS $Script:ProfilesDir）
    pub profiles_dir: PathBuf,
    /// app_settings.json 的手动路径键（PS $Script:SettingsPathKey）
    pub settings_path_key: &'static str,
    /// 优雅关闭等待秒数（豆包 8 / WB+CB 5 / 默认 3）
    pub graceful_wait_secs: u64,
    /// 进程名白名单（不带 .exe，精确匹配 = Get-Process -Name 语义；Stop 用）
    pub proc_names: &'static [&'static str],
    /// 进程名通配组（**exe 发现第 5 级专用**，比 Stop 的精确组更宽，再经
    /// exe_names 白名单过滤防串台；PS $Script:ProcPatterns，双轨刻意保留）
    #[cfg_attr(target_os = "macos", allow(dead_code))] // mac bundle 定位链不消费（Windows 专用）
    pub proc_patterns: &'static [&'static str],
    /// exe 文件名白名单（Test-ExeMatchesApp 语义：lnk/注册表/进程回退防串台）
    pub exe_names: &'static [&'static str],
    /// .lnk 文件名匹配模式（大小写双形态，PS -like 语义）
    #[cfg_attr(target_os = "macos", allow(dead_code))] // mac 无 lnk 生态
    pub lnk_patterns: &'static [&'static str],
    /// 注册表 DisplayName 匹配模式
    #[cfg_attr(target_os = "macos", allow(dead_code))] // mac 无注册表
    pub reg_patterns: &'static [&'static str],
    /// exe 候选路径（环境变量展开后的绝对路径，PS $Script:ExeCandidates）
    #[cfg_attr(target_os = "macos", allow(dead_code))] // mac 走 bundle 定位链
    pub exe_candidates: Vec<PathBuf>,
    /// 仅 CodeBuddy：L3 vscdb 登录真源目录（%APPDATA%\CodeBuddy CN\User\globalStorage）
    pub cb_global_storage_dir: Option<PathBuf>,
    /// F-75 M0-0.6：macOS 支持灰度标志——mac 版数据布局经 M-1 侦察确认前置 true，
    /// 未确认前 run_action 对该应用域直接拒绝（切换/备份/恢复全链路）。
    /// Windows 侧恒放行（`is_macos()` 门控），字段不影响既有行为。
    pub mac_supported: bool,
    /// F-75 M1-1.4 预填（按设计 §3.4 假设表，**均待 M-1 实测确认**）：
    /// mac 数据目录假设值（`~` = $HOME，M-1 确认后随 mac_supported 一起放开）。
    /// - VS Code fork 惯例：~/Library/Application Support/<Name>
    /// - dotfile 惯例：~/.workbuddy、~/.codebuddy
    #[allow(dead_code)] // M1 mac 侧接线前 Windows 构建仅测试读取
    pub mac_data_dir_guess: Option<&'static str>,
    /// F-75 M-1 侦察 ⑥（2026-09-20 真机实测）：mac bundle 防串台白名单
    /// （CFBundleIdentifier）。四应用 CFBundleExecutable 均为 "Electron"，
    /// 不可作身份信号——locate.rs bundle 白名单 OR 分支消费。
    #[allow(dead_code)] // mac locate 接线消费；Windows 构建不编译消费点
    pub mac_bundle_ids: &'static [&'static str],
}

impl AppProfile {
    /// current_account.txt 路径（PS $Script:CurrentAccountFile）
    pub fn current_account_file(&self) -> PathBuf {
        self.profiles_dir.join("current_account.txt")
    }
}

/// Test-ExeMatchesApp 对译：路径文件名必须 ∈ exe_names 白名单
///（防 lnk/注册表/进程回退解析到另一个应用；大小写不敏感）
/// F-75 M-1 侦察 ⑥（mac 分支）：mac 主进程可执行文件名为 "Electron"（四应用同名），
/// 文件名匹配恒不命中——改以路径 bundle 段匹配：路径含 `/<App主干>.app`
///（bundle 根或其内部 Contents/MacOS/ 可执行均命中；Helper 内层 bundle 路径
/// 同样含外层 `/<App>.app` 段，归属判定正确）。
pub fn exe_matches(path: &std::path::Path, prof: &AppProfile) -> bool {
    #[cfg(target_os = "macos")]
    {
        let s = path.to_string_lossy().to_lowercase();
        prof.exe_names.iter().any(|e| {
            e.strip_suffix(".exe")
                .map(|stem| s.contains(&format!("/{}.app", stem.to_lowercase())))
                .unwrap_or(false)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => prof.exe_names.iter().any(|e| e.eq_ignore_ascii_case(name)),
            None => false,
        }
    }
}

/// 档案表（PS switch 块逐项对译；参数顺序见各分支注释）
pub fn profile_for(app: TargetApp, app_data_dir: &std::path::Path) -> AppProfile {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let local = env("LOCALAPPDATA");
    let program_files = env("ProgramFiles");
    let data = app_data_dir;
    match app {
        TargetApp::Trae => AppProfile {
            app_name: "Trae",
            layout: Layout::Icube,
            data_dir: roaming_dir("Trae CN"),
            profiles_dir: data.join("data").join("profiles_trae"),
            settings_path_key: "trae_cn_path",
            // 审查修复（2026-09-15）：3s 实测恒超时 → 每次切换都强杀，vscdb WAL 残留
            // 被客户端启动重放导致旧账号复活（与豆包 8s 同理：落盘/退出需要时间）
            graceful_wait_secs: 8,
            proc_names: &["Trae CN"],
            proc_patterns: &["Trae*", "TRAE*"],
            exe_names: &["Trae CN.exe"],
            lnk_patterns: &["*TRAE*", "*Trae*"],
            reg_patterns: &["*TRAE*", "*Trae*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Programs\\Trae CN\\Trae CN.exe")),
                PathBuf::from(format!("{program_files}\\Trae CN\\Trae CN.exe")),
                PathBuf::from("D:\\Programs\\Trae CN\\Trae CN.exe"),
            ],
            cb_global_storage_dir: None,
            // M-1 侦察 ①（2026-09-20 实测确认）：~/Library/Application Support/Trae CN
            // 存在且 User/globalStorage/{state.vscdb,storage.json} 与 Windows 同布局；
            // 差异项：无 Local State / Network/（Cookies 在根级，icube mac 项已补）
            mac_supported: true,
            mac_data_dir_guess: Some("~/Library/Application Support/Trae CN"),
            mac_bundle_ids: &["cn.trae.app"],
        },
        TargetApp::Doubao => AppProfile {
            app_name: "豆包",
            layout: Layout::Chromium,
            data_dir: doubao_data_dir(),
            profiles_dir: data.join("data").join("profiles_doubao"),
            settings_path_key: "doubao_path",
            // chromium 壳退出前要落盘 leveldb/cookie，3 秒实测经常不够（强杀导致
            // 文件锁 → 备份静默缺文件 → 恢复后登录态丢失）
            graceful_wait_secs: 8,
            proc_names: &["Doubao"],
            proc_patterns: &["Doubao*"],
            exe_names: &["Doubao.exe"],
            lnk_patterns: &["*Doubao*", "*豆包*"],
            reg_patterns: &["*Doubao*", "*豆包*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Doubao\\Application\\Doubao.exe")),
                PathBuf::from(format!("{program_files}\\Doubao\\Application\\Doubao.exe")),
            ],
            cb_global_storage_dir: None,
            // M-1 侦察 ③（2026-09-20 实测确认）：mac 版存在（Doubao.app，
            // Chromium 布局 Local State + Default，Cookies 在 Default/ 根级——
            // chromium.rs cookie_files 新旧布局归一已覆盖）；同机快照复制不受
            // Keychain（Doubao Safe Storage）影响
            mac_supported: true,
            mac_data_dir_guess: Some("~/Library/Application Support/Doubao"),
            mac_bundle_ids: &["com.bot.pc.doubao"],
        },
        TargetApp::WorkBuddy => AppProfile {
            app_name: "WorkBuddy",
            layout: Layout::Authfile,
            data_dir: home_join(".workbuddy"),
            profiles_dir: data.join("data").join("profiles_workbuddy"),
            settings_path_key: "workbuddy_path",
            graceful_wait_secs: 5,
            // F2-3 双端解耦：auth 文件虽与 CodeBuddy 共用同一物理文件，但实测确认
            // CodeBuddy 从不回写共享 auth 文件（登录真源在自身 vscdb）——
            // 切/存 WorkBuddy 不关停 CodeBuddy，两端完全独立（ProcNames 仅本端）
            proc_names: &["WorkBuddy"],
            proc_patterns: &["WorkBuddy*"],
            exe_names: &["WorkBuddy.exe"],
            lnk_patterns: &["*WorkBuddy*"],
            reg_patterns: &["*WorkBuddy*"],
            exe_candidates: vec![PathBuf::from(format!(
                "{local}\\Programs\\WorkBuddy\\WorkBuddy.exe"
            ))],
            cb_global_storage_dir: None,
            // M-1 侦察 ②（2026-09-20 实测确认）：~/.workbuddy 存在，storage/{skeleton,
            // user-<uid>*} 与 Windows 同构；auth 文件在 CodeBuddyExtension 布局
            //（mac 根为 ~/Library/Application Support，authfile.rs 已分派）——
            // mac 把握最高的先行域
            mac_supported: true,
            mac_data_dir_guess: Some("~/.workbuddy"),
            mac_bundle_ids: &["com.tencent.workbuddy.mac"],
        },
        TargetApp::CodeBuddy => AppProfile {
            app_name: "CodeBuddy",
            layout: Layout::Authfile,
            data_dir: home_join(".codebuddy"),
            profiles_dir: data.join("data").join("profiles_codebuddy"),
            settings_path_key: "codebuddy_path",
            graceful_wait_secs: 5,
            // F2-1 进程解耦：CodeBuddy 登录真源在自身 state.vscdb（%APPDATA%\CodeBuddy CN），
            // 不消费共享 auth 文件——切/存 CodeBuddy 不关停在跑的 WorkBuddy
            proc_names: &["CodeBuddy", "CodeBuddy CN"],
            proc_patterns: &["CodeBuddy*"],
            exe_names: &["CodeBuddy.exe", "CodeBuddy CN.exe"],
            lnk_patterns: &["*CodeBuddy*"],
            reg_patterns: &["*CodeBuddy*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Programs\\CodeBuddy\\CodeBuddy.exe")),
                PathBuf::from(format!("{local}\\Programs\\CodeBuddy CN\\CodeBuddy CN.exe")),
            ],
            // F1-1 L3 层：CodeBuddy CN（VS Code fork）登录真源实测在自身 roaming 的
            // state.vscdb secret storage，不在共享 auth 文件——快照/恢复必须覆盖此处
            //（mac 根为 ~/Library/Application Support，布局同构）
            cb_global_storage_dir: Some(
                roaming_dir("CodeBuddy CN").join("User").join("globalStorage"),
            ),
            // M-1 侦察 ⑨（2026-09-20 复测放开）：首次侦察时 CodeBuddy CN.app 未启动
            //（无 ~/.codebuddy/、无 CodeBuddy CN 数据目录），维持灰度。复测时客户端已
            // 通过 WorkBuddy OAuth 扫码登录，双目录布局与 Windows 同构：
            //   L2 数据根  ~/.codebuddy/（settings.json, mcp.json, plugins, skills-marketplace）
            //   L3 登录真源 ~/Library/Application Support/CodeBuddy CN/User/globalStorage/
            //              （state.vscdb, storage.json, tencent-cloud.coding-copilot/）
            //   共享 auth  ~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/
            //              workbuddy-desktop.info
            // 三路径全部存在且活跃回写 → mac_supported 放开
            mac_supported: true,
            mac_data_dir_guess: Some("~/.codebuddy"),
            mac_bundle_ids: &["com.tencent.codebuddycn"],
        },
        TargetApp::TraeWork => AppProfile {
            app_name: "Trae Work",
            layout: Layout::Icube,
            data_dir: roaming_dir("TRAE SOLO CN"),
            profiles_dir: data.join("data").join("profiles"),
            settings_path_key: "trae_path",
            // 同 Trae：3s 恒超时强杀 → WAL 残留回放，提至 8s 优雅落盘
            graceful_wait_secs: 8,
            proc_names: &["TRAE SOLO CN", "TRAE SOLO", "Trae"],
            proc_patterns: &["Trae*", "TRAE*"],
            exe_names: &["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"],
            lnk_patterns: &["*TRAE*", "*Trae*"],
            reg_patterns: &["*TRAE*", "*Trae*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Programs\\TRAE SOLO CN\\TRAE SOLO CN.exe")),
                PathBuf::from(format!("{local}\\Programs\\TRAE SOLO\\TRAE SOLO.exe")),
                PathBuf::from(format!("{program_files}\\TRAE SOLO CN\\TRAE SOLO CN.exe")),
                PathBuf::from(format!("{program_files}\\TRAE SOLO\\TRAE SOLO.exe")),
                PathBuf::from(format!("{local}\\Programs\\Trae\\Trae.exe")),
                PathBuf::from(format!("{program_files}\\Trae\\Trae.exe")),
                PathBuf::from("D:\\Programs\\TRAE SOLO CN\\TRAE SOLO CN.exe"),
            ],
            cb_global_storage_dir: None,
            // M-1 侦察 ①（2026-09-20 实测确认）：~/Library/Application Support/TRAE SOLO CN
            // 存在且 globalStorage 布局与 Windows 同构（差异项同 Trae）
            mac_supported: true,
            mac_data_dir_guess: Some("~/Library/Application Support/TRAE SOLO CN"),
            mac_bundle_ids: &["cn.trae.solo.app"],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data() -> PathBuf {
        std::env::temp_dir().join(format!("sw-profile-test-{}", std::process::id()))
    }

    #[test]
    fn 五应用档案字段与ps常量表一致() {
        let data = temp_data();
        let tw = profile_for(TargetApp::TraeWork, &data);
        assert_eq!(tw.app_name, "Trae Work");
        assert_eq!(tw.layout, Layout::Icube);
        #[cfg(windows)]
        assert_eq!(
            tw.data_dir,
            PathBuf::from(std::env::var("APPDATA").unwrap()).join("TRAE SOLO CN")
        );
        assert_eq!(tw.profiles_dir, data.join("data").join("profiles"));
        assert_eq!(tw.settings_path_key, "trae_path");
        assert_eq!(tw.graceful_wait_secs, 8);
        assert_eq!(tw.proc_names, &["TRAE SOLO CN", "TRAE SOLO", "Trae"]);
        assert_eq!(tw.exe_candidates.len(), 7);
        assert!(tw.cb_global_storage_dir.is_none());

        let db = profile_for(TargetApp::Doubao, &data);
        assert_eq!(db.layout, Layout::Chromium);
        assert_eq!(db.graceful_wait_secs, 8);
        assert_eq!(db.profiles_dir, data.join("data").join("profiles_doubao"));

        let wb = profile_for(TargetApp::WorkBuddy, &data);
        assert_eq!(wb.layout, Layout::Authfile);
        assert_eq!(wb.graceful_wait_secs, 5);
        assert_eq!(wb.proc_names, &["WorkBuddy"]);

        let cb = profile_for(TargetApp::CodeBuddy, &data);
        assert_eq!(cb.proc_names, &["CodeBuddy", "CodeBuddy CN"]);
        assert!(cb.cb_global_storage_dir.is_some());

        let trae = profile_for(TargetApp::Trae, &data);
        assert_eq!(trae.settings_path_key, "trae_cn_path");
        assert_eq!(trae.profiles_dir, data.join("data").join("profiles_trae"));
        assert_eq!(trae.exe_candidates.len(), 3);
    }

    #[test]
    fn 五应用档案_mac灰度_m1侦察后五域放开() {
        // F-75 M0-0.6：mac_supported 仅在 M-1 侦察确认后放开。M-1 真机侦察
        //（2026-09-20）：Trae/TraeWork/WorkBuddy/豆包四域布局实测确认 → true；
        // CodeBuddy 首次侦察时客户端未启动，复测时已通过 WorkBuddy OAuth 扫码
        // 登录，L2/L3/authfile 三路径全部存在 → 一并放开
        let confirmed = [
            (TargetApp::TraeWork, "Trae Work"),
            (TargetApp::Trae, "Trae"),
            (TargetApp::Doubao, "豆包"),
            (TargetApp::WorkBuddy, "WorkBuddy"),
            (TargetApp::CodeBuddy, "CodeBuddy"),
        ];
        for (app, name) in confirmed {
            let prof = profile_for(app, &temp_data());
            assert!(prof.mac_supported, "{name} M-1 侦察确认后应为 true");
            assert!(!prof.mac_bundle_ids.is_empty(), "{name} mac_bundle_ids 必填（防串台）");
        }
    }

    #[test]
    fn 五应用档案_mac数据目录预填_完整() {
        // M1-1.4 预填假设表（设计 §3.4）：M-1 实测（2026-09-20）确认四域猜测值与
        // 真实布局一致；CodeBuddy 维持猜测待补侦察
        let cases = [
            (TargetApp::TraeWork, Some("~/Library/Application Support/TRAE SOLO CN")),
            (TargetApp::Trae, Some("~/Library/Application Support/Trae CN")),
            (TargetApp::Doubao, Some("~/Library/Application Support/Doubao")),
            (TargetApp::WorkBuddy, Some("~/.workbuddy")),
            (TargetApp::CodeBuddy, Some("~/.codebuddy")),
        ];
        for (app, guess) in cases {
            assert_eq!(profile_for(app, &temp_data()).mac_data_dir_guess, guess);
        }
    }

    #[test]
    fn current_account_file_位于profiles根() {
        let data = temp_data();
        let tw = profile_for(TargetApp::TraeWork, &data);
        assert_eq!(
            tw.current_account_file(),
            data.join("data").join("profiles").join("current_account.txt")
        );
    }

    #[cfg(windows)]
    #[test]
    fn exe_matches_大小写不敏感与白名单外拒绝() {
        let data = temp_data();
        let tw = profile_for(TargetApp::TraeWork, &data);
        assert!(exe_matches(std::path::Path::new("C:\\x\\trae solo cn.EXE"), &tw));
        assert!(exe_matches(std::path::Path::new("D:\\a\\Trae.exe"), &tw));
        // 白名单外的 exe（如 Trae CN.exe 属于 Trae 档案）拒绝——防串台
        assert!(!exe_matches(std::path::Path::new("C:\\x\\Trae CN.exe"), &tw));
        assert!(!exe_matches(std::path::Path::new("C:\\x\\Doubao.exe"), &tw));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn exe_matches_mac_bundle段匹配() {
        // M-1 侦察 ⑥：mac 主进程可执行名同为 Electron，按路径 bundle 段归属
        let data = temp_data();
        let tw = profile_for(TargetApp::TraeWork, &data);
        // 主进程：…/TRAE SOLO CN.app/Contents/MacOS/Electron
        assert!(exe_matches(
            std::path::Path::new("/Applications/TRAE SOLO CN.app/Contents/MacOS/Electron"),
            &tw
        ));
        // Helper 内层 bundle 路径仍含外层 /TRAE SOLO CN.app 段 → 归属正确
        assert!(exe_matches(
            std::path::Path::new(
                "/Applications/TRAE SOLO CN.app/Contents/Frameworks/TRAE SOLO CN Helper (GPU).app/Contents/MacOS/TRAE SOLO CN Helper (GPU)"
            ),
            &tw
        ));
        // bundle 根（locate 语义）
        assert!(exe_matches(std::path::Path::new("/Applications/TRAE SOLO CN.app"), &tw));
        // 串台拒绝：Trae CN 的 bundle 不属于 TraeWork 档案
        assert!(!exe_matches(
            std::path::Path::new("/Applications/Trae CN.app/Contents/MacOS/Electron"),
            &tw
        ));

        let trae = profile_for(TargetApp::Trae, &data);
        assert!(exe_matches(
            std::path::Path::new("/Applications/Trae CN.app/Contents/MacOS/Electron"),
            &trae
        ));
        assert!(!exe_matches(
            std::path::Path::new("/Applications/TRAE SOLO CN.app/Contents/MacOS/Electron"),
            &trae
        ));
    }
}
