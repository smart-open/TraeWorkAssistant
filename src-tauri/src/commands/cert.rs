//! CA 证书状态与安装（P4-9 Rust 化）：
//! - 生成：直调 [`crate::device_proxy::ca::ensure_ca`]（兼容 Python 版 RSA CA，
//!   缺失则用 rcgen 生成，布局 data/certs/{ca.crt,ca.key,ca.cer} 不变）；
//!   原「device_proxy.py --gen-ca + pip 依赖自愈」链路随 Python 移除一并删除。
//! - 安装：certutil 管理员写入本地计算机受信任根（PowerShell RunAs 触发 UAC）；
//!   提权被拒（VPN 客户端证书保护/企业组策略/杀软锁 HKLM 根存储，issue #12）时
//!   降级为当前用户存储直装（certutil -user -addstore，无需管理员，Chrome/Edge
//!   信任 HKCU Root，Windows 弹系统安全确认框）。
//! - 探测：HKLM 与 HKCU Root 任一命中即视为已安装。

use tauri::{AppHandle, State};

#[cfg(windows)]
use crate::platform::cmd::sys_command;
use crate::state::AppState;

#[derive(serde::Serialize)]
pub struct CertStatus {
    pub installed: bool,
}

/// 探测文件当前用户可读（空 DACL 等 ACL 损坏时返回 false）
#[cfg(windows)]
fn file_readable(p: &std::path::Path) -> bool {
    std::fs::File::open(p).is_ok()
}

/// 0x80070005（E_ACCESSDENIED）的 i32 表示：进程退出码按有符号解释为 -2147024891
#[cfg(windows)]
const E_ACCESSDENIED: i32 = 0x80070005u32 as i32;

/// certutil/PowerShell 失败退出码 → 人话（5=Win32 拒绝访问；0x80070005=HRESULT 拒绝访问；1223=用户取消 UAC）
#[cfg(windows)]
fn explain_certutil_exit(code: Option<i32>) -> String {
    match code {
        Some(1223) => "用户取消了 UAC 授权".into(),
        Some(5) | Some(E_ACCESSDENIED) => {
            "拒绝访问：可能是证书文件权限不足（可删除数据目录下 certs 文件夹后重试），\
             也可能是杀毒软件/企业策略/VPN 客户端锁定了根证书存储（可暂时退出后重试）"
                .into()
        }
        Some(_) => "可能需要管理员权限或证书文件不可读".into(),
        None => "进程异常退出".into(),
    }
}

#[tauri::command(async)]
pub fn cert_status(_app: AppHandle, _state: State<AppState>) -> CertStatus {
    // Windows：Chrome/Edge 走 Windows 证书 API，HKLM 与 HKCU Root 合并参与链验证，
    // 任一命中即视为已安装（HKCU 降级安装的用户态路径）；检测实现收口至
    // device_proxy::ca::installed_in_windows_root（与代理启动日志同源）。
    // macOS：security find-certificate（F-75 M2-2.2，platform::cert_ctl 收口）
    let installed = crate::platform::cert_ctl::cert_query("TraeDeviceProxyCA");
    CertStatus { installed }
}

#[tauri::command(async)]
pub fn cert_install(app: AppHandle, state: State<AppState>) -> Result<CertStatus, String> {
    // 1. 确保 CA 证书已生成（data_dir/certs/ca.cer；ensure_ca 内部兼容历史 RSA CA）
    let certs_dir = state.path("certs");
    crate::device_proxy::ca::ensure_ca(&certs_dir)?;

    // 2. macOS：security add-trusted-cert 写用户信任域（弹 GUI 授权，用户点
    //    「始终信任」完成；无 UAC 概念）。rcgen 生成的 ca.crt(PEM) 两平台共用。
    #[cfg(target_os = "macos")]
    {
        crate::platform::cert_ctl::cert_install_trusted(&certs_dir.join("ca.crt"))?;
        let result = cert_status(app, state);
        if !result.installed {
            return Err(
                "证书信任命令已执行，但钥匙串中未找到 TraeDeviceProxyCA；请在弹窗中确认\
                 已点「始终信任」，或检查系统安全设置"
                    .into(),
            );
        }
        return Ok(result);
    }

    // 2.（Windows）管理员权限安装到本地计算机受信任根证书颁发机构（触发 UAC）：
    //    路径加引号防止含空格时被拆参；-PassThru + exit 取 certutil 真实退出码；
    //    try/catch 把「用户取消 UAC」映射为 1223（取消时 Start-Process 抛错 →
    //    $p 为 null → exit $null 会误报成功，随后只能靠复查根存储兜底且文案误导）
    #[cfg(windows)]
    {
        let cer = certs_dir.join("ca.cer");
        let cer_arg = cer.to_string_lossy().replace('\\', "/").to_string();
        let ps = format!(
            "try {{ $p = Start-Process certutil -ArgumentList '-addstore','-f','Root','\"{}\"' -Verb RunAs -Wait -PassThru -ErrorAction Stop; exit $p.ExitCode }} catch {{ exit 1223 }}",
            cer_arg
        );
        let mut status = sys_command("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status()
            .map_err(|e| format!("启动证书安装失败: {e}"))?;

        // 3. 失败自愈：certutil 失败常见根因是证书文件 ACL 异常（历史版本收紧 certs
        //    目录可能留下空 DACL，certutil 提权后也读不到 ca.cer，UAC 允许后控制台一闪
        //    而过即退出）。探测文件可读性，不可读则 icacls /reset 恢复继承后重试一次。
        if !status.success() && !file_readable(&cer) {
            let _ = sys_command("icacls")
                .arg(&certs_dir)
                .args(["/reset", "/T"])
                .output();
            if file_readable(&cer) {
                status = sys_command("powershell")
                    .args(["-NoProfile", "-Command", &ps])
                    .status()
                    .map_err(|e| format!("启动证书安装失败: {e}"))?;
            }
        }

        // 4. 降级兜底：提权安装仍被拒且非用户取消 UAC → 当前用户存储直装（issue #12）。
        //    VPN 客户端证书保护/企业组策略/杀软锁 HKLM 根存储时，这是唯一可行路径：
        //    certutil -user -addstore 写 HKCU Root，无需管理员；Windows 会弹系统安全
        //    确认框，用户点是即可；Chrome/Edge 信任 HKCU Root
        let mut installed_via_user_store = false;
        if !status.success() && status.code() != Some(1223) {
            installed_via_user_store = sys_command("certutil")
                .args(["-user", "-addstore", "-f", "Root", &cer_arg])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }

        // 两条路径都失败才报错（降级路径成败由末尾复查统一判定，避免误报）
        if !status.success() && !installed_via_user_store {
            return Err(format!(
                "证书安装被取消或失败（{}；certutil 退出码 {:?}）",
                explain_certutil_exit(status.code()),
                status.code()
            ));
        }

        // 5. 复查根存储（HKLM/HKCU 任一命中），防止「命令成功但证书未生效」的误报
        let result = cert_status(app, state);
        if !result.installed {
            return Err(
                "证书安装命令已执行，但根证书存储中未找到 TraeDeviceProxyCA；若本机装有 \
                 VPN/安全软件，请暂时退出后重试，或检查企业策略是否限制安装根证书"
                    .into(),
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)] // 全部用例 Windows 专属（certutil 语义）
    use super::*;

    #[cfg(windows)]
    #[test]
    fn certutil_exit_code_explanation() {
        // 0x80070005 的有符号表示（issue #12 实测退出码）
        assert_eq!(Some(-2147024891), Some(E_ACCESSDENIED));
        assert!(explain_certutil_exit(Some(E_ACCESSDENIED)).contains("拒绝访问"));
        assert!(explain_certutil_exit(Some(5)).contains("拒绝访问"));
        assert_eq!(explain_certutil_exit(Some(1223)), "用户取消了 UAC 授权");
        assert_eq!(explain_certutil_exit(None), "进程异常退出");
    }
}
