use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::python::spawn_script;
use crate::state::AppState;

#[derive(serde::Serialize)]
pub struct CertStatus {
    pub installed: bool,
}

const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 取多行文本尾部若干行作为错误摘要（防 traceback 过长刷屏）
fn stderr_tail(text: &str, lines: usize) -> String {
    let collected: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..].join(" | ")
}

/// 检查当前解析到的 Python 环境能否导入指定模块
fn python_import_ok(state: &AppState, module: &str) -> bool {
    matches!(
        Command::new(&state.python_exe)
            .args(["-c", &format!("import {module}")])
            .creation_flags(CREATE_NO_WINDOW)
            .output(),
        Ok(o) if o.status.success()
    )
}

/// 用当前 Python 环境安装依赖包（用于 cryptography 缺失时自愈）
fn pip_install(state: &AppState, pkgs: &[&str]) -> Result<(), String> {
    let out = Command::new(&state.python_exe)
        .args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input"])
        .args(pkgs)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("启动 pip 失败: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = stderr_tail(&String::from_utf8_lossy(&out.stderr), 3);
    Err(format!(
        "exit {}：{}",
        out.status.code().unwrap_or(-1),
        if err.is_empty() { "无错误输出".into() } else { err }
    ))
}

#[tauri::command(async)]
pub fn cert_status(_app: AppHandle, _state: State<AppState>) -> CertStatus {
    let out = Command::new("certutil")
        .args(["-store", "Root"])
        .creation_flags(CREATE_NO_WINDOW)
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

#[tauri::command(async)]
pub fn cert_install(app: AppHandle, state: State<AppState>) -> Result<CertStatus, String> {
    // 0. 依赖自检：cryptography 缺失是 --gen-ca 失败的头号原因（issue #6），先尝试自愈再继续
    if !python_import_ok(&state, "cryptography") {
        pip_install(&state, &["cryptography>=42.0.0", "pywin32>=306"]).map_err(|e| {
            format!(
                "Python 缺少 cryptography 模块且自动安装失败（{}）。请手动执行：\"{}\" -m pip install cryptography pywin32",
                e, state.python_exe
            )
        })?;
    }

    // 1. 确保 CA 证书已生成（data_dir/certs/ca.cer），失败时携带子进程真实报错
    let cer = state.path("certs").join("ca.cer");
    if !cer.exists() {
        let child = spawn_script(&state, "device_proxy.py", &["--gen-ca".to_string()], true)?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("等待 CA 生成进程失败: {e}"))?;
        if !cer.exists() {
            let err = stderr_tail(&String::from_utf8_lossy(&out.stderr), 4);
            // 最常见根因是 Python 依赖缺失（issue #6）；自检已自动装过 cryptography，
            // 仍报此错说明环境异常，给出可执行的手动修复指引
            let hint = if err.contains("No module named") {
                format!(
                    "。提示：Python 依赖缺失，请在 \"{}\" 中执行 -m pip install cryptography pywin32 后重试（解释器可查 app.log 的 python_exe= 行）",
                    state.python_exe
                )
            } else {
                String::new()
            };
            return Err(format!(
                "CA 证书生成失败（exit {}）：{}{}",
                out.status.code().unwrap_or(-1),
                if err.is_empty() { "无错误输出".into() } else { err },
                hint
            ));
        }
    }
    let cer_arg = cer.to_string_lossy().replace('\\', "/").to_string();

    // 2. 管理员权限安装到本地计算机受信任根证书颁发机构（触发 UAC）：
    //    路径加引号防止含空格时被拆参；-PassThru + exit 取 certutil 真实退出码（-Wait 不取退出码会误报成功）
    let ps = format!(
        "$p = Start-Process certutil -ArgumentList '-addstore','-f','Root','\"{}\"' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        cer_arg
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("启动证书安装失败: {e}"))?;

    if !status.success() {
        return Err(format!(
            "证书安装被取消或失败（可能需要管理员权限；certutil 退出码 {:?}）",
            status.code()
        ));
    }

    // 3. 复查根存储，防止「命令成功但证书未生效」的误报
    let result = cert_status(app, state);
    if !result.installed {
        return Err("证书安装命令已执行，但根证书存储中未找到 TraeDeviceProxyCA，请检查系统策略".into());
    }
    Ok(result)
}
