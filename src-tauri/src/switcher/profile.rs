//! 应用档案表（F-48 表驱动）：5 应用 × 3 快照布局（原 trae-switch-bridge.ps1
//! 82-198 行对译）。icube 布局（TraeWork/Trae）同为 icube 内核的 VSCode fork，
//! 登录态文件结构完全同构，按档案参数化复用全部切换逻辑；chromium（豆包）/
//! authfile（WorkBuddy/CodeBuddy）布局各有独立快照管线。

use std::path::PathBuf;

use super::TargetApp;

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
    pub proc_patterns: &'static [&'static str],
    /// exe 文件名白名单（Test-ExeMatchesApp 语义：lnk/注册表/进程回退防串台）
    pub exe_names: &'static [&'static str],
    /// .lnk 文件名匹配模式（大小写双形态，PS -like 语义）
    pub lnk_patterns: &'static [&'static str],
    /// 注册表 DisplayName 匹配模式
    pub reg_patterns: &'static [&'static str],
    /// exe 候选路径（环境变量展开后的绝对路径，PS $Script:ExeCandidates）
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
}

impl AppProfile {
    /// current_account.txt 路径（PS $Script:CurrentAccountFile）
    pub fn current_account_file(&self) -> PathBuf {
        self.profiles_dir.join("current_account.txt")
    }
}

/// Test-ExeMatchesApp 对译：路径文件名必须 ∈ exe_names 白名单
///（防 lnk/注册表/进程回退解析到另一个应用；大小写不敏感）
pub fn exe_matches(path: &std::path::Path, prof: &AppProfile) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => prof.exe_names.iter().any(|e| e.eq_ignore_ascii_case(name)),
        None => false,
    }
}

/// 档案表（PS switch 块逐项对译；参数顺序见各分支注释）
pub fn profile_for(app: TargetApp, app_data_dir: &std::path::Path) -> AppProfile {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let appdata = env("APPDATA");
    let local = env("LOCALAPPDATA");
    let home = env("USERPROFILE");
    let program_files = env("ProgramFiles");
    let data = app_data_dir;
    match app {
        TargetApp::Trae => AppProfile {
            app_name: "Trae",
            layout: Layout::Icube,
            data_dir: PathBuf::from(format!("{appdata}\\Trae CN")),
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
            // mac 预填假设 ~/Library/Application Support/Trae CN（VS Code fork 惯例），待 M-1 侦察 1 确认
            mac_supported: false,
            mac_data_dir_guess: Some("~/Library/Application Support/Trae CN"),
        },
        TargetApp::Doubao => AppProfile {
            app_name: "豆包",
            layout: Layout::Chromium,
            data_dir: PathBuf::from(format!("{local}\\Doubao\\User Data")),
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
            // mac 预填假设 ~/Library/Application Support/Doubao（Chromium 惯例），待 M-1 侦察 3 确认
            mac_supported: false,
            mac_data_dir_guess: Some("~/Library/Application Support/Doubao"),
        },
        TargetApp::WorkBuddy => AppProfile {
            app_name: "WorkBuddy",
            layout: Layout::Authfile,
            data_dir: PathBuf::from(format!("{home}\\.workbuddy")),
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
            // mac dotfile 惯例 ~\.workbuddy 理论零改动（把握最高的 mac 先行域），待 M-1 侦察 2 确认
            mac_supported: false,
            mac_data_dir_guess: Some("~/.workbuddy"),
        },
        TargetApp::CodeBuddy => AppProfile {
            app_name: "CodeBuddy",
            layout: Layout::Authfile,
            data_dir: PathBuf::from(format!("{home}\\.codebuddy")),
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
            cb_global_storage_dir: Some(PathBuf::from(format!(
                "{appdata}\\CodeBuddy CN\\User\\globalStorage"
            ))),
            // mac 双目录混合布局（dotfile + VS Code fork globalStorage 真源），
            // 含 Keychain secret 绑定风险，待 M-1 侦察 9 确认
            mac_supported: false,
            mac_data_dir_guess: Some("~/.codebuddy"),
        },
        TargetApp::TraeWork => AppProfile {
            app_name: "Trae Work",
            layout: Layout::Icube,
            data_dir: PathBuf::from(format!("{appdata}\\TRAE SOLO CN")),
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
            // mac 预填假设 ~/Library/Application Support/TRAE SOLO CN，待 M-1 侦察 1 确认
            mac_supported: false,
            mac_data_dir_guess: Some("~/Library/Application Support/TRAE SOLO CN"),
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
        assert_eq!(tw.data_dir, PathBuf::from(std::env::var("APPDATA").unwrap()).join("TRAE SOLO CN"));
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
    fn 五应用档案_mac灰度_未侦察前全部禁用() {
        // F-75 M0-0.6：M-1 侦察确认前置 mac_supported=true；锁定默认全 false，
        // 防止「顺手放开」未侦察应用域——放开时必须显式改动此测试
        for (app, name) in [
            (TargetApp::TraeWork, "Trae Work"),
            (TargetApp::Trae, "Trae"),
            (TargetApp::Doubao, "豆包"),
            (TargetApp::WorkBuddy, "WorkBuddy"),
            (TargetApp::CodeBuddy, "CodeBuddy"),
        ] {
            let prof = profile_for(app, &temp_data());
            assert!(!prof.mac_supported, "{name} 未侦察确认前 mac_supported 必须为 false");
        }
    }

    #[test]
    fn 五应用档案_mac数据目录预填_完整() {
        // M1-1.4 预填假设表（设计 §3.4）：确认前仅供侦察对照，不参与任何路径展开
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
}
