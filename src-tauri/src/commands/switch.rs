use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;

// async：内含 python 探测子进程（网络 I/O，最长 ~15s）与 detect_guard_uid_strict 子进程，
// 同步命令会冻结 UI（项目约定：阻塞型命令一律 #[tauri::command(async)]）
#[tauri::command(async)]
pub fn switch_account(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
    proxy_port: Option<u16>,
    // 续期 JWT 流程专用：目标账号 JWT 本就可能已吊销（续期正是为了重抓），
    // 跳过 TRAE 切换前 JWT 预检，否则预检 401 会把续期链路拦死
    skip_jwt_probe: Option<bool>,
) -> Result<(), String> {
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "switch/重置/保存 已到达 Rust: ps_dir={:?}, bridge 存在={}",
            ps_dir,
            bridge.exists()
        ),
    );
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    fs_utils::app_log(&state.data_dir, &format!("开始切换账号: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB（设置开关控制，其他应用不受影响）
    let is_doubao = target_app.as_deref() == Some("Doubao");
    // JWT 预检仅 TRAE 双应用（TraeWork/Trae，含默认）：WorkBuddy/CodeBuddy 会话模型不同，
    // 且其 uid 与 TRAE 账号池撞库时会被误探活错误拦截——非 trae 一律放行（CodeBuddy 同 WorkBuddy）
    let is_trae = matches!(target_app.as_deref(), None | Some("TraeWork") | Some("Trae"));
    let include_idb = is_doubao && state.settings().doubao_snapshot_include_idb;
    // C1：一键以账号打开时注入代理（>0 才传给桥）
    let inject_port = proxy_port.filter(|p| *p > 0);
    // 切换前服务端会话预检（仅豆包）：目标槽位快照里的会话若已被服务端吊销——常见于
    // 在豆包客户端内退出登录/重登该账号（passport logout 吊销旧会话，快照文件却完好）——
    // 恢复后客户端一联网即被 SESSION_EXPIRED 强制登出，表现为「切换成功但豆包未登录」。
    // 实测不对称现象根因：2026-09-09 A 槽探测 code=710012001（expired）、B 槽 code=0（ok）。
    // 提前拦截给出补救指引，避免白切一场；探测不可用 fail-open 不阻断（见函数内实现）。
    if is_doubao {
        crate::commands::doubao::probe_slot_session_alive(
            &state.data_dir,
            &state.python_dir,
            &state.python_exe,
            &user_id,
        )?;
    } else if is_trae && !skip_jwt_probe.unwrap_or(false) {
        // TRAE（TraeWork/Trae）：切换前 JWT 服务端预检（issue #9）——目标账号 JWT 被服务端
        // 吊销时本地快照仍完好，切换恢复后 IDE 一联网即被登出，用户感知为「切换了但没反应」。
        // 401 判死时提前中止并给出补救指引；网络故障 fail-open 不阻断（见函数内实现）。
        crate::commands::accounts::probe_trae_jwt_alive(&state, &user_id)?;
    }

    // 防误覆盖守卫：把关闭客户端前检测到的当前登录 uid 传给桥，桥仅在它与
    // current_account.txt 一致时才把"当前态"回写进该账号槽。豆包走严格版（还要求
    // Live Cookies 里验证到登录会话——uid 检测可能被快照 localStorage 残留骗过，
    // Cookie 存在性无法伪造）；icube 布局（TraeWork/Trae）走本机使用证据推导（见下）
    let expected_uid = if is_doubao {
        crate::commands::doubao::detect_guard_uid_strict(&state)
    } else if is_trae {
        // icube 布局（TraeWork/Trae）切换守卫：此前恒为空串（fail-open），桥不会收到
        // -ExpectedCurrentUid，关闭客户端前的"当前态"可能被回写进错误账号槽。现复用
        // trae_apps 的本机使用证据推导（apps_accounts_discover 同源实现）填充当前 uid；
        // 推导失败（None）→ 维持空串 fail-open 不阻断切换。switch_account 为 async 命令，
        // vscdb/storage 同步读取在工作线程执行，不冻结 UI
        let kind = target_app.as_deref().unwrap_or("TraeWork");
        crate::commands::trae_apps::infer_current_cloud_uid(kind).unwrap_or_default()
    } else {
        String::new()
    };

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &bridge.to_string_lossy(),
        "-Action",
        "Switch",
        "-UserId",
        &user_id,
        "-TargetApp",
        target_app.as_deref().unwrap_or("TraeWork"),
        "-Json",
    ]);
    if let Some(p) = inject_port {
        cmd.args(["-ProxyPort", &p.to_string()]);
    }
    if include_idb {
        cmd.arg("-IncludeIndexedDB");
    }
    if !expected_uid.is_empty() {
        cmd.args(["-ExpectedCurrentUid", &expected_uid]);
    }
    // 数据目录注入：桥的 ProfilesDir/日志按 AIWORKDATA_DIR 解析（便携模式/默认 %APPDATA% 均一致）
    cmd.env("AIWORKDATA_DIR", &state.data_dir);
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：隐藏切换时闪出的黑色控制台窗口
        .spawn()
        .map_err(|e| format!("启动切换失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("切换脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let uid_for_dc = user_id.clone();
    let dc_dir = data_dir.clone();

    // stdout 线程：NDJSON -> switch-progress 事件
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut done_emitted = false;
        // 不能用 BufRead::lines()：桥 stdout 若回退为 GBK（中文系统重定向），首行含中文即
        // Err 且 for-lines 直接终止——done 信号丢失。字节级 read_until + lossy 解码（同 doubao.rs）
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let l = String::from_utf8_lossy(&buf).trim().to_string();
            if l.is_empty() {
                continue;
            }
            let _ = app2.emit("switch-progress", &l);
            // 检测 done / fatal 行，emit switch-done 事件
            if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                let success = l.contains("\"stage\":\"done\"");
                done_emitted = true;
                let _ = app2.emit("switch-done", serde_json::json!({ "success": success, "raw": l }));
                // 切换成功后补充该账号的账户中心（icube-dc）id 预留记录（只记录不展示）
                // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
                if success && !is_doubao {
                    let _ = crate::commands::trae_apps::backfill_dc_id_for(&dc_dir, &uid_for_dc);
                }
            }
        }
        let exit_status = child.wait();
        // 仅当脚本未输出 done/fatal 时才在结束时兜底 emit，避免对同一次切换重复发两次 switch-done
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit("switch-done", serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }));
        }
    });

    // stderr 线程：防止管道缓冲区写满导致子进程死锁
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = data_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            let mut reader = BufReader::new(stderr);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                let l = String::from_utf8_lossy(&buf);
                let l = format!("[stderr] {}", l.trim());
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    let _ = writeln!(f, "[{}] {}", fs_utils::now_ts(), l);
                }
            }
        });
    }

    Ok(())
}

/// 保存当前登录态：关闭 Trae → 精准备份到 userId 槽位 → 重新启动
/// 通过 NDJSON 事件流式返回进度，前端订阅 save-login-progress / save-login-done
// async：豆包分支含会话预检 python 子进程（网络 I/O），同步命令会冻结 UI
#[tauri::command(async)]
pub fn save_current_login(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "switch/重置/保存 已到达 Rust: ps_dir={:?}, bridge 存在={}",
            ps_dir,
            bridge.exists()
        ),
    );
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    // 保存前预检（仅豆包）：Live profile 必须持有登录会话 Cookie。没有 = 客户端当前未登录，
    // 保存只会把未登录状态存进账号槽（实测 908 槽被未登录态覆盖后"切换成功但永远没登录"），
    // 直接拒绝并告知补救方式。客户端此时仍在运行，Cookies 被锁由 python 复制到临时目录读取。
    if target_app.as_deref() == Some("Doubao") {
        crate::commands::doubao::ensure_live_has_login_session(&state)?;
        // 服务端会话预检：本地 Cookie 存在≠会话有效。会话可能早已被服务端吊销
        // （客户端内退出过/被新登录顶替），存进去就是死会话，之后每次切换该账号都未登录
        //（实测 A 槽事故：20:43 保存的快照当时已是/随后被吊销的死会话）。expired 拒绝保存。
        crate::commands::doubao::probe_live_session_alive(&state, &user_id)?;
    }

    fs_utils::app_log(&state.data_dir, &format!("开始保存当前登录态: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB
    let include_idb = target_app.as_deref() == Some("Doubao")
        && state.settings().doubao_snapshot_include_idb;

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &bridge.to_string_lossy(),
        "-Action",
        "SaveCurrentLogin",
        "-UserId",
        &user_id,
        "-TargetApp",
        target_app.as_deref().unwrap_or("TraeWork"),
        "-Json",
    ]);
    if include_idb {
        cmd.arg("-IncludeIndexedDB");
    }
    // 数据目录注入（同 switch_account）
    cmd.env("AIWORKDATA_DIR", &state.data_dir);
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：隐藏控制台窗口
        .spawn()
        .map_err(|e| format!("启动保存登录态失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("保存登录态脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let uid_for_dc = user_id.clone();
    let dc_dir = data_dir.clone();

    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut done_emitted = false;
        // 同 switch_account：字节级读取 + lossy，防 GBK 回退时丢行/丢 done 信号
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let l = String::from_utf8_lossy(&buf).trim().to_string();
            if l.is_empty() {
                continue;
            }
            let _ = app2.emit("save-login-progress", &l);
            if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                let success = l.contains("\"stage\":\"done\"");
                done_emitted = true;
                let _ = app2.emit(
                    "save-login-done",
                    serde_json::json!({ "success": success, "raw": l }),
                );
                // 保存登录态成功后同样补充 dc id 预留记录（快照刚生成，来源最可靠）
                // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
                if success && target_app.as_deref() != Some("Doubao") {
                    let _ = crate::commands::trae_apps::backfill_dc_id_for(&dc_dir, &uid_for_dc);
                }
            }
        }
        let exit_status = child.wait();
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit(
                "save-login-done",
                serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }),
            );
        }
    });

    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = data_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            let mut reader = BufReader::new(stderr);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                let l = String::from_utf8_lossy(&buf);
                let l = format!("[stderr] {}", l.trim());
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    let _ = writeln!(f, "[{}] {}", fs_utils::now_ts(), l);
                }
            }
        });
    }

    Ok(())
}

/// 6 层设备标识重置：调用 PowerShell 脚本的 ResetDeviceIds 动作
/// 通过 NDJSON 事件流式返回进度，前端订阅 device-reset-progress / device-reset-done
/// target_app：TraeWork（默认，TRAE SOLO CN）/ Trae（Trae CN IDE），决定清理哪个应用的数据目录
#[tauri::command]
pub fn reset_device_ids(
    app: AppHandle,
    state: State<AppState>,
    target_app: Option<String>,
) -> Result<(), String> {
    let target = match target_app.as_deref() {
        Some("Trae") => "Trae",
        _ => "TraeWork",
    };
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "switch/重置/保存 已到达 Rust: ps_dir={:?}, bridge 存在={}, target_app={}",
            ps_dir,
            bridge.exists(),
            target
        ),
    );
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    fs_utils::app_log(&state.data_dir, "开始 6 层设备标识重置");

    let mut child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &bridge.to_string_lossy(),
            "-Action",
            "ResetDeviceIds",
            "-TargetApp",
            target,
            "-Json",
        ])
        // 数据目录注入：桥日志按 AIWORKDATA_DIR 解析，与主进程保持一致
        .env("AIWORKDATA_DIR", &state.data_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：隐藏控制台窗口
        .spawn()
        .map_err(|e| format!("启动设备标识重置失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("设备重置脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();

    // stdout 线程：NDJSON -> device-reset-progress 事件
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut done_emitted = false;
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                let _ = app2.emit("device-reset-progress", &l);
                if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                    let success = l.contains("\"stage\":\"done\"");
                    done_emitted = true;
                    let _ = app2.emit(
                        "device-reset-done",
                        serde_json::json!({ "success": success, "raw": l }),
                    );
                }
            }
        }
        let exit_status = child.wait();
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit(
                "device-reset-done",
                serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }),
            );
        }
    });

    // stderr 线程：防止管道缓冲区写满导致子进程死锁
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = data_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                    {
                        let _ = writeln!(f, "[{}] {}", fs_utils::now_ts(), l);
                    }
                }
            }
        });
    }

    Ok(())
}
