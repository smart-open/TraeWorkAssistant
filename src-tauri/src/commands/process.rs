//! F-47 进程管理增强：三级关闭策略
//!
//! 参考社区实现（oss-ecosystem-research §5.3 rotate.rs）的三级关闭思路：
//! 1. **优雅关闭**：Windows taskkill（不带 /F）向主窗口发送 WM_CLOSE / macOS SIGTERM
//!    （sysinfo kill_with，Electron 收到后走正常 quit 流程，落盘语义等价 WM_CLOSE），
//!    等待最长 3s，让 Electron 尽量正常落盘（避免强杀导致 leveldb/vscdb 文件锁与数据损坏）；
//!    实测 Electron 收到 WM_CLOSE 后通常 1s 内退出，3s 足够且不拖慢切换体验；
//! 2. **强杀进程树**：Windows taskkill /T /F / macOS SIGKILL（sysinfo kill()），
//!    等待最长 2s；
//! 3. **人工介入**：仍存活则返回 Err，由前端 toast 提示用户手动关闭。
//!
//! 匹配策略：仅按主程序映像名精确匹配（Windows tasklist 的 IMAGENAME /
//! macOS sysinfo 进程名），crashpad-helper 等子进程不会独立命中；
//! 树杀阶段随主进程一并清理。
//! Windows 子进程以 CREATE_NO_WINDOW 拉起（platform::cmd::sys_command 收敛点）；
//! macOS 进程探测/关闭走 sysinfo（F-75 M1-1.1，与 switcher/proc.rs 同源实现，
//! Windows 路径零变化——tasklist/taskkill 语义保留）。

#[cfg(windows)]
use crate::platform::cmd::sys_command;
use std::time::{Duration, Instant};

/// 检查任一映像名是否仍在运行（Windows：tasklist 精确匹配 IMAGENAME；
/// macOS：sysinfo 进程名精确匹配，剥离 .exe 后缀比对）
pub fn images_running(images: &[&str]) -> Vec<String> {
    #[cfg(windows)]
    {
        let mut alive = Vec::new();
        for img in images {
            let running = sys_command("tasklist")
                .args(["/FI", &format!("IMAGENAME eq {img}"), "/NH"])
                .output()
                .map(|o| {
                    let s = String::from_utf8_lossy(&o.stdout);
                    s.to_lowercase().contains(&img.to_lowercase())
                })
                .unwrap_or(false);
            if running {
                alive.push(img.to_string());
            }
        }
        alive
    }
    #[cfg(target_os = "macos")]
    {
        let sys = mac_snapshot();
        let mut alive = Vec::new();
        for img in images {
            // mac 进程名无 .exe 后缀（待 M-1 侦察 6 核对各应用 mac 主进程名）；
            // images_for_app 的 Windows 形态名单经剥离后精确比对
            let want = img.strip_suffix(".exe").unwrap_or(img);
            if sys
                .processes()
                .values()
                .any(|p| p.name().to_string_lossy().eq_ignore_ascii_case(want))
            {
                alive.push(img.to_string());
            }
        }
        alive
    }
}

#[cfg(windows)]
fn run_taskkill(args: &[&str]) {
    let _ = sys_command("taskkill").args(args).output();
}

/// macOS sysinfo 快照（与 switcher/proc.rs 同款封装）
#[cfg(target_os = "macos")]
fn mac_snapshot() -> sysinfo::System {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys
}

/// macOS：全部进程的可执行文件路径（F-75 P2-2 app_locate mac 分派的进程回退级；
/// 调用方自行做 bundle 根归一与白名单防串台，见 commands/env.rs）
#[cfg(target_os = "macos")]
pub(crate) fn mac_exe_paths() -> Vec<std::path::PathBuf> {
    mac_snapshot()
        .processes()
        .values()
        .filter_map(|p| p.exe().map(|e| e.to_path_buf()))
        .collect()
}

/// macOS 白名单进程匹配（剥离 .exe 后缀精确比对）
#[cfg(target_os = "macos")]
fn mac_name_matches(images: &[&str], name: &str) -> bool {
    images
        .iter()
        .any(|img| img.strip_suffix(".exe").unwrap_or(img).eq_ignore_ascii_case(name))
}

/// 三级关闭指定映像名的进程。
/// 返回 Ok(()) 表示全部退出；返回 Err 提示人工介入。
pub fn graceful_kill_images(images: &[&str]) -> Result<(), String> {
    let alive = images_running(images);
    if alive.is_empty() {
        return Ok(());
    }

    #[cfg(windows)]
    {
        // ---- 第一级：优雅关闭（WM_CLOSE），最长等待 3s ----
        for img in &alive {
            // taskkill 不带 /F：向有窗口的进程发送关闭消息
            run_taskkill(&["/IM", img]);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if images_running(images).is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        // ---- 第二级：强杀进程树（/T /F），最长等待 2s ----
        for img in &alive {
            run_taskkill(&["/T", "/F", "/IM", img]);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if images_running(images).is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    #[cfg(target_os = "macos")]
    {
        // ---- 第一级：优雅关闭（SIGTERM，Electron 正常落盘），最长等待 3s ----
        {
            let sys = mac_snapshot();
            for p in sys.processes().values() {
                if mac_name_matches(images, &p.name().to_string_lossy()) {
                    let _ = p.kill_with(sysinfo::Signal::Term); // SIGTERM
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if images_running(images).is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        // ---- 第二级：强杀（SIGKILL，sysinfo kill()），最长等待 2s ----
        {
            let sys = mac_snapshot();
            for p in sys.processes().values() {
                if mac_name_matches(images, &p.name().to_string_lossy()) {
                    let _ = p.kill(); // unix = SIGKILL
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if images_running(images).is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    // ---- 第三级：人工介入 ----
    let still = images_running(images).join("、");
    Err(format!(
        "进程 {still} 未能自动关闭（优雅关闭与强制结束均失败），请手动关闭后重试"
    ))
}

/// 按应用类别返回候选映像名（与 env.rs 的安装探测保持一致）。
pub fn images_for_app(app_kind: &str) -> Vec<&'static str> {
    match app_kind {
        // Trae Work（SOLO CN）及其历史版本名
        "TraeWork" => vec!["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"],
        // Trae CN IDE
        "Trae" => vec!["Trae CN.exe"],
        // 豆包桌面版（Chromium 壳，主进程即 Doubao.exe）
        "Doubao" => vec!["Doubao.exe"],
        // WorkBuddy 桌面版（批次3 会话备份/恢复前置关闭）
        "WorkBuddy" => vec!["WorkBuddy.exe"],
        // CodeBuddy 桌面版（安装形态含 CN 后缀，与 trae-switch-bridge.ps1 ProcNames 对齐；
        // 此前缺失导致 Rust 侧 graceful_kill_app 杀不掉 CodeBuddy，与桥行为不一致存竞态）
        "CodeBuddy" => vec!["CodeBuddy.exe", "CodeBuddy CN.exe"],
        _ => vec![],
    }
}

/// 便捷入口：按应用类别三级关闭
pub fn graceful_kill_app(app_kind: &str) -> Result<(), String> {
    let images = images_for_app(app_kind);
    if images.is_empty() {
        return Err(format!("未知的应用类别: {app_kind}"));
    }
    graceful_kill_images(&images)
}
