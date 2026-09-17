//! 平台服务层（F-75 macOS 支持 M0 底座）：
//! 数据根目录 / 子进程构建 / （后续批次：系统代理、证书信任、保险库、定时注册）
//!
//! 归位原则（f75-macos-support-design.md §2.1）：
//! - 能用 `#[cfg]` 就地分支的不抽象（单函数 ≤5 行差异放原文件）；
//! - 多处复用的收敛为本模块公共函数（≥3 文件使用）；
//! - Windows 行为零变化红线：本模块在 Windows 上的输出与既有 `std::env::var` 直读完全一致。

pub mod cert_ctl;
pub mod cmd;
pub mod proxy_ctl;
pub mod secret;

/// 平台标识（platform_info 命令下发前端，控制入口显隐与文案）
pub const OS: &str = if cfg!(target_os = "macos") {
    "macos"
} else {
    "windows"
};

pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// 用户应用数据根目录（业务数据落点）：
/// - Windows: `%APPDATA%`（C:\Users\<u>\AppData\Roaming）
/// - macOS:   `$HOME/Library/Application Support`
pub fn app_support_root() -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .map_err(|_| "无法读取 APPDATA 环境变量".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map_err(|_| "无法读取 HOME 环境变量".to_string())?;
        Ok(std::path::PathBuf::from(home).join("Library/Application Support"))
    }
}

/// 目标应用数据基根（profile.rs / env.rs 的档案路径展开用）：
/// - Windows: `%LOCALAPPDATA%`（Trae CN/豆包在 Roaming 与 Local 混布，由各档案指定）
/// - macOS:   `$HOME/Library/Application Support`
#[allow(dead_code)] // M1 档案表 os 维度接线后移除
pub fn local_data_root() -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .map_err(|_| "无法读取 LOCALAPPDATA 环境变量".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        app_support_root()
    }
}

/// Program Files / /Applications（exe 候选路径基根）
#[allow(dead_code)] // M1 locate.rs bundle 探测接线后移除
pub fn programs_root() -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    {
        std::env::var("ProgramFiles")
            .map(std::path::PathBuf::from)
            .map_err(|e| format!("无法读取 ProgramFiles 环境变量: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(std::path::PathBuf::from("/Applications"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_os_flag_matches_target() {
        if cfg!(target_os = "macos") {
            assert_eq!(OS, "macos");
            assert!(is_macos());
        } else {
            assert_eq!(OS, "windows");
            assert!(!is_macos());
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_roots_resolve_on_windows() {
        // Windows 形态锁定：%APPDATA% / %LOCALAPPDATA% / Program Files
        assert!(app_support_root().unwrap().ends_with("Roaming"));
        assert!(local_data_root().unwrap().ends_with("Local"));
        assert!(programs_root().unwrap().to_string_lossy().contains("Program Files"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_roots_resolve_on_macos() {
        assert!(app_support_root()
            .unwrap()
            .ends_with("Library/Application Support"));
        assert_eq!(programs_root().unwrap(), std::path::PathBuf::from("/Applications"));
    }
}
