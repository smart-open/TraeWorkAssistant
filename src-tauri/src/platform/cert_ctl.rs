//! CA 证书信任原语（F-75 M2-2.2）：
//! - Windows：certutil（HKLM 提权 UAC / HKCU 降级），主流程在 commands/cert.rs 原样保留；
//! - macOS：`security` CLI——`find-certificate -p` 存在性诊断 + `verify-cert -p ssl`
//!   信任判定（审查 P2：仅存在性不等于已信任）、`add-trusted-cert` 写用户信任域
//!   （**不带 -d**：`-d` 是 admin 域需管理员；用户域 `login.keychain-db` 无需 sudo，
//!   首次执行会弹 GUI 授权，用户点「始终信任」完成，无 UAC 概念）。
//!
//! Windows 侧 certutil 输出的 ACL 自愈逻辑不适用 mac（Keychain 自管理），不移植。

/// 查询 CA 是否已在系统信任域（cert_status 消费）。
/// Windows：HKLM/HKCU Root 任一命中即已安装（原 installed_in_windows_root）；
/// macOS：审查 P2 修复——原 find-certificate 仅证明「钥匙串里存在这张证书」，
/// 不证明「信任策略已放行」（用户装了证书但在弹窗点拒绝 → 已装未信任被误报）。
/// 现两步走：① find-certificate -p 导出 PEM（保留作存在性诊断，未命中直接 false）；
/// ② `security verify-cert -c <pem> -p ssl` 以真实信任域评估，退出码 0 = 已信任，
/// 最终 bool 由 verify-cert 决定。PEM 经临时文件中转（verify-cert 只收文件路径，
/// 本函数拿不到 ca.crt 落盘路径——调用链仅传 subject，见 commands/cert.rs）。
pub fn cert_query(subject: &str) -> bool {
    #[cfg(windows)]
    {
        let _ = subject; // Windows 侧固定查 TraeDeviceProxyCA（ca.rs 同源实现）
        crate::device_proxy::ca::installed_in_windows_root()
    }
    #[cfg(target_os = "macos")]
    {
        let pem = match crate::platform::cmd::sys_output(
            crate::platform::cmd::sys_command("security")
                .args(["find-certificate", "-c", subject, "-p"]),
        ) {
            Ok(p) if !p.trim().is_empty() => p,
            _ => return false, // 钥匙串无此证书（存在性诊断未命中）
        };
        // 临时文件中转导出的 PEM（本轮审查加固）：OpenOptions create_new + 0600——
        // 原 fs::write 用固定可预测名（pid+seq）+ 跟随符号链接 + 默认 0644，存在
        // 「预置符号链接 → O_TRUNC 覆写任意用户可写文件」的面；create_new 原子
        // 创建拒绝抢占（已存在即失败返回未信任，可重试），0600 仅本用户可读。
        // 文件名含 pid + 全局原子计数，同进程并发 cert_query（cert_status 与
        // 代理启动同时触发）不再交错；内容为公开证书，无泄密面。
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = std::env::temp_dir().join(format!(
            "aiwork_cert_verify_{}_{}.pem",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            opts.mode(0o600);
            match opts.open(&tmp) {
                Ok(f) => f,
                // 已存在（历史残留/异常撞名）→ 放弃本次信任判定（返回未信任，
                // cert_status 表现为「未信任」提示，用户可重试）
                Err(_) => return false,
            }
        };
        let written = file.write_all(pem.as_bytes()).is_ok();
        drop(file);
        let trusted = written
            && crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("security")
                    .args(["verify-cert", "-c"])
                    .arg(&tmp)
                    .args(["-p", "ssl"]),
            )
            .is_ok(); // sys_output 非零退出码即 Err（含 CSSMERR_TP_NOT_TRUSTED 未信任）
        let _ = std::fs::remove_file(&tmp);
        trusted
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
