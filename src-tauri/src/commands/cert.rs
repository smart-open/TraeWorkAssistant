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

/// 生成 CA 证书（data_dir/certs/ca.cer），失败时返回真实原因。
///
/// 关键：必须捕获 stdout/stderr 与退出码。此前 `let _ = ...wait()` + `capture=false`
/// 会把子进程的真实报错（典型如 `ModuleNotFoundError: No module named 'cryptography'`）
/// 全部丢弃，上层只能给出「CA 证书生成失败」这种无法定位的文案，用户表现为
/// 「点击安装证书后毫无反应」。这里取 stderr 末 5 行回传，并对「缺 Python 依赖」
/// 这一最常见根因补充可执行的修复提示。
fn gen_ca(state: &AppState) -> Result<(), String> {
    let out = spawn_script(state, "device_proxy.py", &["--gen-ca".to_string()], true)?
        .wait_with_output()
        .map_err(|e| format!("生成 CA 证书失败（等待进程结束出错）: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // 取 stderr 末 5 行：traceback 的有效信息在末尾，且避免刷屏
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut tail: Vec<&str> = stderr.lines().rev().take(5).collect();
    tail.reverse();
    let detail = tail.join("\n");
    let detail = if detail.trim().is_empty() {
        format!("子脚本退出码 {:?}", out.status.code())
    } else {
        detail
    };
    // 最常见根因：安装包未随附 Python 运行时与依赖，回退到系统 Python 后缺 cryptography
    let hint = if detail.contains("No module named") {
        "\n\n提示：Python 依赖缺失。请在应用实际使用的 Python 中执行 \
         `python -m pip install cryptography pywin32` 后重启应用\
         （应用使用的解释器见 app.log 中 python_exe= 一行）。"
    } else {
        ""
    };
    Err(format!("CA 证书生成失败：{detail}{hint}"))
}

#[tauri::command]
pub fn cert_install(app: AppHandle, state: State<AppState>) -> Result<CertStatus, String> {
    // 1. 确保 CA 证书已生成（data_dir/certs/ca.cer）
    let cer = state.path("certs").join("ca.cer");
    if !cer.exists() {
        gen_ca(&state)?;
    }
    if !cer.exists() {
        return Err("CA 证书生成失败：证书文件未生成（请查看日志）".into());
    }
    let cer_arg = cer.to_string_lossy().replace('\\', "/").to_string();

    // 2. 以管理员权限安装到本地计算机受信任根证书颁发机构（触发 UAC）
    //    - 路径必须带引号：Start-Process 会把 -ArgumentList 各元素按空格拼成命令行，
    //      用户名/数据目录含空格（如 C:/Users/John Doe/...）时会被拆成多个参数而失败。
    //    - 用 -PassThru 拿到进程对象并 exit $p.ExitCode：仅 -Wait 只等待不取退出码，
    //      certutil 执行失败时 PowerShell 仍返回 0，会误报「安装成功」。
    let ps = format!(
        "$p = Start-Process certutil -ArgumentList '-addstore','-f','Root','\"{cer_arg}\"' -Verb RunAs -PassThru -Wait; exit $p.ExitCode"
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(0x08000000)
        .status()
        .map_err(|e| format!("启动证书安装失败: {e}"))?;

    if !status.success() {
        return Err(format!(
            "证书安装被取消或失败（可能需要管理员权限；certutil 退出码 {:?}）",
            status.code()
        ));
    }
    Ok(cert_status(app, state))
}
