//! Python 子进程管理辅助。
use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};

use crate::state::AppState;

/// 将子进程挂入 Job Object（kill-on-close）：父进程无论正常退出、崩溃还是被
/// 强杀，OS 都会自动终止子进程（Issue #7 根治孤儿代理）。所有 python 子进程
/// （device_proxy / --gen-ca / auto_checkin）经 spawn_script 启动时统一挂载；
/// 句柄有意不关闭——关闭即杀子进程，句柄随本进程存活，量级可忽略。
/// 采用 windows-sys 类型化 API（既有依赖，仅启用 features，零新增依赖）。
#[cfg(target_os = "windows")]
pub fn assign_job_object(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0
        {
            AssignProcessToJobObject(job, child.as_raw_handle());
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn assign_job_object(_child: &Child) {}

/// 启动一个 python 脚本，注入 TRAEDATA_DIR（指向应用数据目录）。
/// `script` 为 python 目录下的文件名（如 "device_proxy.py"）。
/// 子进程统一挂入 kill-on-close Job（失败不阻断启动，仍有上层兜底）。
pub fn spawn_script(
    state: &AppState,
    script: &str,
    args: &[String],
    capture: bool,
) -> Result<std::process::Child, String> {
    let script_path: PathBuf = state.python_dir.join(script);
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let data_dir = state.data_dir.to_string_lossy().to_string();
    let mut cmd = Command::new(&state.python_exe);
    cmd.arg(&script_path)
        .args(args)
        .creation_flags(0x08000000)
        .env("TRAEDATA_DIR", &data_dir)
        .env("PYTHONIOENCODING", "utf-8");
    if capture {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let child = cmd.spawn().map_err(|e| format!("启动 {} 失败: {}", script, e))?;
    assign_job_object(&child);
    Ok(child)
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::assign_job_object;
    use std::process::{Command, Stdio};

    /// 冒烟测试：对真实子进程执行 assign_job_object 全流程调用安全（不 panic）。
    ///（FFI 结构布局正确性由 SetInformationJobObject 返回值保证。
    /// 子进程为 ping，约 1s 自然退出。）
    #[test]
    fn assign_job_object_smoke() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "-n", "1", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping");
        assign_job_object(&child);
        let _ = child.wait();
    }
}
