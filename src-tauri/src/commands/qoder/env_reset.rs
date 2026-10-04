//! Qoder M4 · 环境重置 / 彻底登出（对照 WorkBuddy F-14 同语义，粒度按语义块）。
//!
//! 9 项清理清单（8 项映射 QODER_IDE_ITEMS 15 条文件级目标，switcher/icube.rs
//! L54-71 + 疑点④ Work 客户端会话项）：按「认证语义块」聚合而非逐文件，避免
//! 用户面对 15 个勾选项。执行顺序：关闭 Qoder CN（防占用与清理后回写；Work
//! 本体 0.4.3+ 进程名同为 "Qoder CN.exe"，同一 kill 覆盖 IDE 与 Work）→
//! 按勾选项逐项清理（单项失败不中断）。Qoder 无 SSO 注销对应物（凭证为本地
//! PAT/客户端存储），无 Keycloak 步骤。
//!
//! 指纹提示：清理 machine_identity / shared_client_cache 等于放弃当前设备身份，
//! 客户端下次启动将重新注册（可配合「账号绑定指纹」仍存于工具侧不受影响）。
//!
//! 【跨平台审查 2026-10-03】macOS 适配预留：清理清单的目录语义本身跨平台同构
//! （数据目录布局不变），差异点：① 路径解析走 ide_data_dir()/cli_dir()，改底层
//! 取目录函数即全清单生效；② graceful_kill_app 的 taskkill 为 Windows 专属，
//! macOS 需 pkill/AppleScript 等价实现（见 commands/process.rs）；③ 带重试删除
//! （force_rmtree 3×400ms）针对 Windows 文件锁场景，macOS 下无害可原样保留；
//! ④ cli_auth 详情文案「%USERPROFILE%」为 Windows 表述，macOS 分支需本地化。
//! 检索标记：`macOS 适配预留`。

use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::ide_data_dir;

fn cli_dir() -> Option<PathBuf> {
    // 主目录收口（跨平台审查 2026-10-05）：统一走 platform::home_dir()
    // （Windows=USERPROFILE / macOS=HOME），env 缺失时返回空 PathBuf → None
    let home = crate::platform::home_dir();
    if home.as_os_str().is_empty() {
        None
    } else {
        Some(home.join(".qoder-cn"))
    }
}

/// QoderWork 客户端数据目录（疑点④）：Electron userData 根，0.4.3 起与 IDE
/// 拆分独立布局；登录会话 = Local State（Cookies 解密密钥）+ Cookies（会话）。
/// 三平台收口（跨平台审查 2026-10-05）：Windows=%APPDATA%\com.qodercn.app.stable /
/// macOS=~/Library/Application Support/com.qodercn.app.stable（对齐
/// switcher/profile.rs QoderWork data_dir）/ 其他平台回退 APPDATA。
fn work_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let root = crate::platform::app_support_root_lossy();
        if root.as_os_str().is_empty() {
            None
        } else {
            Some(root.join("com.qodercn.app.stable"))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| PathBuf::from(d).join("com.qodercn.app.stable"))
    }
}

/// 9 项清单（id, label, detail）——存在性检查在命令层动态计算。
/// 疑点④ 补 Work 客户端项：Work（0.4.3 起与 IDE 拆分）数据目录与 IDE 不同
///（com.qodercn.app.stable），登录会话不在 IDE 清理项覆盖范围内。
fn qoder_reset_catalog() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("vscdb_auth", "登录令牌库", "删除 User\\globalStorage\\state.vscdb 及 -wal/-shm/.backup 边车（登录态真源，客户端启动重建）"),
        ("storage_json", "storage.json", "删除 User\\globalStorage\\storage.json（设备标识/遥测/认证信息）"),
        ("machine_identity", "机器身份文件", "删除根级 machineid / Local State / Preferences（设备指纹与窗口状态；Local State 含 vscdb 解密密钥，清理后残留 vscdb 登录密文不可解，客户端需重新登录）"),
        ("local_storage", "Local Storage", "删除 Local Storage\\leveldb（web 侧登录/偏好 KV）"),
        ("network_cookies", "Network Cookies", "删除 Network 目录（Cookie 等网络会话数据）"),
        ("session_storage", "Session Storage", "删除 Session Storage 目录（会话级 KV）"),
        ("shared_client_cache", "客户端身份四小件", "删除 SharedClientCache\\cache 下 id / machine_token.json / client.json / status.json（设备注册与激活状态）"),
        ("cli_auth", "CLI 数据目录", "删除 ~/.qoder-cn（R-3 侦察结论：当前无凭证落盘，清残留配置）"),
        ("work_client", "Work 客户端会话", "删除 QoderWork（com.qodercn.app.stable）内 Local State 与 Network\\Cookies（客户端登录会话：qoderuid Cookie 及其解密密钥，删除后 Work 需重新登录）"),
    ]
}

/// 带重试删除目录（Windows 文件占用场景：最多 3 次，间隔 400ms）；不存在返回 Ok(false)
fn force_rmtree(p: &Path) -> Result<bool, String> {
    if !p.exists() {
        return Ok(false);
    }
    let mut last = String::new();
    for _ in 0..3 {
        match std::fs::remove_dir_all(p) {
            Ok(()) => return Ok(true),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
    Err(format!("删除 {} 失败: {last}", p.display()))
}

/// 删除 dir 下指定文件名集合，返回实际删除个数
fn remove_files(dir: &Path, names: &[&str]) -> Result<usize, String> {
    let mut n = 0;
    for name in names {
        let f = dir.join(name);
        if f.exists() {
            std::fs::remove_file(&f).map_err(|e| format!("删除 {} 失败: {e}", f.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// 执行单个清理项，返回人类可读结果描述。
/// items 供关联项校验：machine_identity 删除 Local State 后残留 vscdb 登录密文
/// 不可解，未勾选 vscdb_auth 时追加联动提示（不自动连带删除，保持用户勾选语义）
fn run_reset_item(id: &str, items: &[String]) -> Result<String, String> {
    let base = ide_data_dir().ok_or("无法解析 %APPDATA%（QoderCN 数据目录不可用）")?;
    let gs = base.join("User").join("globalStorage");
    match id {
        "vscdb_auth" => {
            let n = remove_files(
                &gs,
                &["state.vscdb", "state.vscdb-wal", "state.vscdb-shm", "state.vscdb.backup"],
            )?;
            Ok(format!("已删除登录令牌库 {n}/4 个文件"))
        }
        "storage_json" => match remove_files(&gs, &["storage.json"])? {
            1 => Ok("已删除 storage.json".into()),
            _ => Ok("storage.json 不存在（跳过）".into()),
        },
        "machine_identity" => {
            let n = remove_files(&base, &["machineid", "Local State", "Preferences"])?;
            let mut detail = format!("已删除机器身份文件 {n}/3 个");
            if n > 0 && !items.iter().any(|i| i == "vscdb_auth") && gs.join("state.vscdb").exists() {
                detail.push_str(
                    "（注意：Local State 已删但「登录令牌库」未勾选，残留 state.vscdb 的解密密钥已丢失，客户端需重新登录；建议下次一并勾选）",
                );
            }
            Ok(detail)
        }
        "local_storage" => match force_rmtree(&base.join("Local Storage").join("leveldb"))? {
            true => Ok("已删除 Local Storage\\leveldb".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "network_cookies" => match force_rmtree(&base.join("Network"))? {
            true => Ok("已删除 Network 目录（Cookie）".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "session_storage" => match force_rmtree(&base.join("Session Storage"))? {
            true => Ok("已删除 Session Storage 目录".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "shared_client_cache" => {
            let cache = base.join("SharedClientCache").join("cache");
            let n = remove_files(&cache, &["id", "machine_token.json", "client.json", "status.json"])?;
            Ok(format!("已删除客户端身份文件 {n}/4 个"))
        }
        "cli_auth" => match cli_dir() {
            Some(d) => match force_rmtree(&d)? {
                true => Ok("已删除 ~/.qoder-cn".into()),
                false => Ok("目录不存在（跳过）".into()),
            },
            None => Ok("无法解析 %USERPROFILE%（跳过）".into()),
        },
        // 疑点④：Work 登录会话清理——与 IDE 侧 machine_identity+network_cookies 同
        // 语义（解密密钥 + Cookie 库成对删除，残留密文不可解）；只清登录相关，
        // 不整目录删除（userData 内含缓存/日志等非会话数据）
        "work_client" => match work_data_dir() {
            Some(d) => {
                let n = remove_files(&d, &["Local State"])?;
                let cookies = force_rmtree(&d.join("Network"))?;
                if n == 0 && !cookies {
                    Ok("Work 会话文件不存在（跳过）".into())
                } else {
                    Ok(format!(
                        "已删除 Work 客户端会话（Local State {n}/1，Network 目录{}）",
                        if cookies { "已删" } else { "不存在" }
                    ))
                }
            }
            None => Ok("无法解析 %APPDATA%（跳过）".into()),
        },
        _ => Err(format!("未知清理项: {id}")),
    }
}

#[derive(Serialize, Clone)]
pub struct QoderResetItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub exists: bool,
}

/// 环境重置清单：8 项 + 动态存在性标注（供 UI 勾选预览）
#[tauri::command]
pub fn qoder_env_reset_items() -> Vec<QoderResetItem> {
    let base = ide_data_dir();
    let gs = base.as_ref().map(|b| b.join("User").join("globalStorage"));
    qoder_reset_catalog()
        .iter()
        .map(|(id, label, detail)| {
            let in_gs = |name: &str| gs.as_ref().map(|g| g.join(name).exists()).unwrap_or(false);
            let in_base = |name: &str| base.as_ref().map(|b| b.join(name).exists()).unwrap_or(false);
            let exists = match *id {
                "vscdb_auth" => {
                    in_gs("state.vscdb") || in_gs("state.vscdb-wal") || in_gs("state.vscdb-shm") || in_gs("state.vscdb.backup")
                }
                "storage_json" => in_gs("storage.json"),
                "machine_identity" => in_base("machineid") || in_base("Local State") || in_base("Preferences"),
                "local_storage" => base.as_ref().map(|b| b.join("Local Storage").join("leveldb").is_dir()).unwrap_or(false),
                "network_cookies" => base.as_ref().map(|b| b.join("Network").is_dir()).unwrap_or(false),
                "session_storage" => base.as_ref().map(|b| b.join("Session Storage").is_dir()).unwrap_or(false),
                "shared_client_cache" => {
                    in_base("SharedClientCache")
                        && ["id", "machine_token.json", "client.json", "status.json"]
                            .iter()
                            .any(|f| base.as_ref().map(|b| b.join("SharedClientCache").join("cache").join(f).exists()).unwrap_or(false))
                }
                "cli_auth" => cli_dir().map(|d| d.is_dir()).unwrap_or(false),
                "work_client" => work_data_dir()
                    .map(|d| {
                        d.join("Network").join("Cookies").exists() || d.join("Local State").exists()
                    })
                    .unwrap_or(false),
                _ => false,
            };
            QoderResetItem {
                id: id.to_string(),
                label: label.to_string(),
                detail: detail.to_string(),
                exists,
            }
        })
        .collect()
}

/// 环境重置执行：关闭 Qoder CN → 按勾选项逐项清理（单项失败不中断）。
#[tauri::command(async)]
pub async fn qoder_env_reset(
    app: AppHandle,
    state: State<'_, AppState>,
    items: Vec<String>,
) -> Result<Vec<serde_json::Value>, String> {
    if items.is_empty() {
        return Err("未选择任何清理项".into());
    }
    // spawn_blocking（对照 credits.rs/oauth.rs 做法）：graceful_kill_app 的
    // tasklist/taskkill 轮询（最坏 3×400ms×N 项）与 force_rmtree 大目录删除均为
    // 重 IO，async 命令体内直接执行会占用 async worker 线程；state 需提前
    // clone/move 进闭包（State 非移动安全），items/app 一并移交，结果 await 回传
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut results: Vec<serde_json::Value> = vec![];

        // 关闭 Qoder CN（防数据目录占用与清理后回写）
        let _ = crate::commands::process::graceful_kill_app("Qoder CN");

        // 按勾选项执行（单项失败不中断其余项）
        for id in &items {
            match run_reset_item(id, &items) {
                Ok(detail) => results.push(serde_json::json!({ "id": id, "ok": true, "detail": detail })),
                Err(e) => results.push(serde_json::json!({ "id": id, "ok": false, "detail": e })),
            }
        }
        let ok_n = results
            .iter()
            .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
            .count();
        let fail_n = results.len() - ok_n;
        fs_utils::app_log(
            &state.data_dir,
            &format!("qoder: 环境重置完成（{ok_n}/{} 项成功）", items.len()),
        );
        if fail_n > 0 {
            crate::commands::workbuddy::push_notify(
                Some(&app),
                &state.data_dir,
                "Qoder 环境重置",
                &format!("清理完成，{fail_n} 项失败，请查看详情"),
                crate::notify::NotifyEvent::Other,
            );
        }
        Ok(results)
    })
    .await
    .map_err(|e| format!("环境重置任务失败: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_catalog_has_9_unique_ids() {
        let cat = qoder_reset_catalog();
        // 疑点④：第 9 项 work_client（QoderWork 客户端 Local State + Network\Cookies）
        assert_eq!(cat.len(), 9);
        let mut ids: Vec<&str> = cat.iter().map(|(id, _, _)| *id).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n);
        // 新增项必须在列（防止后续误删导致静默回退 8 项）
        assert!(ids.contains(&"work_client"));
    }

    #[test]
    fn remove_files_counts_only_existing() {
        let dir = std::env::temp_dir().join(format!("qoder_reset_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        let n = remove_files(&dir, &["a.json", "missing.json"]).unwrap();
        assert_eq!(n, 1);
        assert!(!dir.join("a.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
