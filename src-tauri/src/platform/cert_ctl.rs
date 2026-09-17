//! CA 证书信任原语（F-75 M2-2.2）：
//! - Windows：certutil（HKLM 提权 UAC / HKCU 降级），主流程在 commands/cert.rs 原样保留；
//! - macOS：`security` CLI——`find-certificate` 查询、`add-trusted-cert` 写用户信任域
//!   （**不带 -d**：`-d` 是 admin 域需管理员；用户域 `login.keychain-db` 无需 sudo，
//!   首次执行会弹 GUI 授权，用户点「始终信任」完成，无 UAC 概念）。
//!
//! Windows 侧 certutil 输出的 ACL 自愈逻辑不适用 mac（Keychain 自管理），不移植。

/// 查询 CA 是否已在系统信任域（cert_status 消费）。
/// Windows：HKLM/HKCU Root 任一命中即已安装（原 installed_in_windows_root）；
/// macOS：security find-certificate 命中（退出码 0）即视为已信任。
pub fn cert_query(subject: &str) -> bool {
    #[cfg(windows)]
    {
        let _ = subject; // Windows 侧固定查 TraeDeviceProxyCA（ca.rs 同源实现）
        crate::device_proxy::ca::installed_in_windows_root()
    }
    #[cfg(target_os = "macos")]
    {
        crate::platform::cmd::sys_output(
            crate::platform::cmd::sys_command("security").args(["find-certificate", "-c", subject]),
        )
        .is_ok()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = subject;
        false
    }
}

/// 安装 CA 到用户信任域（macOS 专用；Windows 主流程在 cert.rs 不经此函数）。
/// security 会弹 GUI 授权，用户在弹窗中点「始终信任」完成。
#[allow(dead_code)] // mac 分支消费（cert.rs cfg(macos)），Windows 构建不接线
pub fn cert_install_trusted(pem: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let login_keychain = format!(
            "{}/Library/Keychains/login.keychain-db",
            std::env::var("HOME").map_err(|_| "无法读取 HOME 环境变量".to_string())?
        );
        crate::platform::cmd::sys_output(
            crate::platform::cmd::sys_command("security")
                .args(["add-trusted-cert", "-r", "trustRoot", "-k", &login_keychain])
                .arg(pem),
        )
        .map(|_| ())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pem;
        Err("仅 macOS 支持该安装路径（Windows 走 certutil 提权流程）".into())
    }
}
