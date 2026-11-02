//! authfile 布局快照（WorkBuddy/CodeBuddy，原 PS Backup/Restore-AuthFileProfile、
//! Wait-AuthFileQuiet、Get-AuthFileUid、Confirm-AuthFileSwitch 924-1243 对译）。
//!
//! 依据 workbuddy-product-design.md §3.3（M2 账号切换 authfile 布局）：
//!   L1 必选  %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info
//!           （登录态明文 JSON；客户端启动时会重写并生成历史快照
//!             workbuddy-desktop.<ts>.<pid>.<uuid>.info，作交叉校验，不入快照槽）
//!           注意：该文件为 **WorkBuddy 专属登录驱动源**（F2-4）——恢复仅对 WorkBuddy
//!           回写；CodeBuddy 登录真源在自身 vscdb（L3），回写共享 auth 会连带切换
//!           WorkBuddy（实测端隔离缺陷），已跳过
//!   L2 体验  ~\<app_data_dir>\storage\user-<uid>* 目录（用户级数据，随账号迁移）
//!   L3（仅 CodeBuddy）state.vscdb 登录真源（含 -wal/-shm 边车/.backup/storage.json
//!     + tencent-cloud.coding-copilot 扩展存储目录）——PS 审查修复点：-wal/-shm 必须
//!     随主库快照，WAL 模式下最新登录写入可能尚未 checkpoint 进主库
//!   元数据  slot\meta.json：uid / savedAt（供 Rust 端校验与账号池回填）
//! 快照/恢复前客户端须已关闭（入口 Switch/SaveCurrentLogin 已先 stop_app）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use super::copy;
use super::{ProgressSink, Session, StepStatus};
use crate::tasks::wb_common::Creds;

// 路径常量（与 commands/workbuddy/common.rs::auth_file_path 同值，注释互指；
// CodeBuddyExtension 宿主目录 + workbuddy 产品线文件名，实测确认）
#[cfg(windows)] // mac 分支走 Application Support 基根（不含反斜杠相对段）
const WB_AUTH_DIR_REL: &str = "CodeBuddyExtension\\Data\\Public\\auth";
const WB_AUTH_FILE_NAME: &str = "workbuddy-desktop.info";

pub(crate) fn wb_auth_dir() -> PathBuf {
    // 审查修复（L14）：LOCALAPPDATA 缺失（如计划任务/服务上下文）时原实现退化为
    // 随进程 CWD 漂移的相对路径——Windows 侧改从 USERPROFILE 推导绝对路径兜底，
    // 两者皆缺失才保留相对路径作最后回退；macOS 侧走 Application Support 基根
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .map(|l| PathBuf::from(l).join(WB_AUTH_DIR_REL))
            .or_else(|_| {
                std::env::var("USERPROFILE")
                    .map(|p| PathBuf::from(p).join("AppData").join("Local").join(WB_AUTH_DIR_REL))
            })
            .unwrap_or_else(|_| PathBuf::from(WB_AUTH_DIR_REL))
    }
    // M-1 侦察 ②（2026-09-20 实测确认）：mac auth 文件与 Windows 同构，仅宿主基根
    // 不同——~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/
    // workbuddy-desktop.info（本机实测存在且客户端活跃回写）
    #[cfg(target_os = "macos")]
    {
        crate::platform::app_support_root_lossy()
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")
    }
}

pub(crate) fn wb_auth_file() -> PathBuf {
    wb_auth_dir().join(WB_AUTH_FILE_NAME)
}

/// 国际版 WorkBuddy AI auth 文件（同目录异名：workbuddy-desktop-ai.info，
/// 与国内 workbuddy-desktop.info 共存互不干扰；2026-10-16 本机实测确认）。
/// 由 Session::new 按 TargetApp::WorkBuddyAI 注入 sess.auth_file。
pub(crate) fn wb_ai_auth_file() -> PathBuf {
    wb_auth_dir().join("workbuddy-desktop-ai.info")
}

/// 国际版 CodeBuddy AI auth 文件（同目录异名推定：codebuddy-desktop-ai.info，
/// 对齐 workbuddy → workbuddy-desktop-ai.info 命名惯例；国际客户端 auth 文件名
/// 暂无实测证据，若推定名不存在，backup_authfile 已按 M7 将 CodeBuddy 系 L1
/// 可选化——auth 文件缺失时跳过 L1、登录真源走 L3 vscdb，行为仍正确）。
/// 由 Session::new 按 TargetApp::CodeBuddyAI 注入 sess.auth_file。
pub(crate) fn cb_ai_auth_file() -> PathBuf {
    wb_auth_dir().join("codebuddy-desktop-ai.info")
}

/// 快照槽内 auth 文件名（随会话目标而异：国内 / 国际异名共存，
/// 备份与恢复按会话实际 auth 文件名对齐，勿再硬编码常量）
fn slot_auth_name(sess: &Session) -> String {
    sess.auth_file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(WB_AUTH_FILE_NAME)
        .to_string()
}

/// 关闭客户端后 auth 文件可能仍被延迟回写（强杀后残留子进程退出落盘，实测恢复后
/// 3 秒被旧账号回写）。备份前等待其静默：mtime 连续 2 秒不变（最长 10 秒），
/// 避免把"回写到一半"的旧登录备份进快照。
/// **调用点仅 backup_authfile 开头（PS 968）**——Restore 路径不等待（PS 1062 起
/// 无此调用，保持一致勿在恢复侧新增，否则恢复多出最长 10s）。
fn wait_auth_file_quiet(auth_file: &Path, sink: &dyn ProgressSink) {
    let Ok(meta) = std::fs::metadata(auth_file) else { return };
    let Ok(last) = meta.modified() else { return };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        let cur = std::fs::metadata(auth_file).and_then(|m| m.modified());
        match cur {
            Ok(t) if t != last => {
                // 变动了，继续等待至静默
            }
            _ => return, // 与上次相同（或读取失败）= 静默
        }
    }
    sink.step(
        "backup",
        StepStatus::Warn,
        "auth 文件持续变动（疑似仍有客户端进程在回写），继续执行但请核实切换结果",
    );
}

/// 从 auth 文件 JSON 提取 uid（兼容 account.uid / uid / auth.account.uid / auth.uid
/// 四键，PS 953-963；按 PS 精确四键保持日志可对拍）
pub fn auth_file_uid(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let j: serde_json::Value =
        serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()?;
    auth_file_uid_value(&j)
}

/// 纯 Value 版 uid 提取（四键兼容，供快照合成后自检复用）
fn auth_file_uid_value(j: &serde_json::Value) -> Option<String> {
    for v in [
        j.get("account").and_then(|a| a.get("uid")),
        j.get("uid"),
        j.get("auth").and_then(|a| a.get("account")).and_then(|a| a.get("uid")),
        j.get("auth").and_then(|a| a.get("uid")),
    ] {
        if let Some(v) = v {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// 备份（PS Backup-AuthFileProfile 965-1060 对译）
/// 具名账号槽位（排除 last / last.bak / last.bak2 这类「当前现场」备份槽）
fn is_named_slot(slot: &str) -> bool {
    !matches!(slot, "last" | "last.bak" | "last.bak2")
}

/// 槽位 meta.json 记录的登录身份（缺失/损坏 → None）
fn read_slot_meta_uid(slot_dir: &Path) -> Option<String> {
    let raw = crate::fs_utils::read_json::<serde_json::Value>(&slot_dir.join("meta.json"));
    raw.get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 槽位当前承载的登录身份：CodeBuddy 槽优先读槽内 vscdb/storage.json 的
/// genie.userId（登录真源随 L3 拷入，独立于 meta.json），缺失才回退 meta.uid；
/// WorkBuddy 槽登录真源就是 auth 文件拷贝、无独立身份信号，直接用 meta.uid。
/// 背景（2026-10-25 实测）：存量 CodeBuddy 槽的 meta.uid 曾被 WorkBuddy uid 污染，
/// 而槽内 vscdb 登录态完好——只看 meta.uid 会把完好存量槽误判为「身份不符/快照
/// 不可用」，且重新保存会被槽位守卫再次拦截（误报粘滞，只能手删槽位）。真源
/// 优先可自愈：守卫放行后 meta.json 随备份重写为当前 live 身份。
pub(crate) fn slot_held_uid(slot_dir: &Path, is_codebuddy: bool) -> Option<String> {
    if is_codebuddy {
        let raw = crate::fs_utils::read_json::<serde_json::Value>(
            &slot_dir.join("vscdb").join("storage.json"),
        );
        crate::fs_utils::dig(&raw, &["genie.userId"])
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| read_slot_meta_uid(slot_dir))
    } else {
        read_slot_meta_uid(slot_dir)
    }
}

/// 客户端当前 live 登录身份：CodeBuddy 系取 `globalStorage\storage.json` 的 genie.userId
/// （缺失回退共享 auth 文件 uid），WorkBuddy 系优先 skeleton 登录快照 primary.uid、
/// 缺失才回退 auth 文件 uid。
/// 注意：共享 auth 文件记录的是 **WorkBuddy** 当前账号，与 CodeBuddy 端可能不一致，
/// 故 CodeBuddy 系必须优先用 genie.userId；WorkBuddy 系同理——现版本客户端接受登录
/// 后只重写 skeleton 快照、不回写 auth 文件（2026-10-30 实测 auth mtime 停在旧日期），
/// 手动登录后立即「保存当前登录态」时 auth 文件可能仍停在上一个账号，用它会触发
/// 槽位身份对齐守卫误拦；skeleton primary.uid 随每次登录/切换重写，是更可信的 live 信号。
pub(crate) fn live_login_uid(sess: &Session) -> Option<String> {
    if matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI") {
        sess.prof
            .cb_global_storage_dir
            .as_ref()
            .and_then(|gs_dir| {
                let raw =
                    crate::fs_utils::read_json::<serde_json::Value>(&gs_dir.join("storage.json"));
                crate::fs_utils::dig(&raw, &["genie.userId"])
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| auth_file_uid(&sess.auth_file))
    } else {
        skeleton_snapshot_uid(sess).or_else(|| auth_file_uid(&sess.auth_file))
    }
}

/// WorkBuddy skeleton 登录快照（`storage\skeleton\account-snapshot.json`）的
/// primary.uid（旧平铺键兜底）；缺失/损坏 → None。
fn skeleton_snapshot_uid(sess: &Session) -> Option<String> {
    let raw = crate::fs_utils::read_json::<serde_json::Value>(
        &sess
            .prof
            .data_dir
            .join("storage")
            .join("skeleton")
            .join("account-snapshot.json"),
    );
    snapshot_uid_of(&raw)
}

pub fn backup_authfile(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let auth_file = &sess.auth_file;
    // 关闭客户端后先等 auth 文件静默（残留进程可能延迟回写旧登录，实测 3 秒后才落盘）
    wait_auth_file_quiet(&auth_file, sink);
    let dest = sess.prof.profiles_dir.join(slot);
    // 审查修复（M7）：CodeBuddy 系（CN/AI）登录真源在 L3 vscdb，auth 文件缺失
    // 不构成早退理由——原实现对 CodeBuddyAI 会话用国内共享 auth 文件做前置判断，
    // 纯国际用户（无国内 WorkBuddy auth 文件）整体早退连 L3 一起跳过，切换空转
    let is_codebuddy = matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI");
    if !auth_file.exists() && !is_codebuddy {
        // 前置早退：首次使用/从未登录场景（区别于复制失败）——不建槽、不写 meta、
        // 不动 .bak，整体返回
        sink.step(
            "backup",
            StepStatus::Warn,
            &format!("auth 文件不存在（可能从未登录）：{}", auth_file.display()),
        );
        return Ok(());
    }
    // live 身份（L2 用，同时做槽位身份对齐守卫）：CodeBuddy 系优先读 genie.userId，
    // 缺失才回退共享 auth 文件（auth 文件记录的是 WorkBuddy 当前登录账号，与 CodeBuddy
    // 端不一致时会把 WorkBuddy 的 uid 写进 CodeBuddy 槽位 meta）。
    let uid = live_login_uid(sess);

    // 槽位身份对齐守卫（2026-10-30 实测）：客户端把无效会话回退到账号 A 后，流程仍按
    // 「检测到的目标 A'」把现场写进 A' 槽位，结果 A' 槽里装的是 A 的登录态（槽 meta.uid
    // 与槽位账号不符，下次切换必然失败）。具名槽位写入前比对槽位承载身份
    //（slot_held_uid）与当前 live 身份，不一致就跳过；last/last.bak 是"当前现场"备份，放行。
    // prev 用 slot_held_uid（CB 槽内 genie.userId 优先）而非 meta.uid：
    // 存量 CB 槽 meta.uid 曾被 WorkBuddy uid 污染（槽内 vscdb 完好），按 meta.uid
    // 判定会误拦合法备份且误报粘滞（重存仍被拦，只能手删槽位）。
    if is_named_slot(slot) {
        if let (Some(live), Some(prev)) = (uid.as_deref(), slot_held_uid(&dest, is_codebuddy)) {
            if prev != live {
                sink.step(
                    "backup",
                    StepStatus::Warn,
                    &format!(
                        "槽位 {slot} 原有登录身份 {prev}，与客户端当前实际登录 {live} 不一致，\
                         已跳过回写（防止把别的账号登录态写进该槽位）；请确认客户端已登录目标账号后重试"
                    ),
                );
                return Ok(());
            }
        }
    }

    // 单代回滚保护（对齐豆包：覆盖前挪 .bak）
    copy::rotate_bak(&dest, slot, sink);
    std::fs::create_dir_all(dest.join("auth")).map_err(|e| format!("创建快照目录失败: {e}"))?;
    let mut copied = 0usize;

    // L1：auth 文件（WorkBuddy 系必选；CodeBuddy 端隔离可选——F2-4：CodeBuddy
    // 不消费共享 auth 文件、恢复侧跳过回写，文件缺失仅 Skip 不阻断）
    match std::fs::copy(&auth_file, dest.join("auth").join(slot_auth_name(sess))) {
        Ok(_) => {
            copied += 1;
            sink.step("backup", StepStatus::Ok, "L1 auth 文件已备份");
        }
        Err(_) if is_codebuddy && !auth_file.exists() => {
            sink.step(
                "backup",
                StepStatus::Skip,
                "L1 auth 文件不存在，跳过（CodeBuddy 登录真源在 vscdb，见 L3）",
            );
        }
        Err(e) => {
            sink.step("backup", StepStatus::Error, &format!("auth 文件备份失败: {e}"));
            return Err(super::thrown(sink, "auth 文件备份失败"));
        }
    }

    // L2 体验：~\<app_data>\storage\user-<uid>* 目录（uid 见函数开头的 live_login_uid；
    // CodeBuddy 系不从共享 auth 文件取号——那是 WorkBuddy 的登录账号，2026-10-25 实测
    // 会把 WorkBuddy 的 uid 写进 CodeBuddy 槽位 meta，导致确认期误判「已回退」）
    match &uid {
        Some(uid) => {
            let storage_dir = sess.prof.data_dir.join("storage");
            if storage_dir.exists() {
                let mut user_dirs: Vec<PathBuf> = std::fs::read_dir(&storage_dir)
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| {
                                p.is_dir()
                                    && p.file_name()
                                        .and_then(|n| n.to_str())
                                        .map(|n| n.starts_with(&format!("user-{uid}")))
                                        .unwrap_or(false)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                user_dirs.sort();
                for d in &user_dirs {
                    let name = d.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let target = dest.join("storage").join(&name);
                    let _ = std::fs::remove_dir_all(&target);
                    if copy::copy_snapshot_item(d, &target) {
                        copied += 1;
                    }
                }
                if !user_dirs.is_empty() {
                    sink.step(
                        "backup",
                        StepStatus::Ok,
                        &format!("L2 用户数据已备份（{} 个目录）", user_dirs.len()),
                    );
                } else {
                    sink.step(
                        "backup",
                        StepStatus::Skip,
                        "L2 用户数据目录不存在，跳过（首次登录前正常）",
                    );
                }
            }
        }
        None => {
            sink.step(
                "backup",
                StepStatus::Warn,
                if auth_file.exists() {
                    "auth 文件中未能解析 uid（JSON 结构变化？），L2 跳过"
                } else {
                    "auth 文件不存在，无法解析 uid，L2 跳过"
                },
            );
        }
    }

    // Intl 扩展（仅 WorkBuddy AI）：国际客户端登录态除 auth 文件外，还落在
    // security/<uid> 加密 vault（cipher 目录）与根级 user-<uid>* 目录（与
    // storage/user-* 并存，本机实测 2026-10-16）——随账号一并快照
    if sess.prof.app_name == "WorkBuddy AI" {
        if let Some(uid) = &uid {
            let mut root_dirs: Vec<PathBuf> = std::fs::read_dir(&sess.prof.data_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.is_dir()
                                && p.file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|n| n.starts_with(&format!("user-{uid}")))
                                    .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            root_dirs.sort();
            for d in &root_dirs {
                let name = d.file_name().unwrap_or_default().to_string_lossy().to_string();
                let target = dest.join("root_user").join(&name);
                let _ = std::fs::remove_dir_all(&target);
                if copy::copy_snapshot_item(d, &target) {
                    copied += 1;
                }
            }
            if !root_dirs.is_empty() {
                sink.step(
                    "backup",
                    StepStatus::Ok,
                    &format!("Intl 根级用户数据已备份（{} 个目录）", root_dirs.len()),
                );
            }
            let sec_dir = sess.prof.data_dir.join("security").join(uid);
            if sec_dir.is_dir() {
                let target = dest.join("security").join(uid);
                let _ = std::fs::remove_dir_all(&target);
                if copy::copy_snapshot_item(&sec_dir, &target) {
                    copied += 1;
                    sink.step("backup", StepStatus::Ok, "Intl security vault 已备份");
                }
            }
        }
    }

    // L3（仅 CodeBuddy）：vscdb 登录真源快照。CodeBuddy CN 桌面端登录态在
    // %APPDATA%\CodeBuddy CN\User\globalStorage（state.vscdb 的 secret://…
    // planning-genie.new.accessTokencn，实测确认），仅备份共享 auth 文件对它无效
    //（root cause：切 CodeBuddy "还是最初的登录账号"）。含 backup 库与扩展存储目录
    //（防 VS Code 启动从 backup/扩展缓存恢复旧登录）。客户端已由 stop_app 关闭，
    // vscdb 无锁可整文件复制。
    if matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI") {
        if let Some(gs_dir) = &sess.prof.cb_global_storage_dir {
            if gs_dir.join("state.vscdb").exists() {
                let dest_vscdb = dest.join("vscdb");
                std::fs::create_dir_all(&dest_vscdb).map_err(|e| e.to_string())?;
                // 审查修复：-wal/-shm 边车必须随主库一起快照——SQLite WAL 模式下
                // 最新登录写入可能尚未 checkpoint 进主库，漏拷会丢失；恢复侧也依赖
                // 它们正确回放。
                for f in [
                    "state.vscdb",
                    "state.vscdb-wal",
                    "state.vscdb-shm",
                    "state.vscdb.backup",
                    "storage.json",
                ] {
                    let src_f = gs_dir.join(f);
                    if src_f.exists() {
                        let _ = std::fs::copy(&src_f, dest_vscdb.join(f));
                        copied += 1;
                    }
                }
                let cop_dir = gs_dir.join("tencent-cloud.coding-copilot");
                if cop_dir.exists() {
                    let cop_dst = dest_vscdb.join("tencent-cloud.coding-copilot");
                    let _ = std::fs::remove_dir_all(&cop_dst);
                    if copy::copy_snapshot_item(&cop_dir, &cop_dst) {
                        copied += 1;
                    }
                }
                sink.step(
                    "backup",
                    StepStatus::Ok,
                    "L3 vscdb 登录态已备份（CodeBuddy CN globalStorage）",
                );
            }
        }
    }

    // 元数据（app 记录实际目标应用：WorkBuddy 与 CodeBuddy 共用 authfile 管线，
    // 以档案名区分）。统一无 BOM 写入——修复 PS 两布局 BOM 不一致（authfile 版
    // 曾带 BOM、chromium 版无 BOM，8.2 #5）
    let meta = json!({
        "schemaVersion": 1,
        "layout": "authfile",
        "app": sess.prof.app_name,
        "uid": uid,
        "savedAt": crate::fs_utils::now_ts(),
        "savedAtMs": chrono::Utc::now().timestamp_millis(),
    });
    // 原子写对齐 mark_slot_key：非原子 std::fs::write 中断会留半截 meta.json——
    // 守卫 fail-open 尚可，但徽标静默消失/标记静默丢失
    if let Err(e) = crate::fs_utils::write_text_atomic(
        &dest.join("meta.json"),
        &serde_json::to_string(&meta).unwrap_or_default(),
    )
    {
        sink.step(
            "backup",
            StepStatus::Warn,
            &format!("meta.json 写入失败（不影响快照）: {e}"),
        );
    }
    sink.step(
        "backup",
        StepStatus::Ok,
        &format!("已备份当前登录态到 {slot} ({copied} 项)"),
    );
    Ok(())
}

/// 恢复（PS Restore-AuthFileProfile 1062-1145 对译）
pub fn restore_authfile(
    sess: &Session,
    slot: &str,
    sink: &dyn ProgressSink,
) -> Result<(), String> {
    let auth_dir = &sess.auth_dir;
    let auth_file = &sess.auth_file;
    let (src, _slot_label) = copy::resolve_slot(sess, slot, sink)?;
    let auth_src = src.join("auth").join(slot_auth_name(sess));
    // 审查修复（M7）：CodeBuddy 系备份侧已允许 L1 auth 文件缺席（登录真源在
    // vscdb，见 L3），恢复侧若仍强制要求，会把纯国际用户的合法快照（仅 vscdb、
    // 无 auth 文件）误判「不完整」而中止恢复
    let is_codebuddy = matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI");
    if !auth_src.exists() && !is_codebuddy {
        sink.step(
            "restore",
            StepStatus::Error,
            "快照缺少 auth 文件，疑似不完整快照，已中止恢复",
        );
        return Err(super::thrown(sink, &format!("快照缺少 auth 文件（槽位 {slot}）")));
    }
    let mut restored = 0usize;

    // F2-4 端隔离（实测 2026-09-15）：共享 auth 文件是 **WorkBuddy** 的登录驱动源，
    // CodeBuddy 不消费它（登录真源在自身 vscdb，见 L3）——切/存 CodeBuddy 时回写
    // 共享 auth 会把 WorkBuddy 的登录一并切走（实测「切 CodeBuddy 时 WorkBuddy
    // 伴随切换」根因）。CodeBuddy 恢复跳过 L1 回写，仅走 L3 vscdb；
    //   WorkBuddy 恢复保持原语义。
    if is_codebuddy {
        sink.step(
            "restore",
            StepStatus::Skip,
            "L1 共享 auth 文件跳过回写（CodeBuddy 登录真源在自身 vscdb；该文件为 WorkBuddy 登录驱动源，回写会连带切换 WorkBuddy）",
        );
    } else {
        // L1 必选：回写 auth 文件
        if let Err(e) = {
            let _ = std::fs::create_dir_all(&auth_dir);
            std::fs::copy(&auth_src, &auth_file)
        } {
            sink.step("restore", StepStatus::Error, &format!("auth 文件恢复失败: {e}"));
            return Err(super::thrown(sink, "auth 文件恢复失败"));
        }
        restored += 1;
        sink.step("restore", StepStatus::Ok, "L1 auth 文件已恢复");

        // 清除客户端的「登出标记」（2026-10-30 实测）：客户端登出时会写
        // `{auth}.logged-out`，启动时读到它就 `Auth logout marker found, ignoring
        // stored session` —— 恢复进去的登录态被**静默忽略**，表现为「切换成功但客户端
        // 一直未登录」。一次切到失效快照就会留下该标记，此后本端所有切换全部失效，
        // 故必须在恢复后无条件清掉。
        if clear_logout_marker(&auth_file) {
            sink.step(
                "restore",
                StepStatus::Ok,
                "已清除客户端登出标记（.logged-out）：否则客户端会忽略恢复的登录态",
            );
        }

        // 回写校验：确认落盘内容确为目标账号（防复制中途失败或复制后立即被其他进程回写）
        let want_uid = auth_file_uid(&auth_src);
        let got_uid = auth_file_uid(&auth_file);
        if let (Some(want), Some(got)) = (&want_uid, &got_uid) {
            if got != want {
                sink.step(
                    "restore",
                    StepStatus::Error,
                    "auth 文件恢复后 uid 不一致（疑似被其他进程回写），已中止启动",
                );
                return Err(super::thrown(sink, "auth 文件恢复后校验失败（uid 不一致）"));
            }
        }
    }

    // L2 体验：storage\user-* 目录对称回写
    let slot_storage = src.join("storage");
    if slot_storage.exists() {
        let dest_storage = sess.prof.data_dir.join("storage");
        let _ = std::fs::create_dir_all(&dest_storage);
        if let Ok(entries) = std::fs::read_dir(&slot_storage) {
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let target = dest_storage.join(&name);
                let _ = std::fs::remove_dir_all(&target);
                if copy::copy_snapshot_item(&p, &target) {
                    restored += 1;
                }
            }
        }
        sink.step("restore", StepStatus::Ok, "L2 用户数据已恢复");
    }

    // Intl 扩展（仅 WorkBuddy AI）：security/<uid> vault 与根级 user-<uid>* 对称回写
    //（备份侧同构；槽内没有 = 旧版快照或目标账号从未在本机登录，跳过不报错）
    if sess.prof.app_name == "WorkBuddy AI" {
        let slot_root = src.join("root_user");
        if slot_root.exists() {
            if let Ok(entries) = std::fs::read_dir(&slot_root) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if !p.is_dir() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    let target = sess.prof.data_dir.join(&name);
                    let _ = std::fs::remove_dir_all(&target);
                    if copy::copy_snapshot_item(&p, &target) {
                        restored += 1;
                    }
                }
                sink.step("restore", StepStatus::Ok, "Intl 根级用户数据已恢复");
            }
        }
        let slot_sec = src.join("security");
        if slot_sec.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&slot_sec) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if !p.is_dir() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    let target = sess.prof.data_dir.join("security").join(&name);
                    let _ = std::fs::remove_dir_all(&target);
                    if copy::copy_snapshot_item(&p, &target) {
                        restored += 1;
                    }
                }
                sink.step("restore", StepStatus::Ok, "Intl security vault 已恢复");
            }
        }
    }

    // L3（仅 CodeBuddy）：回写 vscdb 登录真源。旧版快照（L3 落地前）无 vscdb →
    // 仅告警不硬失败（auth 文件仍恢复，但客户端登录态大概率不变——如实提示重新保存）。
    if matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI") {
        if let Some(gs_dir) = &sess.prof.cb_global_storage_dir {
            let slot_vscdb = src.join("vscdb");
            if slot_vscdb.join("state.vscdb").exists() {
                let _ = std::fs::create_dir_all(gs_dir);
                // 审查修复：恢复前先清掉现场残留的 -wal/-shm——旧 WAL 重放到刚恢复的
                // 主库会把目标账号的登录写回旧数据（SQLITE 回放语义），必须先删再拷。
                for stale in ["state.vscdb-wal", "state.vscdb-shm"] {
                    let _ = std::fs::remove_file(gs_dir.join(stale));
                }
                for f in [
                    "state.vscdb",
                    "state.vscdb-wal",
                    "state.vscdb-shm",
                    "state.vscdb.backup",
                    "storage.json",
                ] {
                    let src_f = slot_vscdb.join(f);
                    if src_f.exists() {
                        let _ = std::fs::copy(&src_f, gs_dir.join(f));
                        restored += 1;
                    }
                }
                let cop_src = slot_vscdb.join("tencent-cloud.coding-copilot");
                if cop_src.exists() {
                    let cop_dst = gs_dir.join("tencent-cloud.coding-copilot");
                    let _ = std::fs::remove_dir_all(&cop_dst);
                    if copy::copy_snapshot_item(&cop_src, &cop_dst) {
                        restored += 1;
                    }
                }
                sink.step(
                    "restore",
                    StepStatus::Ok,
                    "L3 vscdb 登录态已恢复（CodeBuddy CN globalStorage）",
                );
            } else {
                sink.step(
                    "restore",
                    StepStatus::Warn,
                    &format!(
                        "槽位 {slot} 为旧版快照（无 vscdb 登录态），CodeBuddy 无登录数据可恢复（共享 auth 文件属 WorkBuddy，F2-4 起不回写）——请在客户端登录目标账号后重新「保存当前登录态」升级快照"
                    ),
                );
            }
        }
    }
    sink.step(
        "restore",
        StepStatus::Ok,
        &format!("已恢复账号 {slot} 的登录态 ({restored} 项)"),
    );
    Ok(())
}

// ── 快照合成（issue #78 方案 A：冷启动死锁解除）────────────────────────────
//
// 「切换要快照 → 快照要保存 → 保存被 F2-5 守卫拒绝（客户端当前登录是别的账号）→
//  旧账号不会被自动登出」闭环成死锁（issue #78）。方案 A：无快照账号改由工具侧
// 凭证副本（token store，账号登录/OAuth 时自动留存）合成一份**最小可用**的
// authfile 快照（仅 L1 auth 文件 + meta），让「切换」能先恢复出该账号登录态；
// 客户端接受后 confirm_switch 确认，再由 switch_flow 调 backup_current 升级为
// 完整快照（rotate_bak 顺带把合成槽轮转为 .bak 留作回退）。

/// 递归键替换命中报告：各组是否至少改写一处（决定合成后是否需要补写默认键位）
#[derive(Default)]
struct RewriteHits {
    uid: bool,
    access: bool,
    refresh: bool,
    expires: bool,
    refresh_expires: bool,
}

/// 过期时间值形态保持：壳中原值是字符串（RFC3339 形态）则仍写字符串，
/// 否则写毫秒数；毫秒转 RFC3339 失败回退毫秒数
fn expires_value(orig: Option<&serde_json::Value>, ms: i64) -> serde_json::Value {
    if orig.is_some_and(serde_json::Value::is_string) {
        if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
            return json!(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
    }
    json!(ms)
}

/// 尝试把单个键按身份组替换（键名大小写不敏感；仅标量形态替换、仅非空值替换），
/// 返回是否替换。嵌套形态（键值仍是对象）返回 false 交由递归下钻。
fn replace_identity_key(
    k: &str,
    v: &mut serde_json::Value,
    creds: &Creds,
    hits: &mut RewriteHits,
) -> bool {
    let lk = k.to_ascii_lowercase();
    let scalar = v.is_string() || v.is_null() || v.is_number();
    let set_str = |v: &mut serde_json::Value, s: &str| *v = json!(s);
    if scalar && !creds.uid.is_empty() && matches!(lk.as_str(), "uid" | "userid" | "user_id") {
        set_str(v, &creds.uid);
        hits.uid = true;
        return true;
    }
    if scalar
        && !creds.access_token.is_empty()
        // 审查修复：不再匹配裸键 "token"——壳模板内语义无关的私有 token 字段
        // 会被误改写为新 access_token，违反「客户端私有字段不碰」原则
        && matches!(lk.as_str(), "accesstoken" | "access_token")
    {
        set_str(v, &creds.access_token);
        hits.access = true;
        return true;
    }
    if scalar
        && !creds.refresh_token.is_empty()
        && matches!(lk.as_str(), "refreshtoken" | "refresh_token")
    {
        set_str(v, &creds.refresh_token);
        hits.refresh = true;
        return true;
    }
    // 过期键保留原值形态（字符串 RFC3339 / 数字毫秒），仅换值；
    // expiresat/expires_at 为 RFC3339 形态键名（与 refreshexpiresat 对称覆盖）
    if scalar
        && creds.expires_at_ms.is_some()
        && matches!(
            lk.as_str(),
            "expiresatms" | "expires_at_ms" | "accesstokenexpiresatms" | "expires_in_ms"
                | "expiresat" | "expires_at"
        )
    {
        *v = expires_value(Some(v), creds.expires_at_ms.unwrap_or_default());
        hits.expires = true;
        return true;
    }
    if scalar
        && creds.refresh_expires_at_ms.is_some()
        && matches!(
            lk.as_str(),
            "refreshexpiresatms" | "refresh_expires_at_ms" | "refreshexpiresat"
        )
    {
        *v = expires_value(Some(v), creds.refresh_expires_at_ms.unwrap_or_default());
        hits.refresh_expires = true;
        return true;
    }
    if scalar && !creds.nickname.is_empty() && matches!(lk.as_str(), "nickname" | "nick_name") {
        set_str(v, &creds.nickname);
        return true;
    }
    if scalar
        && !creds.edition.is_empty()
        && matches!(lk.as_str(), "editiontype" | "edition_type" | "edition")
    {
        set_str(v, &creds.edition);
        return true;
    }
    if scalar && !creds.domain.is_empty() && lk == "domain" {
        set_str(v, &creds.domain);
        return true;
    }
    false
}

/// 递归替换壳内身份键：保持壳文件原有形态（嵌套位置/值类型/客户端私有字段一律
/// 不动，只改值），改写不了的形态原样保留（客户端私有校验字段不碰）
fn rewrite_identity_keys(j: &mut serde_json::Value, creds: &Creds) -> RewriteHits {
    let mut hits = RewriteHits::default();
    fn walk(j: &mut serde_json::Value, creds: &Creds, hits: &mut RewriteHits) {
        match j {
            serde_json::Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if !replace_identity_key(k, v, creds, hits) {
                        walk(v, creds, hits);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for v in items.iter_mut() {
                    walk(v, creds, hits);
                }
            }
            _ => {}
        }
    }
    walk(j, creds, &mut hits);
    // 裸键 token 兜底：live auth 文件实测存在裸键 `token` 形态的 access token
    //（issue #58 诊断，extract_access_token / wb_common::creds_of 的候选键均含
    // "token"）。主 walk 的 access 匹配集不含裸键（防壳内语义无关的私有 token
    // 字段被误改写），但壳模板只有裸键时若无此兜底：主 walk 不命中 → 补写块插入
    // 新 accessToken → 旧裸键 token 残留且过期时间指向新 token，三者共存，最坏
    // 导致客户端读到旧 token、切换无效。仅当整树未命中任何 access 键且凭证非空时
    // 二次遍历改写第一个标量 token 键（大小写不敏感）并置 hits.access = true，
    // 阻止补写块重复插 accessToken；壳内同时存在 accessToken 与私有 token 字段时
    // 不触发（主 walk 已命中），私有字段零接触——兼顾「私有字段不碰」与「裸键
    // 身份位必须改写」两个目标
    if !hits.access && !creds.access_token.is_empty() {
        fn walk_bare_token(j: &mut serde_json::Value, creds: &Creds, hit: &mut bool) {
            if *hit {
                return;
            }
            match j {
                serde_json::Value::Object(map) => {
                    for (k, v) in map.iter_mut() {
                        let scalar = v.is_string() || v.is_null() || v.is_number();
                        if k.eq_ignore_ascii_case("token") && scalar {
                            *v = json!(creds.access_token);
                            *hit = true;
                            return;
                        }
                        walk_bare_token(v, creds, hit);
                        if *hit {
                            return;
                        }
                    }
                }
                serde_json::Value::Array(items) => {
                    for v in items.iter_mut() {
                        walk_bare_token(v, creds, hit);
                        if *hit {
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
        let mut bare_hit = false;
        walk_bare_token(j, creds, &mut bare_hit);
        hits.access = bare_hit;
    }
    hits
}

/// 用凭证副本合成最小 authfile 快照（issue #78 方案 A 核心步骤）。
/// 壳模板优先取 live auth 文件（保留客户端私有字段与真实结构），无 live 文件
/// （首次使用/从未登录）时用默认最小壳。返回 Some(真实 uid) = 合成成功；
/// None = 无法合成（凭证缺失 / 槽位已存在——后者绝不覆盖，防污染）。
pub fn synthesize_auth_snapshot(
    sess: &Session,
    slot: &str,
    creds: &Creds,
    sink: &dyn ProgressSink,
) -> Option<String> {
    if creds.uid.is_empty() || creds.access_token.is_empty() {
        sink.step("bootstrap", StepStatus::Warn, "凭证缺失（uid 或 access_token 为空），无法合成");
        return None;
    }
    let dest = sess.prof.profiles_dir.join(slot);
    if dest.exists() {
        // 合成只发生在「该账号从无快照」场景；槽位已在 = 历史快照存在（可能污染），
        // 绝不覆盖，交由正常切换/守卫链路处理
        sink.step("bootstrap", StepStatus::Warn, &format!("槽位 {slot} 已存在快照，跳过合成"));
        return None;
    }
    // 壳模板：live auth 文件优先（解析失败/非对象回退默认壳）
    let mut shell: serde_json::Value = std::fs::read_to_string(&sess.auth_file)
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| json!({"account": {}, "auth": {}}));
    let hits = rewrite_identity_keys(&mut shell, &creds);

    // 壳缺失键位补写（默认壳/结构未知壳）：uid 按 auth_file_uid 四键兼容保证可读，
    // 凭证字段按 camelCase 惯例（与 token store / 实测 auth 文件形态一致）补齐
    if let Some(obj) = shell.as_object_mut() {
        if !hits.uid {
            obj.insert("uid".into(), json!(creds.uid));
            let acc = obj.entry("account").or_insert(json!({}));
            if let Some(a) = acc.as_object_mut() {
                a.insert("uid".into(), json!(creds.uid));
                if !creds.nickname.is_empty() {
                    a.insert("nickname".into(), json!(creds.nickname));
                }
                if !creds.edition.is_empty() {
                    a.insert("editionType".into(), json!(creds.edition));
                }
            }
        }
        if !hits.access || !hits.refresh || !hits.expires || !hits.refresh_expires {
            let auth = obj.entry("auth").or_insert(json!({}));
            if let Some(a) = auth.as_object_mut() {
                if !hits.access && !creds.access_token.is_empty() {
                    a.insert("accessToken".into(), json!(creds.access_token));
                }
                if !hits.refresh && !creds.refresh_token.is_empty() {
                    a.insert("refreshToken".into(), json!(creds.refresh_token));
                }
                if !hits.expires {
                    if let Some(ms) = creds.expires_at_ms {
                        a.insert("expiresAtMs".into(), json!(ms));
                    }
                }
                if !hits.refresh_expires {
                    if let Some(ms) = creds.refresh_expires_at_ms {
                        a.insert("refreshExpiresAtMs".into(), json!(ms));
                    }
                }
            }
        }
        if !creds.domain.is_empty() {
            // 壳内已有 domain 时 replace 已改写为新值，此处同值重插无副作用
            obj.insert("domain".into(), json!(creds.domain));
        }
    }
    // 自检：合成结果必须能被 auth_file_uid 读出身份（四键兼容），否则客户端与
    // 工具侧校验都无据可依，宁可不合成
    let uid_written = auth_file_uid_value(&shell);
    if uid_written.as_deref() != Some(creds.uid.as_str()) {
        sink.step(
            "bootstrap",
            StepStatus::Warn,
            "合成结果无法解析出目标 uid（壳结构不识别），已放弃合成",
        );
        return None;
    }

    // 落槽：仅 L1 auth 文件 + meta（L2/L3 属体验增强，由 confirm 后的完整备份补齐）；
    // 序列化失败绝不落空文件（空 auth 文件会被客户端视为损坏，宁可不合成）
    let body = match serde_json::to_string(&shell) {
        Ok(s) => s,
        Err(e) => {
            sink.step("bootstrap", StepStatus::Warn, &format!("合成快照序列化失败: {e}"));
            return None;
        }
    };
    let synth_auth_path = dest.join("auth").join(slot_auth_name(sess));
    if let Err(e) = std::fs::create_dir_all(dest.join("auth"))
        .map_err(|e| e.to_string())
        .and_then(|()| {
            std::fs::write(&synth_auth_path, &body)
                .map_err(|e| e.to_string())
        })
    {
        sink.step("bootstrap", StepStatus::Warn, &format!("合成快照写入失败: {e}"));
        return None;
    }
    // 合成 auth 文件含 accessToken/refreshToken 明文，std::fs::write 落盘权限为
    // umask 默认（通常 0644），mac 多用户机上其他用户可读——收紧为 0600
    //（备份/恢复走 fs::copy 会保留源权限，不受此影响）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &synth_auth_path,
            std::fs::Permissions::from_mode(0o600),
        );
    }
    let meta = json!({
        "schemaVersion": 1,
        "layout": "authfile",
        "app": sess.prof.app_name,
        "uid": creds.uid,
        "savedAt": crate::fs_utils::now_ts(),
        "savedAtMs": chrono::Utc::now().timestamp_millis(),
        "bootstrap": true,
    });
    // 原子写对齐 backup_authfile：中断不留半截 meta.json
    if let Err(e) = crate::fs_utils::write_text_atomic(
        &dest.join("meta.json"),
        &serde_json::to_string(&meta).unwrap_or_default(),
    )
    {
        // meta 缺失不阻塞合成结果：confirm 期望 uid 回退会话暂存 bootstrap_uid
        //（与 meta 的 uid 同源），后续完整备份会重写 meta
        sink.step("bootstrap", StepStatus::Warn, &format!("meta.json 写入失败: {e}"));
    }
    sink.step(
        "bootstrap",
        StepStatus::Ok,
        &format!("已用凭证副本合成账号 {slot} 的最小登录态快照（仅 auth 文件）"),
    );
    Some(creds.uid.clone())
}

/// 槽位是否带合成标记（meta.json `"bootstrap": true`，合成落槽时写入）。
/// 完整快照升级门控 `sess.bootstrap_done` 仅当次 run_action 会话有效；合成后切换
/// 未走完（进程退出 / 切换失败重试）时，下次会话切到同槽（槽位已存在，不再合成）
/// 成功确认也要能触发升级——以槽位持久化标记补判。backup_authfile 重写 meta 时
/// 不带该键，升级后标记自然清除。
pub fn slot_marked_bootstrap(sess: &Session, slot: &str) -> bool {
    std::fs::read_to_string(sess.prof.profiles_dir.join(slot).join("meta.json"))
        .ok()
        .and_then(|raw| {
            serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')).ok()
        })
        .and_then(|m| m.get("bootstrap").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// 读取 authfile 槽位 meta.json 的（快照保存时间 ms, 最近一次身份不符/回退 ms,
/// 最近一次确认超时 ms）。旧快照只有 `savedAt`（本地时间字符串）→ 宽松解析回填；
/// 损坏/缺失返回 (None, None, None)。
pub(crate) fn read_slot_snapshot_meta(
    profiles_dir: &std::path::Path,
    slot: &str,
) -> (Option<i64>, Option<i64>, Option<i64>) {
    let path = profiles_dir.join(slot).join("meta.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return (None, None, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')) else {
        return (None, None, None);
    };
    let saved_ms = v
        .get("savedAtMs")
        .and_then(|x| x.as_i64())
        .or_else(|| {
            v.get("savedAt")
                .and_then(|x| x.as_str())
                .and_then(crate::fs_utils::parse_ts_loose)
                .map(|s| s * 1000)
        });
    (
        saved_ms,
        v.get("verifyFailedAtMs").and_then(|x| x.as_i64()),
        v.get("verifyTimeoutAtMs").and_then(|x| x.as_i64()),
    )
}

/// 标记「切换到该账号时客户端未确认登录（身份不符/回退）」。只读改写槽位 meta.json；
/// 槽位不存在时静默跳过。
pub(crate) fn mark_snapshot_verify_failed(profiles_dir: &std::path::Path, slot: &str) {
    mark_slot_key(profiles_dir, slot, "verifyFailedAtMs");
}

/// 标记「切换后确认登录超时」（确定性不如身份不符：慢启动/NEED_REFRESH 也可能超时）。
/// 与 verifyFailedAtMs 分开落盘，前端据此用黄标而非红标，减少慢启动误报。
pub(crate) fn mark_snapshot_timeout(profiles_dir: &std::path::Path, slot: &str) {
    mark_slot_key(profiles_dir, slot, "verifyTimeoutAtMs");
}

fn mark_slot_key(profiles_dir: &std::path::Path, slot: &str, key: &str) {
    let path = profiles_dir.join(slot).join("meta.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')) else {
        return;
    };
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    // 最新一次复核结果覆盖旧标记：超时可把历史红标降级为黄标，反之亦然，
    // 避免上一轮的标记在下一轮已重新切换后永久残留
    let other = if key == "verifyFailedAtMs" {
        "verifyTimeoutAtMs"
    } else {
        "verifyFailedAtMs"
    };
    obj.remove(other);
    obj.insert(
        key.into(),
        serde_json::json!(chrono::Utc::now().timestamp_millis()),
    );
    let _ = crate::fs_utils::write_text_atomic(&path, &v.to_string());
}

/// 清除切换验证标记（身份不符 / 超时均清；确认登录成功或重新保存快照时）。
pub(crate) fn clear_snapshot_verify_failed(profiles_dir: &std::path::Path, slot: &str) {
    let path = profiles_dir.join(slot).join("meta.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')) else {
        return;
    };
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    let removed = obj.remove("verifyFailedAtMs").is_some()
        | obj.remove("verifyTimeoutAtMs").is_some();
    if removed {
        let _ = crate::fs_utils::write_text_atomic(&path, &v.to_string());
    }
}

/// 客户端「登出标记」路径：`{auth_file}.logged-out`（客户端登出时写入，启动时据此
/// 忽略已存登录态）。用 OsString 拼接，避免 display() 对非 UTF-8 路径 lossy。
fn logout_marker_path(auth_file: &Path) -> PathBuf {
    let mut p = auth_file.as_os_str().to_os_string();
    p.push(".logged-out");
    PathBuf::from(p)
}

/// 删除登出标记；返回 true = 确实删除了（供日志用）
fn clear_logout_marker(auth_file: &Path) -> bool {
    let marker = logout_marker_path(auth_file);
    if marker.exists() && std::fs::remove_file(&marker).is_ok() {
        return true;
    }
    false
}

/// 切换确认结果（PS 返回 'ok'/'timeout'）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerifyResult {
    Ok,
    Timeout,
    /// 信号④（仅 CodeBuddy）：客户端启动后 live 身份（storage.json genie.userId）
    /// 重写为非目标账号——vscdb mtime 只是「动过登录库」的因果信号，防不了
    /// 「会话回退到旧账号」的假阳性（switcher.log 实测：恢复 A 后 verify 报 OK，
    /// 45 秒后 live 身份仍是旧账号 B，守卫正确跳过槽位回写但用户拿到假「切换成功」）
    Reverted,
}

/// authfile 切换后轮询确认（F-02 验收项，超时 30s / 2s 间隔，PS 1156-1243 对译）。
/// 三信号，命中任一即确认：
///   ①数据目录 skeleton 登录快照 uid（仅 WorkBuddy 有，CodeBuddy 无）；
///   ②共享 auth 文件 uid —— 客户端启动后若接受恢复的登录会保持/重写目标 uid；
///     若被残留进程回写会退回旧 uid（实测切换"不生效"的形态），同样能被观测到；
///   ③vscdb mtime 因果信号（F2-3，仅 CodeBuddy）——CodeBuddy 登录真源在自身
///     vscdb、实测从不回写 auth 文件，信号②恒不触发导致 verify 恒超时告警。
///     改以客户端启动后实际重写自身登录库（state.vscdb mtime 偏离恢复锚点）计为
///     已接受恢复的登录态。
/// ②需连续两次轮询（间隔 2 秒）均命中才算确认，排除恢复刚落盘时的瞬时匹配；
/// 无 skeleton 的应用额外要求 mtime 偏离恢复落盘锚点——防恢复内容原样躺着的假阳性
///（实测 4 秒即"确认成功"而客户端根本未接受登录）。
/// fail-open：超时仅警告不判失败（快照已恢复、客户端已启动）。
/// 30 秒窗口是**故意保留**的：切换失败需要人工登录时，用户有 30 秒在客户端里登录，
/// 一旦登录成功信号①（快照 uid）立刻命中即确认。做得快的前提是「成功能立刻看见」，
/// 不是把窗口缩短——缩短会让「需要人工登录」的场景来不及确认。
/// skeleton 登录快照 → 当前登录 uid。
/// 现版本客户端结构：`{"primary":{"uid":"…","nickname":"…","savedAt":…}}`；
/// 旧结构把 uid 放顶层 / `account.uid` / `accountId`。
/// **2026-10-30 修复**：旧实现只读后三者，现版本一个都读不到 → 信号①恒不命中；
/// 而信号②要求客户端重写 auth 文件，实测客户端从不回写（auth mtime 停在旧日期），
/// 于是 WorkBuddy 切换「实际成功」也必然走到 30 秒超时。`primary` 优先、旧键兜底。
fn snapshot_uid_of(j: &serde_json::Value) -> Option<String> {
    snapshot_field(j, "uid", &["uid", "accountId"])
}

/// 登录快照字段取值（`primary` 优先，旧平铺键兜底）。
/// `pub(crate)`：账号列表/环境检测（commands/workbuddy/accounts.rs）复用同一份解析，
/// 避免两处各写一套键序而漂移。
pub(crate) fn snapshot_field(
    j: &serde_json::Value,
    key: &str,
    legacy: &[&str],
) -> Option<String> {
    j.get("primary")
        .and_then(|p| p.get(key))
        .and_then(serde_json::Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::fs_utils::dig(j, legacy)
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

pub fn confirm_switch(
    sess: &Session,
    slot: &str,
    sink: &dyn ProgressSink,
    launched_at: std::time::SystemTime,
) -> VerifyResult {
    let result = confirm_switch_inner(sess, slot, sink, launched_at);
    // 账号列表快照徽标数据源：身份不符/回退 → 红（verifyFailedAtMs）；超时 →
    // 黄（verifyTimeoutAtMs，fail-open 语义下慢启动也会超时，不与身份不符同判）；
    // 确认成功 → 两类标记都清除
    match result {
        VerifyResult::Ok => clear_snapshot_verify_failed(&sess.prof.profiles_dir, slot),
        VerifyResult::Reverted => mark_snapshot_verify_failed(&sess.prof.profiles_dir, slot),
        VerifyResult::Timeout => mark_snapshot_timeout(&sess.prof.profiles_dir, slot),
    }
    result
}

/// 切换后轮询确认（真正的实现，见上方注释）。
fn confirm_switch_inner(
    sess: &Session,
    slot: &str,
    sink: &dyn ProgressSink,
    launched_at: std::time::SystemTime,
) -> VerifyResult {
    let auth_file = &sess.auth_file;
    let snap_file = sess.prof.data_dir.join("storage").join("skeleton").join("account-snapshot.json");
    let meta_file = sess.prof.profiles_dir.join(slot).join("meta.json");
    // 期望 uid 回退链：CodeBuddy 系优先取**槽内** vscdb/storage.json 的 genie.userId
    //（恢复会把它原样拷进 live，是「恢复后应当登录谁」的权威答案；历史槽位 meta.uid
    // 曾被 WorkBuddy 共享 auth 文件的 uid 污染，用 meta 会误判「回退到其他账号」）
    // → meta.json → 会话暂存 bootstrap_uid（合成快照真实 uid，与 meta 同源；meta.json
    // 写失败不阻塞合成时的回退依据）→ slot 名（wb-<hash> 池 id ≠ uuid 体系真实 uid，
    // 仅作最后兜底——回退到它会让三信号确认恒不命中）
    let mut expect_uid: Option<String> =
        if matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI") {
            let raw = crate::fs_utils::read_json::<serde_json::Value>(
                &sess.prof.profiles_dir.join(slot).join("vscdb").join("storage.json"),
            );
            crate::fs_utils::dig(&raw, &["genie.userId"])
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
    if expect_uid.is_none() {
        // trim + 空串过滤对齐 read_slot_meta_uid：外部编辑/半截写入的 "uid":""
        // 若不过滤会变成 Some("")，短路 bootstrap_uid/槽名兜底 → 三信号比对恒
        // false，表现为「每次切该账号都 30s 超时（黄标）」
        expect_uid = std::fs::read_to_string(&meta_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')).ok())
            .and_then(|m| {
                m.get("uid")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            });
    }
    if expect_uid.is_none() {
        expect_uid = sess.bootstrap_uid.clone();
    }
    if expect_uid.is_none() {
        expect_uid = Some(slot.to_string());
    }
    let expect_uid = expect_uid.unwrap_or_default();
    let has_snap_dir = snap_file.parent().map(|p| p.exists()).unwrap_or(false);
    // F2-4 端隔离：CodeBuddy 恢复不再回写共享 auth 文件（该文件属 WorkBuddy 登录
    // 驱动源），信号②对其恒无意义且会误报「疑似被其他进程回写」——仅 WorkBuddy 检查
    let check_auth = !matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI");

    // F1-2 因果信号锚点：记录"恢复落盘后"的 auth 文件 mtime
    let restore_mtime = std::fs::metadata(&auth_file).and_then(|m| m.modified()).ok();
    // 信号③锚点（仅 CodeBuddy）：恢复落盘后的 vscdb mtime。仅在快照确实包含 vscdb
    // 登录真源（本次恢复覆盖了它）时启用——旧版快照（无 vscdb 槽）只恢复了 auth 文件，
    // 登录真源未变，客户端启动正常写 vscdb 也会翻动 mtime，贸然启用会假阳性确认。
    let slot_has_vscdb = matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI")
        && sess
            .prof
            .profiles_dir
            .join(slot)
            .join("vscdb")
            .join("state.vscdb")
            .exists();
    let vscdb_file = match (slot_has_vscdb, &sess.prof.cb_global_storage_dir) {
        (true, Some(gs)) => Some(gs.join("state.vscdb")),
        _ => None,
    };
    let vscdb_mtime = vscdb_file
        .as_deref()
        .and_then(|f| std::fs::metadata(f).ok())
        .and_then(|m| m.modified().ok());

    if !has_snap_dir {
        sink.step(
            "verify",
            StepStatus::Info,
            &format!(
                "{} 数据目录无登录快照（storage/skeleton 不存在），改以客户端实际重写信号确认（auth 重写或 vscdb 更新，静默不动不算确认）",
                sess.prof.app_name
            ),
        );
    } else {
        sink.step(
            "verify",
            StepStatus::Running,
            "等待客户端刷新登录快照（最长 30 秒）…",
        );
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut auth_hits = 0usize;
    let mut revert_warned = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        // 会话已死优先于一切「已重写」信号（2026-10-30 实测）：客户端启动后若因 401
        // 自行登出，同样会写 vscdb / auth 文件——不先判死，登出就会被当成切换成功
        //（橡皮3 槽位因此被回写成未登录态）。
        // 仅 CodeBuddy 提前判：它的日志按启动分目录、无跨会话污染；WorkBuddy 主线程
        // 日志跨启动累积（同一文件），启动初期尾部可能仍是上一次运行的内容，提前判
        // 有误报风险——WorkBuddy 仍由 `.logged-out` 标记（早判）+ 超时归因（晚判）覆盖。
        let check_dead_early = matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI");
        if check_dead_early && client_log_says_session_dead(&sess.prof.data_dir, Some(launched_at)) {
            sink.step(
                "verify",
                StepStatus::Warn,
                &format!(
                    "客户端本次启动已自行登出（登录会话被服务端注销）：恢复的登录态不可用，\
                     请重新登录 {} 后「保存当前登录态」（刷新凭证无效）",
                    sess.prof.app_name
                ),
            );
            return VerifyResult::Reverted;
        }
        // 信号②：共享 auth 文件 uid（仅 WorkBuddy；连续两次命中才确认）
        let auth_uid = if check_auth { auth_file_uid(&auth_file) } else { None };
        let mut auth_rewritten = true;
        if !has_snap_dir || check_auth {
            // 2026-10-30 实测修正：auth 文件就是我们刚复制进去的副本，uid 必然等于目标，
            // 「连续两次命中」会立刻成立（4 秒假成功 → 客户端其实没登录）。故 WorkBuddy
            // 系一律要求客户端确实重写过该文件（mtime 偏离恢复锚点），不再只在无 skeleton
            // 时要求。
            auth_rewritten = std::fs::metadata(&auth_file)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| Some(t) != restore_mtime)
                .unwrap_or(false);
        }
        // 登出标记重新出现 = 客户端拒绝恢复的登录态并主动登出（多半是服务端会话已注销）。
        // 恢复阶段已无条件清除该标记，此处再出现即客户端自己写的，直接判失败不再空等 30 秒。
        if check_auth && logout_marker_path(&auth_file).exists() {
            sink.step(
                "verify",
                StepStatus::Warn,
                "客户端已登出（检测到 .logged-out 标记）：恢复的登录态被拒绝，通常意味着服务端会话已被注销，需要重新登录该账号",
            );
            return VerifyResult::Reverted;
        }
        // 信号③（仅 CodeBuddy）：客户端启动后重写自身登录库（vscdb mtime 偏离恢复锚点）
        let vscdb_rewritten = match (vscdb_file.as_deref(), vscdb_mtime) {
            (Some(f), Some(anchor)) => std::fs::metadata(f)
                .and_then(|m| m.modified())
                .map(|t| t != anchor)
                .unwrap_or(false),
            _ => false,
        };

        if vscdb_rewritten
            || (auth_uid
                .as_deref()
                .map(|u| u == expect_uid)
                .unwrap_or(false)
                && auth_rewritten)
        {
            auth_hits += 1;
            if auth_hits >= 2 {
                return confirm_ok(sess, slot, &expect_uid, sink);
            }
        } else {
            auth_hits = 0;
            // 只在「uid 确实与目标不同」时告警：uid 相同但客户端尚未重写文件属于
            // 等待期（信号①会先确认），此前无条件告警会打出「与目标不一致」的假消息
            if check_auth
                && auth_uid.as_deref().is_some_and(|u| u != expect_uid)
                && !revert_warned
            {
                revert_warned = true;
                let short: String = auth_uid
                    .as_deref()
                    .unwrap_or_default()
                    .split('-')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                sink.step(
                    "verify",
                    StepStatus::Warn,
                    &format!(
                        "检测到 auth 文件 uid={short}… 与目标不一致（疑似被其他进程回写），持续观察中"
                    ),
                );
            }
        }
        // 信号①：skeleton 登录快照（数据目录存在才检查）
        if has_snap_dir && snap_file.exists() {
            if let Ok(raw) = std::fs::read_to_string(&snap_file) {
                if let Ok(j) = serde_json::from_str::<serde_json::Value>(
                    raw.trim_start_matches('\u{feff}'),
                ) {
                    if snapshot_uid_of(&j).as_deref() == Some(expect_uid.as_str()) {
                        return confirm_ok(sess, slot, &expect_uid, sink);
                    }
                }
            }
        }
    }
    if !has_snap_dir {
        // 快照含 vscdb（登录真源已恢复）时以"客户端是否重写登录数据"为判据；
        // 旧快照（无 vscdb）登录真源未变，超时则大概率未生效。
        if slot_has_vscdb {
            sink.step(
                "verify",
                StepStatus::Warn,
                &format!(
                    "30 秒内未观察到 {} 客户端重写登录数据（auth 文件与 vscdb 均无更新——客户端可能未启动或未接受恢复的登录态），请打开客户端核实登录身份",
                    sess.prof.app_name
                ),
            );
        } else {
            sink.step(
                "verify",
                StepStatus::Warn,
                &format!(
                    "30 秒内未观察到 {} 客户端重写 auth 文件为目标账号——切换很可能未生效（该客户端登录态不由 auth 文件驱动），请打开客户端核实；若未登录，请手动登录后「保存当前登录态」升级快照",
                    sess.prof.app_name
                ),
            );
        }
    } else {
        sink.step(
            "verify",
            StepStatus::Warn,
            "30 秒内未确认到目标 uid（客户端可能未启动/未联网），请打开客户端核实",
        );
    }
    // 归因（2026-10-30）：客户端日志里出现 invalid_grant / Offline user session /
    // Auth logout marker found → 服务端会话已被注销，本地怎么修都没用，只能重新登录。
    // 判红（而非黄）以免用户反复重试无效切换。
    if client_log_says_session_dead(&sess.prof.data_dir, Some(launched_at)) {
        sink.step(
            "verify",
            StepStatus::Warn,
            &format!(
                "客户端日志显示登录会话已被服务端注销（invalid_grant / 登出标记），恢复的登录态无法使用：\
                 请重新登录 {} 后「保存当前登录态」（刷新凭证无效）",
                sess.prof.app_name
            ),
        );
        return VerifyResult::Reverted;
    }
    VerifyResult::Timeout
}

/// 会话已死的判定标记：任一命中即认为本次启动的登录态已被服务端注销。
/// - WorkBuddy 主线程日志：刷新令牌被拒（invalid_grant）/ 离线会话 / 登出标记；
/// - CodeBuddy 插件日志：会话过期主动登出 / 刷新令牌 401（主进程只写 "Logout started"，
///   真正的判据在插件日志，2026-10-30 实测 橡皮3 即此形态）。
const SESSION_DEAD_MARKERS: [&str; 6] = [
    "invalid_grant",
    "Offline user session",
    "Auth logout marker found",
    "session expired logout triggered",
    "401 code logout",
    "/v2/plugin/auth/token/refresh | Status: 401",
];

/// 读日志尾部（最多 `tail_bytes`，非法 UTF-8 宽松解码）
fn read_log_tail(path: &Path, tail_bytes: usize) -> Option<String> {
    // 只读尾部：confirm 每 2s 轮询一次，全量 read 会在大日志（可上百 MB）上
    // 把 30s 确认窗口的 IO 放大几十倍。seek 到 len-tail 处读即可。
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(tail_bytes as u64);
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// 递归收集 dir 下最新的 `limit` 个 .log（按 mtime 降序；限深 4）
fn newest_log_files(dir: &Path, limit: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<(std::time::SystemTime, PathBuf)>, depth: usize) {
        if depth > 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else if p.extension().map(|x| x.eq_ignore_ascii_case("log")).unwrap_or(false) {
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    out.push((m, p));
                }
            }
        }
    }
    let mut all = Vec::new();
    walk(dir, &mut all, 0);
    all.sort_by(|a, b| b.0.cmp(&a.0));
    all.into_iter().take(limit).map(|(_, p)| p).collect()
}

/// 本次启动对应的日志文件（只认启动时间之后写过的，避免读到上一次运行的登出记录）：
/// - WorkBuddy：`logs/<日期>/workbuddyMainThread__*.log`（目录按天取最新文件；主线程
///   日志跨启动累积于同一文件（见 confirm_switch 早判禁用注释），启动初期尾部可能仍是
///   上一次运行的内容——晚判（30 秒超时后）依赖该窗口内客户端启动产生的大量追加把
///   旧记录挤出尾读窗口，极端空闲场景存在残余误判面，但归因仅影响提示文案的
///   红/黄分级，无破坏性副作用）；
/// - CodeBuddy：`logs/<YYYYMMDDTHHMMSS>/main.log` + 该会话目录下最新若干插件日志。
fn launch_log_files(logs_root: &Path, not_before: Option<std::time::SystemTime>) -> Vec<PathBuf> {
    let fresh = |m: std::time::SystemTime| not_before.map(|b| m >= b).unwrap_or(true);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut wb_best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut cb_best: Option<(std::time::SystemTime, PathBuf)> = None;
    let Ok(entries) = std::fs::read_dir(logs_root) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        // CodeBuddy 会话目录名：20261030T042344（日期+T+时间）
        let name = e.file_name().to_string_lossy().to_string();
        let is_cb_session = name.len() >= 9
            && name.contains('T')
            && name.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
        if is_cb_session {
            if let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) {
                if fresh(m) && cb_best.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                    cb_best = Some((m, p.clone()));
                }
            }
        }
        let Ok(files) = std::fs::read_dir(&p) else { continue };
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().to_string();
            if !fname.starts_with("workbuddyMainThread") {
                continue;
            }
            let Ok(m) = f.metadata().and_then(|m| m.modified()) else { continue };
            if fresh(m) && wb_best.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                wb_best = Some((m, f.path()));
            }
        }
    }
    if let Some((_, p)) = wb_best {
        out.push(p);
    }
    if let Some((_, dir)) = cb_best {
        let main = dir.join("main.log");
        if main.is_file() {
            out.push(main);
        }
        for p in newest_log_files(&dir, 8) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// 扫客户端本次启动的日志，判断是否出现「登录会话已被服务端注销」证据。
/// `not_before`：启动锚点（取 start_app 之前的时间），用于排除上一次运行的残留日志。
pub(crate) fn client_log_says_session_dead(
    data_dir: &Path,
    not_before: Option<std::time::SystemTime>,
) -> bool {
    let logs_root = data_dir.join("logs");
    launch_log_files(&logs_root, not_before).iter().any(|p| {
        read_log_tail(p, 128 * 1024)
            .map(|tail| SESSION_DEAD_MARKERS.iter().any(|m| tail.contains(m)))
            .unwrap_or(false)
    })
}

/// 确认收尾 + 信号④ live 身份复核（仅 CodeBuddy）。
/// vscdb mtime 只证明「客户端动过登录库」，防不了「客户端启动后会话回退到旧账号」
/// 的假阳性（switcher.log 实测：恢复 A 后 verify 报 OK，45 秒后 live 身份仍是旧
/// 账号 B）。恢复落盘时 live 身份（globalStorage\storage.json genie.userId）即目标
/// uid——客户端若回退必然重写它 → 轮询 3 次（2s 间隔），读到非目标非空 uid 即判
/// Reverted；读到目标 uid 或 3 次无定论（客户端暂未回写）则 fail-open 维持确认。
/// 前提守卫：仅当槽位快照含 storage.json（恢复确实覆盖了 live 身份）才启用复核——
/// 槽位缺该文件时 live storage.json 是切换前旧账号残留，读到非目标 uid 只是
/// 「客户端尚未回写」，判 Reverted 即假阳性。
/// WorkBuddy 信号②本就是内容级校验，直接确认（零额外延迟）。
fn confirm_ok(sess: &Session, slot: &str, expect_uid: &str, sink: &dyn ProgressSink) -> VerifyResult {
    if !matches!(sess.prof.app_name, "CodeBuddy" | "CodeBuddy AI") {
        sink.step("verify", StepStatus::Ok, "登录身份已确认为目标账号");
        return VerifyResult::Ok;
    }
    let Some(gs) = sess.prof.cb_global_storage_dir.as_ref() else {
        sink.step("verify", StepStatus::Ok, "登录身份已确认为目标账号");
        return VerifyResult::Ok;
    };
    if !sess.prof.profiles_dir.join(slot).join("vscdb").join("storage.json").exists() {
        sink.step("verify", StepStatus::Ok, "登录身份已确认为目标账号");
        return VerifyResult::Ok;
    }
    let live_file = gs.join("storage.json");
    for _ in 0..3 {
        std::thread::sleep(Duration::from_secs(2));
        let raw = crate::fs_utils::read_json::<serde_json::Value>(&live_file);
        // trim 对齐 slot_held_uid/live_login_uid 的口径：尾随空白会让两侧比对失配
        let uid = crate::fs_utils::dig(&raw, &["genie.userId"])
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !uid.is_empty() {
            if uid == expect_uid {
                sink.step("verify", StepStatus::Ok, "登录身份已确认为目标账号");
                return VerifyResult::Ok;
            }
            sink.step(
                "verify",
                StepStatus::Warn,
                &format!(
                    "客户端实际登录身份为 {uid}，与目标 {expect_uid} 不一致（疑似会话回退到旧账号），请打开客户端核实"
                ),
            );
            return VerifyResult::Reverted;
        }
    }
    sink.step("verify", StepStatus::Ok, "登录身份已确认为目标账号");
    VerifyResult::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switcher::{test_io_lock, Action, RunArgs, TargetApp};

    struct QuietSink;
    impl ProgressSink for QuietSink {
        fn step(&self, _: &str, _: StepStatus, _: &str) {}
    }

    /// 登出标记路径与清理（2026-10-30 实测事故的回归锁）：
    /// 标记存在时客户端会忽略恢复的登录态，恢复流程必须能把它清掉。
    #[test]
    fn 登出标记路径与清理() {
        let dir = std::env::temp_dir().join(format!(
            "sw-auth-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = dir.join(WB_AUTH_FILE_NAME);
        std::fs::write(&auth, "{}").unwrap();
        let marker = logout_marker_path(&auth);
        assert_eq!(
            marker.file_name().unwrap().to_string_lossy(),
            "workbuddy-desktop.info.logged-out"
        );
        // 无标记 → 不报删除
        assert!(!clear_logout_marker(&auth));
        std::fs::write(&marker, "uuid").unwrap();
        assert!(clear_logout_marker(&auth), "标记存在时必须删除成功");
        assert!(!marker.exists(), "标记应已被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Session 构造：auth 路径注入临时目录（不触碰真实 auth 文件），
    /// 目录名由 test_temp_dir 计数器保证唯一，先清再建
    fn session(app: TargetApp, tag: &str) -> Session {
        let data = crate::fs_utils::test_temp_dir(&format!("sw-auth-{tag}"));
        let _ = std::fs::remove_dir_all(&data);
        let mut args = RunArgs {
            action: Action::Switch,
            target_app: app,
            user_id: Some("100".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: data.clone(),
        };
        args.include_indexeddb = false;
        let mut sess = Session::new(&args);
        // 关键隔离：应用数据目录（~\.workbuddy）与 auth 文件路径均来自环境变量
        //（真实路径），测试统一覆盖到临时目录
        sess.prof.data_dir = data.join("appdata");
        let fake_auth_dir = data.join("fake-auth");
        sess.auth_dir = fake_auth_dir.clone();
        sess.auth_file = fake_auth_dir.join(WB_AUTH_FILE_NAME);
        sess
    }

    #[test]
    fn auth_file_uid_四键兼容() {
        let _guard = test_io_lock();
        let base = crate::fs_utils::test_temp_dir("sw-authuid");
        std::fs::create_dir_all(&base).unwrap();
        let p = base.join("auth.info");
        for (content, want) in [
            (r#"{"account":{"uid":"u1"}}"#, Some("u1")),
            (r#"{"uid":"u2"}"#, Some("u2")),
            (r#"{"auth":{"account":{"uid":"u3"}}}"#, Some("u3")),
            (r#"{"auth":{"uid":"u4"}}"#, Some("u4")),
            (r#"{"other":1}"#, None),
            ("非 JSON", None),
        ] {
            std::fs::write(&p, content).unwrap();
            assert_eq!(auth_file_uid(&p).as_deref(), want, "case {content}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 备份auth缺失整体跳过_恢复缺auth中止() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "skip");
        let sink = QuietSink;
        // auth 文件不存在 → warn 整体跳过（不建槽/不写 meta/不动 .bak）
        backup_authfile(&sess, "100", &sink).unwrap();
        assert!(!sess.prof.profiles_dir.join("100").exists());
        // 无快照恢复 → Err
        assert!(restore_authfile(&sess, "100", &sink).is_err());
        // 建快照但缺 auth 文件 → 中止
        std::fs::create_dir_all(sess.prof.profiles_dir.join("100")).unwrap();
        assert!(restore_authfile(&sess, "100", &sink).is_err());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 2026-10-30 实测形态：客户端把无效会话回退到账号 A（live uid 变成 A），流程仍按
    /// 「检测到的目标 A'」把现场写进 A' 槽位 → 槽里装的是 A 的登录态（meta.uid 与槽位
    /// 账号不符，切回来必失败）。守卫必须在写入前拒绝，且不动原槽位。
    #[test]
    fn 备份拒绝对身份不一致的具名槽位写入() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "identity-guard");
        let sink = QuietSink;
        let slot = "100";
        let slot_dir = sess.prof.profiles_dir.join(slot);
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("meta.json"), r#"{"uid":"uid-x"}"#).unwrap();
        std::fs::create_dir_all(&sess.auth_dir).unwrap();
        std::fs::write(&sess.auth_file, r#"{"account":{"uid":"uid-y"}}"#).unwrap();

        backup_authfile(&sess, slot, &sink).expect("身份不符应跳过并返回 Ok");
        let meta = std::fs::read_to_string(slot_dir.join("meta.json")).unwrap();
        assert!(meta.contains("uid-x"), "槽位 meta 不得被改写：{meta}");
        assert!(
            !slot_dir.join("auth").join(WB_AUTH_FILE_NAME).exists(),
            "身份不符时不得写入 auth 文件"
        );

        // last 是「当前现场」备份槽，不参与身份对齐守卫
        backup_authfile(&sess, "last", &sink).unwrap();
        assert!(sess
            .prof
            .profiles_dir
            .join("last")
            .join("auth")
            .join(WB_AUTH_FILE_NAME)
            .exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 2026-11-01 审查修复回归：live_login_uid WorkBuddy 系 skeleton 优先——客户端
    /// 登录后只重写 skeleton 快照、不回写 auth 文件（auth 停在上个账号），槽位身份
    /// 守卫必须采用 skeleton primary.uid；skeleton 缺失/损坏才回退 auth 文件 uid。
    #[test]
    fn 备份守卫采用skeleton身份而非陈旧auth文件() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "skeleton-live");
        let sink = QuietSink;
        // live 现场两层信号刻意错开：skeleton primary.uid = uid-skel（客户端刚登录），
        // auth 文件停留上个账号 uid-auth（客户端从不回写）
        let skeleton = sess.prof.data_dir.join("storage").join("skeleton");
        std::fs::create_dir_all(&skeleton).unwrap();
        std::fs::write(
            skeleton.join("account-snapshot.json"),
            r#"{"primary":{"uid":"uid-skel","nickname":"K","savedAt":123}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(&sess.auth_dir).unwrap();
        std::fs::write(&sess.auth_file, r#"{"account":{"uid":"uid-auth"}}"#).unwrap();

        // 槽位 meta.uid = uid-skel：守卫按 skeleton 判一致 → 放行
        //（若误用 auth 文件的 uid-auth 会误拦）
        let slot = "100";
        let slot_dir = sess.prof.profiles_dir.join(slot);
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("meta.json"), r#"{"uid":"uid-skel"}"#).unwrap();
        backup_authfile(&sess, slot, &sink).unwrap();
        assert!(
            slot_dir.join("auth").join(WB_AUTH_FILE_NAME).exists(),
            "skeleton 身份与槽位一致时应放行备份"
        );

        // 对照：槽位 meta.uid = uid-auth（auth 文件视角一致、skeleton 视角不符）
        // → 守卫按 skeleton 身份拒绝，且不动原槽位
        let _ = std::fs::remove_dir_all(&slot_dir);
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("meta.json"), r#"{"uid":"uid-auth"}"#).unwrap();
        backup_authfile(&sess, slot, &sink).unwrap();
        let meta = std::fs::read_to_string(slot_dir.join("meta.json")).unwrap();
        assert!(
            meta.contains("uid-auth"),
            "守卫按 skeleton 身份拒绝时槽位 meta 不得被改写：{meta}"
        );
        assert!(
            !slot_dir.join("auth").join(WB_AUTH_FILE_NAME).exists(),
            "skeleton 身份不符时不得写入 auth 文件"
        );
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 2026-11-02 合并审查修复回归：存量 CodeBuddy 槽 meta.uid 被 WorkBuddy uid
    /// 污染（2026-10-25 实测），槽内 vscdb 登录态完好——槽位身份守卫必须按
    /// slot_held_uid（槽内 genie.userId 优先）而非 meta.uid 判定：守卫放行备份，
    /// meta 随之自愈为 live 身份；账号列表也不再误标「快照不可用」。
    #[test]
    fn codebuddy槽身份按槽内genie判定而非被污染的meta() {
        let _guard = test_io_lock();
        let mut sess = session(TargetApp::CodeBuddy, "cb-genie-held");
        let sink = QuietSink;
        // live 现场：genie.userId = uid-cb（登录真源；storage.json 为平铺键
        // "genie.userId"，dig 按字面键查找）；共享 auth 文件被 WorkBuddy
        // 侧覆盖成 uid-wb（CB 系仅作 live_login_uid 兜底，真源可用时不参与判定）
        let gs = sess.data_dir.join("fake-gs");
        sess.prof.cb_global_storage_dir = Some(gs.clone());
        std::fs::create_dir_all(&gs).unwrap();
        std::fs::write(gs.join("storage.json"), r#"{"genie.userId":"uid-cb"}"#).unwrap();
        std::fs::create_dir_all(&sess.auth_dir).unwrap();
        std::fs::write(&sess.auth_file, r#"{"account":{"uid":"uid-wb"}}"#).unwrap();

        // 槽位：meta.uid 被污染成 uid-wb，但槽内 vscdb/storage.json 登录态完好（uid-cb）
        let slot = "100";
        let slot_dir = sess.prof.profiles_dir.join(slot);
        std::fs::create_dir_all(slot_dir.join("vscdb")).unwrap();
        std::fs::write(slot_dir.join("meta.json"), r#"{"uid":"uid-wb"}"#).unwrap();
        std::fs::write(
            slot_dir.join("vscdb").join("storage.json"),
            r#"{"genie.userId":"uid-cb"}"#,
        )
        .unwrap();

        assert_eq!(slot_held_uid(&slot_dir, true).as_deref(), Some("uid-cb"));
        backup_authfile(&sess, slot, &sink).unwrap();
        assert!(
            slot_dir.join("auth").join(WB_AUTH_FILE_NAME).exists(),
            "槽内 genie 与 live 一致时应放行备份（不得按污染 meta 误拦）"
        );
        let meta = std::fs::read_to_string(slot_dir.join("meta.json")).unwrap();
        assert!(
            meta.contains("uid-cb"),
            "备份放行后 meta 应自愈为 live 身份：{meta}"
        );
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    #[test]
    fn 备份恢复round_trip_含meta与_l2() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "roundtrip");
        let sink = QuietSink;
        // Live：auth 文件 + L2 用户数据目录（注入的临时 auth 路径）
        std::fs::create_dir_all(&sess.auth_dir).unwrap();
        std::fs::write(&sess.auth_file, r#"{"uid":"100"}"#).unwrap();
        std::fs::create_dir_all(sess.prof.data_dir.join("storage").join("user-100")).unwrap();
        std::fs::write(sess.prof.data_dir.join("storage").join("user-100").join("kv"), "v")
            .unwrap();

        backup_authfile(&sess, "100", &sink).unwrap();
        let slot = sess.prof.profiles_dir.join("100");
        assert_eq!(
            auth_file_uid(&slot.join("auth").join(WB_AUTH_FILE_NAME)).as_deref(),
            Some("100")
        );
        assert!(slot.join("storage").join("user-100").join("kv").exists());
        // meta.json 无 BOM 且字段齐全
        let meta_raw = std::fs::read_to_string(slot.join("meta.json")).unwrap();
        assert!(!meta_raw.starts_with('\u{feff}'));
        let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap();
        assert_eq!(meta["schemaVersion"], 1);
        assert_eq!(meta["layout"], "authfile");
        assert_eq!(meta["uid"], "100");

        // 破坏 Live 后恢复
        std::fs::remove_dir_all(sess.prof.data_dir.join("storage")).unwrap();
        std::fs::remove_file(&sess.auth_file).unwrap();
        restore_authfile(&sess, "100", &sink).unwrap();
        assert_eq!(auth_file_uid(&sess.auth_file).as_deref(), Some("100"));
        assert!(sess
            .prof
            .data_dir
            .join("storage")
            .join("user-100")
            .join("kv")
            .exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    // ── synthesize_auth_snapshot（issue #78 方案 A）────────────────────────────

    fn test_creds() -> Creds {
        Creds {
            uid: "new-uid".into(),
            access_token: "new-a".into(),
            refresh_token: "new-r".into(),
            expires_at_ms: Some(1_760_000_000_000),
            refresh_expires_at_ms: Some(1_790_000_000_000),
            nickname: "测试昵称".into(),
            edition: "pro".into(),
            domain: "example.com".into(),
        }
    }

    /// live auth 文件存在 → 作壳模板，身份键全替换（递归、大小写不敏感、值形态保持），
    /// 客户端私有字段不碰，meta 带 bootstrap 标记
    #[test]
    fn 合成快照_壳模板身份键全替换() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "synth");
        let sink = QuietSink;
        std::fs::create_dir_all(&sess.auth_dir).unwrap();
        std::fs::write(
            &sess.auth_file,
            r#"{"account":{"uid":"old","nickname":"old-n","editionType":"free"},"auth":{"accessToken":"old-a","refreshToken":"old-r","expiresAtMs":111,"refreshExpiresAtMs":222},"domain":"old","vendor":{"marker":{"color":"keep"}}}"#,
        )
        .unwrap();
        let slot = "wb-abc123";
        assert_eq!(
            synthesize_auth_snapshot(&sess, slot, &test_creds(), &sink).as_deref(),
            Some("new-uid")
        );
        let f = sess.prof.profiles_dir.join(slot).join("auth").join(WB_AUTH_FILE_NAME);
        let j: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(j["account"]["uid"], "new-uid");
        assert_eq!(j["account"]["nickname"], "测试昵称");
        assert_eq!(j["account"]["editionType"], "pro");
        assert_eq!(j["auth"]["accessToken"], "new-a");
        assert_eq!(j["auth"]["refreshToken"], "new-r");
        // 数字形态保持（原值是数字 → 仍写毫秒数）
        assert_eq!(j["auth"]["expiresAtMs"], 1_760_000_000_000_i64);
        assert_eq!(j["auth"]["refreshExpiresAtMs"], 1_790_000_000_000_i64);
        assert_eq!(j["domain"], "example.com");
        // 客户端私有字段不碰（递归替换只命中身份键，非身份键原样保留）
        assert_eq!(j["vendor"]["marker"]["color"], "keep");
        // meta 带 bootstrap 标记与目标 uid
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(sess.prof.profiles_dir.join(slot).join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["bootstrap"], true);
        assert_eq!(meta["uid"], "new-uid");
        assert_eq!(meta["layout"], "authfile");
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 无 live auth 文件（首次使用/从未登录）→ 默认最小壳，合成结果必须能被
    /// auth_file_uid 四键兼容读出身份，凭证字段按 camelCase 惯例补齐
    #[test]
    fn 合成快照_无live文件回退默认壳() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "synthdef");
        let sink = QuietSink;
        let slot = "wb-def456";
        assert_eq!(
            synthesize_auth_snapshot(&sess, slot, &test_creds(), &sink).as_deref(),
            Some("new-uid")
        );
        let f = sess.prof.profiles_dir.join(slot).join("auth").join(WB_AUTH_FILE_NAME);
        assert_eq!(auth_file_uid(&f).as_deref(), Some("new-uid"));
        let j: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(j["uid"], "new-uid");
        assert_eq!(j["account"]["uid"], "new-uid");
        assert_eq!(j["auth"]["accessToken"], "new-a");
        assert_eq!(j["auth"]["refreshToken"], "new-r");
        assert_eq!(j["auth"]["expiresAtMs"], 1_760_000_000_000_i64);
        assert_eq!(j["auth"]["refreshExpiresAtMs"], 1_790_000_000_000_i64);
        assert_eq!(j["domain"], "example.com");
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 槽位已存在 → None 且原内容绝不动（防污染）；凭证缺失（uid/access_token 空）→
    /// None 且不建槽
    #[test]
    fn 合成快照_槽位已存在或凭证缺失拒绝() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "synthdeny");
        let sink = QuietSink;
        let slot_dir = sess.prof.profiles_dir.join("wb-xyz");
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("keep.txt"), "origin").unwrap();
        assert!(synthesize_auth_snapshot(&sess, "wb-xyz", &test_creds(), &sink).is_none());
        assert_eq!(std::fs::read_to_string(slot_dir.join("keep.txt")).unwrap(), "origin");
        let no_uid = Creds { uid: String::new(), ..test_creds() };
        assert!(synthesize_auth_snapshot(&sess, "wb-new", &no_uid, &sink).is_none());
        let no_tok = Creds { access_token: String::new(), ..test_creds() };
        assert!(synthesize_auth_snapshot(&sess, "wb-new", &no_tok, &sink).is_none());
        assert!(!sess.prof.profiles_dir.join("wb-new").exists());
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    /// 2026-10-30：现版本客户端快照把身份嵌在 `primary` 下。旧实现只读顶层
    /// `uid`/`account.uid`/`accountId` → 返回 None → 信号①恒不命中，WorkBuddy
    /// 实际切成功也只会在 30 秒后报「未确认」。
    #[test]
    fn snapshot_uid_reads_primary_and_legacy_shapes() {
        // 本机 2026-10-30 实测结构
        let cur = serde_json::json!({
            "primary": {
                "version": 1,
                "uid": "a5e7e8f5-d1ff-4d54-a106-a54c5b3cb2e4",
                "nickname": "橡皮",
                "savedAt": 1_793_303_935_381_i64
            }
        });
        assert_eq!(
            snapshot_uid_of(&cur).as_deref(),
            Some("a5e7e8f5-d1ff-4d54-a106-a54c5b3cb2e4"),
            "primary.uid 必须能读到（本次修复的核心）"
        );
        // 旧平铺结构保持兼容
        assert_eq!(snapshot_uid_of(&serde_json::json!({"uid": "u1"})).as_deref(), Some("u1"));
        assert_eq!(
            snapshot_uid_of(&serde_json::json!({"account": {"uid": "u2"}})).as_deref(),
            Some("u2")
        );
        assert_eq!(
            snapshot_uid_of(&serde_json::json!({"accountId": "u3"})).as_deref(),
            Some("u3")
        );
        // 空串 / 缺失不得产生假命中
        assert_eq!(snapshot_uid_of(&serde_json::json!({"primary": {"uid": ""}})), None);
        assert_eq!(snapshot_uid_of(&serde_json::json!({})), None);
    }

    /// 临时日志根目录（测试隔离）
    fn tmp_log_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sw-wblogs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 2026-10-30 实测形态：CodeBuddy 会话被服务端注销后，客户端启动 4 秒即自行登出。
    /// 主进程日志只有 "Logout started"（不足以判定），插件日志才有 401 刷新失败 → 必须判死，
    /// 否则「登出写 vscdb」会被当成切换成功。
    #[test]
    fn session_dead_detects_codebuddy_logout() {
        let _guard = test_io_lock();
        let dir = tmp_log_dir("cb-dead");
        let sess = dir.join("logs").join("20261030T042344");
        let ext = sess.join("window1").join("exthost").join("Tencent-Cloud.coding-copilot");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(
            sess.join("main.log"),
            "2026-10-30 04:23:48.991 [AuthMainService][IDE-NATIVE-AUTH] Logout started in IDE main process\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !client_log_says_session_dead(&dir, None),
            "只有 Logout started、没有 401 归因时不得判死（防误判）"
        );
        std::fs::write(
            ext.join("腾讯云代码助手.log"),
            "2026-10-30 04:23:48.925 401 code logout, url: /v2/plugin/auth/token/refresh\n\
             2026-10-30 04:23:48.925 [AuthenticationManager] session expired logout triggered\n",
        )
        .unwrap();
        assert!(client_log_says_session_dead(&dir, None), "401 刷新失败 + 主动登出 = 会话已死");
        // 启动锚点晚于日志写入 → 属于上一次运行的残留，不判死
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        assert!(!client_log_says_session_dead(&dir, Some(later)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WorkBuddy 侧：主线程日志出现 invalid_grant / 登出标记即判死
    #[test]
    fn session_dead_detects_workbuddy_invalid_grant() {
        let _guard = test_io_lock();
        let dir = tmp_log_dir("wb-dead");
        let day = dir.join("logs").join("2026-10-30");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("workbuddyMainThread__abc.log"),
            "token refresh failed: invalid_grant\n",
        )
        .unwrap();
        assert!(client_log_says_session_dead(&dir, None));
        // 换一天的新目录（无标记）→ 不判死
        let day2 = dir.join("logs").join("2026-10-31");
        std::fs::create_dir_all(&day2).unwrap();
        std::fs::write(day2.join("workbuddyMainThread__def.log"), "all good\n").unwrap();
        assert!(
            !client_log_says_session_dead(&dir, None),
            "只认最新一次启动的日志，旧会话的登出记录不得跨会话误判"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-10-30 回归：现版本客户端只重写 skeleton 快照（primary.uid），不回写 auth 文件。
    /// 确认必须在第一个轮询周期就成功，而不是空等 30 秒后报「未确认」。
    #[test]
    fn 确认信号一_primary快照首轮即确认() {
        let _guard = test_io_lock();
        let sess = session(TargetApp::WorkBuddy, "confirm-primary");
        let sink = QuietSink;
        let target = "a5e7e8f5-0000-4000-8000-000000000001";
        let slot = "wb-target";
        let slot_dir = sess.prof.profiles_dir.join(slot);
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("meta.json"), format!(r#"{{"uid":"{target}"}}"#)).unwrap();
        // 恢复落盘的 auth 文件（uid 已是目标；客户端不会再写它 → mtime 不变）
        std::fs::create_dir_all(&sess.auth_dir).unwrap();
        std::fs::write(
            &sess.auth_file,
            format!(r#"{{"account":{{"uid":"{target}"}}}}"#),
        )
        .unwrap();
        // 客户端接受恢复的登录态后重写 skeleton 快照（现版本 primary 包裹）
        let snap = sess
            .prof
            .data_dir
            .join("storage")
            .join("skeleton")
            .join("account-snapshot.json");
        std::fs::create_dir_all(snap.parent().unwrap()).unwrap();
        std::fs::write(
            &snap,
            format!(r#"{{"primary":{{"uid":"{target}","savedAt":1793303935381}}}}"#),
        )
        .unwrap();
        let t0 = std::time::Instant::now();
        let r = confirm_switch(&sess, slot, &sink, std::time::SystemTime::now());
        assert!(
            matches!(r, VerifyResult::Ok),
            "primary 快照命中应确认成功（修复前信号①恒不命中，只能 30s 超时）"
        );
        assert!(
            t0.elapsed().as_secs() < 10,
            "应在第一个轮询周期内确认，实际 {:?}",
            t0.elapsed()
        );
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }
}
