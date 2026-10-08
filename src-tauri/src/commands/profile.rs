use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::switcher::{Action, RunArgs, TauriSink, TargetApp};

/// 登录态快照信息
#[derive(Serialize, Clone)]
pub struct ProfileInfo {
    pub slot: String,
    pub size_bytes: u64,
    pub file_count: u64,
    pub last_modified: String,
}

/// profiles 根目录：%APPDATA%\AIWorkAssistant\data\profiles\（Trae Work）
/// 或 data\profiles_trae\（Trae CN IDE）、data\profiles_doubao\（豆包）、
/// data\profiles_codebuddy\（CodeBuddy 桌面）、data\profiles_workbuddy\（WorkBuddy 桌面，
/// authfile 布局）——profiles_workbuddy 由切换桥按 -TargetApp WorkBuddy 写入，
/// 此处映射保证通用快照命令（list/backup/restore/delete）与桥同源，防误操作 TraeWork 快照
fn profiles_dir(state: &State<AppState>, target_app: Option<&str>) -> PathBuf {
    match target_app {
        Some("Trae") => state.data_dir.join("data").join("profiles_trae"),
        Some("Doubao") => state.data_dir.join("data").join("profiles_doubao"),
        Some("CodeBuddy") => state.data_dir.join("data").join("profiles_codebuddy"),
        Some("WorkBuddy") => state.data_dir.join("data").join("profiles_workbuddy"),
        Some("Qoder") => state.data_dir.join("data").join("profiles_qoder"),
        Some("QoderWork") => state.data_dir.join("data").join("profiles_qoder_work"),
        _ => state.data_dir.join("data").join("profiles"),
    }
}

/// 归一化 target_app：仅接受 "Trae"（Trae CN IDE）/ "Doubao"（豆包）/ "CodeBuddy"（CodeBuddy 桌面）/
/// "WorkBuddy"（WorkBuddy 桌面，authfile 布局）/ "Qoder"（Qoder CN IDE）/
/// "QoderWork"（Qoder Work 独立客户端，electron-root 布局），其余一律视为 TraeWork
fn normalize_target_app(target_app: Option<&str>) -> &'static str {
    match target_app {
        Some("Trae") => "Trae",
        Some("Doubao") => "Doubao",
        Some("CodeBuddy") => "CodeBuddy",
        Some("WorkBuddy") => "WorkBuddy",
        Some("Qoder") => "Qoder",
        Some("QoderWork") => "QoderWork",
        _ => "TraeWork",
    }
}

/// 递归计算目录大小和文件数
pub(crate) fn dir_stats(path: &std::path::Path) -> (u64, u64) {
    let mut size = 0u64;
    let mut count = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let (s, c) = dir_stats(&p);
                size += s;
                count += c;
            } else {
                size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                count += 1;
            }
        }
    }
    (size, count)
}

/// 多目录并行统计体积（保持入参顺序返回）。
///
/// 性能（2026-10-07 实测）：CodeBuddy 档 14 个槽位 / 18,372 目录 / 13,659 文件，
/// 单线程 `dir_stats` 约 4.9s，且「只遍历不取 metadata」同样 4.87s——耗时几乎全在逐目录
/// `read_dir`（Windows 上文件 metadata 随目录项免费返回，不是瓶颈）。4 线程同机实测 1.7s。
/// 分配用 round-robin 交错而非连续切块：连续切块下体积悬殊的槽位扎堆同一线程时，
/// 总时长由最慢线程决定，并行收益退化。
fn dir_stats_many(paths: &[PathBuf]) -> Vec<(u64, u64)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let threads = paths.len().min(4).max(1);
    let mut out = vec![(0u64, 0u64); paths.len()];
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                scope.spawn(move || {
                    paths
                        .iter()
                        .enumerate()
                        .skip(i)
                        .step_by(threads)
                        .map(|(gi, p)| (gi, dir_stats(p)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        // 按全局下标回填而非 flat_map 拼接：某线程 panic（join Err）时只在收集线程
        // 串行重算该线程负责的槽位、对位补齐——不会因缺块让后续槽位体积整体错位。
        for (i, h) in handles.into_iter().enumerate() {
            match h.join() {
                Ok(rs) => {
                    for (gi, r) in rs {
                        out[gi] = r;
                    }
                }
                Err(_) => {
                    for (gi, p) in paths.iter().enumerate().skip(i).step_by(threads) {
                        out[gi] = dir_stats(p);
                    }
                }
            }
        }
    });
    out
}

/// 槽位体积缓存项。
#[derive(Clone, Copy)]
struct CachedDirStats {
    size: u64,
    files: u64,
    /// 统计时槽位目录自身的 mtime。实测（NTFS）：只有槽位**直接子项**的增删才会更新它
    /// （子目录内文件改动不会），且更新时间戳存在延迟（同一毫秒内写入常读不到变化）——
    /// 故它只作**尽力而为**的失效信号：命中与否不影响正确性，正确性由 TTL 与显式失效保证。
    slot_mtime_ms: i64,
    at: Instant,
}

/// 槽位体积缓存：列表页每次打开/切档都要体积，而体积统计是「遍历整棵快照目录」的重活。
/// 键 = 槽位目录绝对路径（含 data_dir，多实例/改数据目录天然隔离）。
static PROFILE_STATS_CACHE: OnceLock<Mutex<HashMap<String, CachedDirStats>>> = OnceLock::new();

/// 缓存时间上限（默认 5 分钟）：目录 mtime 更新既可能延迟、也不覆盖子目录内容变化，
/// 故用 TTL 限定陈旧上限；备份 / 删除等已知写入路径另有显式失效（`fresh=true` 前端手动刷新）。
/// 取 5 分钟是权衡：命中即零遍历（弹窗秒开），代价是体积最多滞后 5 分钟。
const PROFILE_STATS_TTL: Duration = Duration::from_secs(300);

fn stats_cache() -> &'static Mutex<HashMap<String, CachedDirStats>> {
    PROFILE_STATS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cached_stats(path: &Path, slot_mtime_ms: i64) -> Option<(u64, u64)> {
    let key = path.to_string_lossy().to_string();
    let cache = stats_cache().lock().ok()?;
    let hit = cache.get(&key)?;
    if hit.slot_mtime_ms == slot_mtime_ms && hit.at.elapsed() < PROFILE_STATS_TTL {
        Some((hit.size, hit.files))
    } else {
        None
    }
}

fn store_stats(path: &Path, slot_mtime_ms: i64, size: u64, files: u64) {
    let key = path.to_string_lossy().to_string();
    if let Ok(mut cache) = stats_cache().lock() {
        cache.insert(
            key,
            CachedDirStats { size, files, slot_mtime_ms, at: Instant::now() },
        );
    }
}

/// 槽位内容被整体写入（备份）/删除后主动失效，避免 mtime 粒度或实现差异留下陈旧体积。
pub(crate) fn invalidate_profile_stats(slot_dir: &Path) {
    let key = slot_dir.to_string_lossy().to_string();
    if let Ok(mut cache) = stats_cache().lock() {
        cache.remove(&key);
    }
}

/// 格式化文件大小
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// 列出所有已保存的登录态快照（target_app: TraeWork=TRAE SOLO CN / Trae=Trae CN IDE）
///
/// 性能（2026-10-07）：体积统计改「缓存 + 多槽并行 + spawn_blocking」——体积要遍历整棵
/// 快照目录（CodeBuddy 档实测 18,372 目录 / 13,659 文件，单线程 4.9s / 4 线程 1.7s），
/// 原实现是同步命令逐槽串行，打开「登录态快照管理」会把 UI 卡住数秒。
/// `fresh=true` 绕过缓存强制重算（前端「刷新列表」按钮走这条）。
#[tauri::command]
pub async fn profile_list(
    state: State<'_, AppState>,
    target_app: Option<String>,
    fresh: Option<bool>,
) -> Result<Vec<ProfileInfo>, String> {
    let dir = profiles_dir(&state, target_app.as_deref());
    let data_dir = state.data_dir.clone();
    let fresh = fresh.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || list_profiles_blocking(&dir, &data_dir, fresh))
        .await
        .map_err(|e| format!("快照列表任务失败: {e}"))
}

/// `profile_list` 的同步实现（在 spawn_blocking 内执行；单测直接调用）。
fn list_profiles_blocking(dir: &Path, data_dir: &Path, fresh: bool) -> Vec<ProfileInfo> {
    // ① 枚举槽位：只读目录项 + 取槽位自身 mtime（轻量，不递归）
    let mut slots: Vec<(String, PathBuf, i64, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let last_modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| {
                    let dt =
                        chrono::DateTime::<chrono::Local>::from(std::time::SystemTime::UNIX_EPOCH + d);
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_else(|| "-".to_string());
            slots.push((
                entry.file_name().to_string_lossy().to_string(),
                path,
                mtime_ms(&meta),
                last_modified,
            ));
        }
    }

    // ② 命中缓存直接用；其余收集起来并行统计（常态命中 → 零遍历）
    let mut sizes: Vec<Option<(u64, u64)>> = vec![None; slots.len()];
    let mut pending: Vec<(usize, PathBuf)> = Vec::new();
    for (i, (_, path, mtime, _)) in slots.iter().enumerate() {
        let hit = if fresh { None } else { cached_stats(path, *mtime) };
        match hit {
            Some(v) => sizes[i] = Some(v),
            None => pending.push((i, path.clone())),
        }
    }
    let mut computed = 0usize;
    let mut computed_bytes = 0u64;
    let mut computed_files = 0u64;
    if !pending.is_empty() {
        let t0 = Instant::now();
        let paths: Vec<PathBuf> = pending.iter().map(|(_, p)| p.clone()).collect();
        for ((i, path), (size, files)) in pending.iter().zip(dir_stats_many(&paths)) {
            store_stats(path, slots[*i].2, size, files);
            sizes[*i] = Some((size, files));
            computed += 1;
            computed_bytes += size;
            computed_files += files;
        }
        // 埋点（对照 [wb-token] 扫描）：只在真的算了的时候落一行，便于回看体积统计开销
        fs_utils::app_log(
            data_dir,
            &format!(
                "[profiles] 快照体积统计 {}：{} 槽位（命中 {} / 计算 {}），计算耗时 {}ms，体积 {}MB / 文件 {}",
                dir.file_name().and_then(|n| n.to_str()).unwrap_or("profiles"),
                slots.len(),
                slots.len() - computed,
                computed,
                t0.elapsed().as_millis(),
                computed_bytes / 1024 / 1024,
                computed_files
            ),
        );
    }

    let mut out: Vec<ProfileInfo> = slots
        .into_iter()
        .enumerate()
        .map(|(i, (slot, _, _, last_modified))| {
            let (size, files) = sizes[i].unwrap_or((0, 0));
            ProfileInfo { slot, size_bytes: size, file_count: files, last_modified }
        })
        .collect();
    // 按修改时间倒序
    out.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    out
}

/// 备份当前登录态到指定 slot（switcher BackupCurrent 动作，进程内直调）
#[tauri::command]
pub fn profile_backup(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // uid 直接作为快照槽目录名传入 switcher，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let target = normalize_target_app(target_app.as_deref());
    // 备份完成即失效该槽位体积缓存（其后列表展示的是刚写入的新体积）
    let slot_dir = profiles_dir(&state, target_app.as_deref()).join(user_id.trim());
    fs_utils::app_log(
        &state.data_dir,
        &format!("开始备份登录态: user_id={user_id}, target_app={target}"),
    );
    // C4：豆包快照可选纳入 IndexedDB
    let include_idb = target == "Doubao" && state.settings().doubao_snapshot_include_idb;
    // F-80 §5.10 保存守卫预探测（2026-10-02 审查新增）：Qoder（IDE vscdb 解密）/
    // QoderWork（客户端 Cookies qoderuid 解密）预探测当前登录账号并传入 L2 守卫
    //（icube/electron_root 守卫优先采用），防把 A 的登录态存进 B 的槽位。
    // 未登录/解密失败 → 空串 fail-open（守卫回退策略见 switcher::mod）。
    // profile_backup 为同步命令：DPAPI 解密仅读一个小文件，耗时可忽略
    let expected_current_uid = match target {
        "Qoder" => crate::commands::qoder::live_account_id(&state).unwrap_or_default(),
        "QoderWork" => crate::commands::qoder::live_work_account_id(&state).unwrap_or_default(),
        _ => String::new(),
    };
    let args = RunArgs {
        action: Action::BackupCurrent,
        target_app: TargetApp::parse(target),
        user_id: Some(user_id),
        proxy_port: None,
        include_indexeddb: include_idb,
        expected_current_uid,
        machine_id_override: None,
        data_dir: state.data_dir.clone(),
    };
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    // 后台线程执行（含优雅关闭等待，不阻塞命令返回；与原 stdout 读线程同语义）
    std::thread::spawn(move || {
        // issue #44：与切换管线对齐——panic 时 profile-done 仍会发射，前端不会永久锁在备份中
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sink = TauriSink::new(&app2, "profile-progress", &data_dir);
            crate::switcher::run_action(args, &sink)
        }));
        // 无论成败都失效该槽位体积缓存：失败也可能已写入部分新文件
        invalidate_profile_stats(&slot_dir);
        super::switch::finish_action_thread(
            &app2,
            "profile-done",
            &data_dir,
            result,
            serde_json::json!({ "action": "backup" }),
        );
    });
    Ok(())
}

/// 恢复指定 slot 的登录态（关闭客户端 → 恢复 → 启动，switcher RestoreOnly 动作）
#[tauri::command]
pub fn profile_restore(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // 检查快照是否存在（按目标应用的 profiles 根目录）
    let slot_dir = profiles_dir(&state, target_app.as_deref()).join(&user_id);
    if !slot_dir.exists() {
        return Err(format!("账号 {} 的登录态快照不存在", user_id));
    }
    // uid 直接作为快照槽目录名传入 switcher，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let target = normalize_target_app(target_app.as_deref());
    fs_utils::app_log(
        &state.data_dir,
        &format!("开始恢复登录态: user_id={user_id}, target_app={target}"),
    );
    // C4：豆包快照可选纳入 IndexedDB（恢复侧对快照内含 IndexedDB 一律回写，此开关主要影响备份）
    let include_idb = target == "Doubao" && state.settings().doubao_snapshot_include_idb;
    // F-80 §5.10.2：Qoder 恢复取账号绑定 machine_id（须在 user_id 被 move 前计算）
    let machine_id_override = if target == "Qoder" {
        crate::commands::qoder::machine_id_of(&state, user_id.trim())
    } else {
        None
    };
    let args = RunArgs {
        action: Action::RestoreOnly,
        target_app: TargetApp::parse(target),
        user_id: Some(user_id),
        proxy_port: None,
        include_indexeddb: include_idb,
        expected_current_uid: String::new(),
        machine_id_override,
        data_dir: state.data_dir.clone(),
    };
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    std::thread::spawn(move || {
        // issue #44：与切换管线对齐——panic 时 profile-done 仍会发射，前端不会永久锁在恢复中
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sink = TauriSink::new(&app2, "profile-progress", &data_dir);
            crate::switcher::run_action(args, &sink)
        }));
        super::switch::finish_action_thread(
            &app2,
            "profile-done",
            &data_dir,
            result,
            serde_json::json!({ "action": "restore" }),
        );
    });
    Ok(())
}

/// 删除指定 slot 的登录态快照
#[tauri::command]
pub fn profile_delete(
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // uid 直接拼进 profiles 根目录路径且本命令整目录删除（remove_dir_all），必须先校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let slot_dir = profiles_dir(&state, target_app.as_deref()).join(&user_id);
    if !slot_dir.exists() {
        return Ok(()); // 不存在视为已删除
    }
    std::fs::remove_dir_all(&slot_dir)
        .map_err(|e| format!("删除快照失败: {e}"))?;
    invalidate_profile_stats(&slot_dir);
    fs_utils::app_log(&state.data_dir, &format!("已删除登录态快照: user_id={user_id}"));
    Ok(())
}

/// 格式化辅助函数（给前端展示用）
#[tauri::command]
pub fn profile_format_size(bytes: u64) -> String {
    format_size(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("profiles_perf_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 建槽位：`files` 个顶层文件 + 一个 `Deep/` 子目录（含 1 个文件）
    /// —— 子目录便于验证「改子目录内容不改槽位目录 mtime」这条缓存语义。
    fn make_slot(root: &Path, slot: &str, files: usize) -> PathBuf {
        let p = root.join(slot);
        std::fs::create_dir_all(p.join("Deep")).unwrap();
        for i in 0..files {
            std::fs::write(p.join(format!("f{i}.bin")), vec![7u8; 100]).unwrap();
        }
        std::fs::write(p.join("Deep").join("g.bin"), vec![7u8; 50]).unwrap();
        p
    }

    #[test]
    fn dir_stats_many_matches_serial_order() {
        let root = tmp_root("many");
        let slots: Vec<PathBuf> = (0..6).map(|i| make_slot(&root, &format!("slot{i}"), i)).collect();
        let serial: Vec<(u64, u64)> = slots.iter().map(|p| dir_stats(p)).collect();
        assert_eq!(dir_stats_many(&slots), serial, "并行结果需与串行逐项一致且保序");
        assert_eq!(dir_stats_many(&[]), Vec::<(u64, u64)>::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缓存原语：同 mtime 命中、mtime 变化不命中、显式失效后不命中。
    #[test]
    fn stats_cache_hits_only_while_slot_mtime_matches() {
        let probe = tmp_root("primitive").join("slot-probe");
        store_stats(&probe, 111, 4096, 7);
        assert_eq!(cached_stats(&probe, 111), Some((4096, 7)), "同 mtime 应命中");
        assert_eq!(cached_stats(&probe, 222), None, "mtime 变化应视为失效");
        invalidate_profile_stats(&probe);
        assert_eq!(cached_stats(&probe, 111), None, "显式失效后不应命中");
        let _ = std::fs::remove_dir_all(probe.parent().unwrap());
    }

    /// 端到端：体积/文件数正确；`fresh=true`（刷新按钮）与显式失效都能拿到最新体积。
    ///
    /// 注：不在此断言「顶层增删后普通调用自动反映」——目录 mtime 的更新在 Windows 上有延迟，
    /// 该路径是尽力而为（见 `CachedDirStats` 注释），断言它会随机器时序抖动。
    #[test]
    fn list_profiles_reports_size_and_honours_fresh() {
        let root = tmp_root("cache");
        let slot = make_slot(&root, "u1", 2);
        // 日志用的 data_dir 必须是 root 之外（否则会被当成第二个槽位）
        let data_dir = std::env::temp_dir().join(format!("profiles_perf_cache_data_{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        let first = list_profiles_blocking(&root, &data_dir, false);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].slot, "u1");
        assert_eq!(first[0].file_count, 3, "2 个顶层文件 + Deep/g.bin");
        assert_eq!(first[0].size_bytes, 2 * 100 + 50);

        // fresh=true（前端「刷新列表」）绕过缓存：新写入一律能看到
        std::fs::write(slot.join("f9.bin"), vec![7u8; 10]).unwrap();
        let refreshed = list_profiles_blocking(&root, &data_dir, true);
        assert_eq!(refreshed[0].file_count, 4);
        assert_eq!(refreshed[0].size_bytes, 2 * 100 + 50 + 10);

        // 子目录内文件增长（不改槽位目录 mtime）：靠显式失效让普通调用也拿到新值
        std::fs::write(slot.join("Deep").join("g.bin"), vec![7u8; 250]).unwrap();
        invalidate_profile_stats(&slot);
        assert_eq!(
            list_profiles_blocking(&root, &data_dir, false)[0].size_bytes,
            2 * 100 + 250 + 10
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
