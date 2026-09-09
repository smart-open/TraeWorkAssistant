//! F-47 进程管理增强：三级关闭策略
//!
//! 参考社区实现（oss-ecosystem-research §5.3 rotate.rs）的三级关闭思路：
//! 1. **优雅关闭**：taskkill（不带 /F）向主窗口发送 WM_CLOSE，等待最长 3s，
//!    让 Electron 尽量正常落盘（避免强杀导致 leveldb/vscdb 文件锁与数据损坏）；
//!    实测 Electron 收到 WM_CLOSE 后通常 1s 内退出，3s 足够且不拖慢切换体验；
//! 2. **强杀进程树**：taskkill /T /F，等待最长 2s；
//! 3. **人工介入**：仍存活则返回 Err，由前端 toast 提示用户手动关闭。
//!
//! 匹配策略：仅按主程序映像名精确匹配（tasklist/taskkill 的 IMAGENAME），
//! crashpad-helper 等子进程不会独立命中；树杀阶段随主进程一并清理。
//! 所有子进程均以 CREATE_NO_WINDOW（0x08000000）拉起，不闪烁控制台窗口。

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 检查任一映像名是否仍在运行（tasklist 精确匹配 IMAGENAME）
pub fn images_running(images: &[&str]) -> Vec<String> {
    let mut alive = Vec::new();
    for img in images {
        let running = Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {img}"), "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
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

fn run_taskkill(args: &[&str]) {
    let _ = Command::new("taskkill")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// 三级关闭指定映像名的进程。
/// 返回 Ok(()) 表示全部退出；返回 Err 提示人工介入。
pub fn graceful_kill_images(images: &[&str]) -> Result<(), String> {
    let alive = images_running(images);
    if alive.is_empty() {
        return Ok(());
    }

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
