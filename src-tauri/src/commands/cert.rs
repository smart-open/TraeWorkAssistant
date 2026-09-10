use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::python::spawn_script;
use crate::state::AppState;

#[derive(serde::Serialize)]
pub struct CertStatus {
    pub installed: bool,
}

#[tauri::command]
pub fn cert_status(_app: AppHandle, _state: State<AppState>) -> CertStatus {
    let out = Command::new("certutil")
        .args(["-store", "Root"])
        .creation_flags(0x08000000)
        .output();
    let installed = match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("TraeDeviceProxyCA")
        }
        Err(_) => false,
    };
    CertStatus { installed }
}

/// 截取输出尾部（最多 400 字符），用于把子进程真实报错带回给前端 toast。
fn tail_400(s: &str) -> String {
    let trimmed = s.trim();
    let mut chars = trimmed.chars();
    let total = trimmed.chars().count();
    if total <= 400 {
        return trimmed.to_string();
    }
    for _ in 0..(total - 400) {
        chars.next();
    }
    chars.collect()
}

#[tauri::command]
pub fn cert_install(app: AppHandle, state: State<AppState>) -> Result<CertStatus, String> {
    // 1. 确保 CA 证书已生成（data_dir/certs/ca.cer）
    let cer = state.path("certs").join("ca.cer");
    if !cer.exists() {
        // capture=true 回收子进程 stdout/stderr，失败时把真实原因（如
        // ModuleNotFoundError: No module named 'cryptography'）返回给前端，
        // 而不是静默吞掉后只报一句不含原因的「生成失败」。
        let out = spawn_script(&state, "device_proxy.py", &["--gen-ca".to_string()], true)?
            .wait_with_output()
            .map_err(|e| format!("等待 CA 证书生成进程失败: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let detail = if stderr.trim().is_empty() {
                tail_400(&stdout)
            } else {
                tail_400(&stderr)
            };
            if detail.is_empty() {
                return Err(format!(
                    "CA 证书生成失败（退出码 {:?}）",
                    out.status.code()
                ));
            }
            return Err(format!("CA 证书生成失败: {}", detail));
        }
    }
    if !cer.exists() {
        return Err("CA 证书生成失败，无法安装".into());
    }
    let cer_arg = cer.to_string_lossy().replace('\\', "/").to_string();

    // 2. 以管理员权限安装到本地计算机受信任根证书颁发机构（触发 UAC）。
    //    -PassThru 拿到 certutil 进程对象并用其 ExitCode 退出 powershell，
    //    保证 status.success() 反映 certutil 的真实结果（而非仅 powershell 自身）；
    //    路径内嵌双引号，含空格的目录（如 C:\Users\John Doe\...）不会被拆成多个参数。
    let ps = format!(
        "$p = Start-Process certutil -ArgumentList @('-addstore','-f','Root','\"{}\"') -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        cer_arg
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(0x08000000)
        .status()
        .map_err(|e| format!("启动证书安装失败: {e}"))?;

    if !status.success() {
        return Err(format!(
            "证书安装失败或被取消（certutil 退出码 {:?}；可能需要管理员权限或被组策略拒绝）",
            status.code()
        ));
    }
    let _ = app;
    Ok(cert_status(app, state))
}

#[cfg(test)]
mod tests {
    use super::tail_400;

    #[test]
    fn tail_short_input_unchanged() {
        assert_eq!(tail_400("  hello world \n"), "hello world");
        assert_eq!(tail_400(""), "");
    }

    #[test]
    fn tail_long_input_keeps_last_400_chars() {
        let s = "a".repeat(500) + "TRAILING_MARKER";
        let t = tail_400(&s);
        assert_eq!(t.chars().count(), 400);
        assert!(t.ends_with("TRAILING_MARKER"));
        assert!(t.starts_with("aaa")); // 剩余的尾部 a
    }
}
