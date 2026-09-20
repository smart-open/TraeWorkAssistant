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

// 路径常量（与 commands/workbuddy/common.rs::auth_file_path 同值，注释互指；
// CodeBuddyExtension 宿主目录 + workbuddy 产品线文件名，实测确认）
#[cfg(windows)] // mac 分支走 Application Support 基根（不含反斜杠相对段）
const WB_AUTH_DIR_REL: &str = "CodeBuddyExtension\\Data\\Public\\auth";
const WB_AUTH_FILE_NAME: &str = "workbuddy-desktop.info";

pub(crate) fn wb_auth_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .map(|l| PathBuf::from(l).join(WB_AUTH_DIR_REL))
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
    for v in [
        j.get("account").and_then(|a| a.get("uid")),
        j.get("uid"),
        j.get("auth").and_then(|a| a.get("account")).and_then(|a| a.get("uid")),
        j.get("auth").and_then(|a| a.get("uid")),
    ] {
        if let Some(v) = v {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// 备份（PS Backup-AuthFileProfile 965-1060 对译）
pub fn backup_authfile(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let auth_file = &sess.auth_file;
    // 关闭客户端后先等 auth 文件静默（残留进程可能延迟回写旧登录，实测 3 秒后才落盘）
    wait_auth_file_quiet(&auth_file, sink);
    let dest = sess.prof.profiles_dir.join(slot);
    if !auth_file.exists() {
        // 前置早退：首次使用/从未登录场景（区别于复制失败）——不建槽、不写 meta、
        // 不动 .bak，整体返回
        sink.step(
            "backup",
            StepStatus::Warn,
            &format!("auth 文件不存在（可能从未登录）：{}", auth_file.display()),
        );
        return Ok(());
    }
    // 单代回滚保护（对齐豆包：覆盖前挪 .bak）
    copy::rotate_bak(&dest, slot, sink);
    std::fs::create_dir_all(dest.join("auth")).map_err(|e| format!("创建快照目录失败: {e}"))?;
    let mut copied = 0usize;

    // L1 必选：auth 文件
    match std::fs::copy(&auth_file, dest.join("auth").join(WB_AUTH_FILE_NAME)) {
        Ok(_) => {
            copied += 1;
            sink.step("backup", StepStatus::Ok, "L1 auth 文件已备份");
        }
        Err(e) => {
            sink.step("backup", StepStatus::Error, &format!("auth 文件备份失败: {e}"));
            return Err(super::thrown(sink, "auth 文件备份失败"));
        }
    }

    // L2 体验：~\<app_data>\storage\user-<uid>* 目录
    let uid = auth_file_uid(&auth_file);
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
                "auth 文件中未能解析 uid（JSON 结构变化？），L2 跳过",
            );
        }
    }

    // L3（仅 CodeBuddy）：vscdb 登录真源快照。CodeBuddy CN 桌面端登录态在
    // %APPDATA%\CodeBuddy CN\User\globalStorage（state.vscdb 的 secret://…
    // planning-genie.new.accessTokencn，实测确认），仅备份共享 auth 文件对它无效
    //（root cause：切 CodeBuddy "还是最初的登录账号"）。含 backup 库与扩展存储目录
    //（防 VS Code 启动从 backup/扩展缓存恢复旧登录）。客户端已由 stop_app 关闭，
    // vscdb 无锁可整文件复制。
    if sess.prof.app_name == "CodeBuddy" {
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
    });
    if let Err(e) =
        std::fs::write(dest.join("meta.json"), serde_json::to_string(&meta).unwrap_or_default())
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
    let auth_src = src.join("auth").join(WB_AUTH_FILE_NAME);
    if !auth_src.exists() {
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
    // WorkBuddy 恢复保持原语义。
    let is_codebuddy = sess.prof.app_name == "CodeBuddy";
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

    // L3（仅 CodeBuddy）：回写 vscdb 登录真源。旧版快照（L3 落地前）无 vscdb →
    // 仅告警不硬失败（auth 文件仍恢复，但客户端登录态大概率不变——如实提示重新保存）。
    if sess.prof.app_name == "CodeBuddy" {
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
pub fn confirm_switch(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> VerifyResult {
    let auth_file = &sess.auth_file;
    let snap_file = sess.prof.data_dir.join("storage").join("skeleton").join("account-snapshot.json");
    let meta_file = sess.prof.profiles_dir.join(slot).join("meta.json");
    // 期望 uid：meta.json 优先，缺失回退 slot 名
    let mut expect_uid: Option<String> = std::fs::read_to_string(&meta_file)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')).ok())
        .and_then(|m| m.get("uid").and_then(|v| v.as_str()).map(|s| s.to_string()));
    if expect_uid.is_none() {
        expect_uid = Some(slot.to_string());
    }
    let expect_uid = expect_uid.unwrap_or_default();
    let has_snap_dir = snap_file.parent().map(|p| p.exists()).unwrap_or(false);
    // F2-4 端隔离：CodeBuddy 恢复不再回写共享 auth 文件（该文件属 WorkBuddy 登录
    // 驱动源），信号②对其恒无意义且会误报「疑似被其他进程回写」——仅 WorkBuddy 检查
    let check_auth = sess.prof.app_name != "CodeBuddy";

    // F1-2 因果信号锚点：记录"恢复落盘后"的 auth 文件 mtime
    let restore_mtime = std::fs::metadata(&auth_file).and_then(|m| m.modified()).ok();
    // 信号③锚点（仅 CodeBuddy）：恢复落盘后的 vscdb mtime。仅在快照确实包含 vscdb
    // 登录真源（本次恢复覆盖了它）时启用——旧版快照（无 vscdb 槽）只恢复了 auth 文件，
    // 登录真源未变，客户端启动正常写 vscdb 也会翻动 mtime，贸然启用会假阳性确认。
    let slot_has_vscdb = sess.prof.app_name == "CodeBuddy"
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
        // 信号②：共享 auth 文件 uid（仅 WorkBuddy；连续两次命中才确认）
        let auth_uid = if check_auth { auth_file_uid(&auth_file) } else { None };
        let mut auth_rewritten = true;
        if !has_snap_dir {
            auth_rewritten = std::fs::metadata(&auth_file)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| Some(t) != restore_mtime)
                .unwrap_or(false);
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
            if check_auth && auth_uid.is_some() && !revert_warned {
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
                    let uid = [
                        j.get("uid"),
                        j.get("account").and_then(|a| a.get("uid")),
                        j.get("accountId"),
                    ]
                    .into_iter()
                    .flatten()
                    .find_map(|v| v.as_str().map(|s| s.to_string()));
                    if uid.as_deref() == Some(expect_uid.as_str()) {
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
    VerifyResult::Timeout
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
    if sess.prof.app_name != "CodeBuddy" {
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
        let uid = crate::fs_utils::dig(&raw, &["genie.userId"])
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
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

    /// Session 构造：auth 路径注入临时目录（不触碰真实 auth 文件），
    /// 目录含纳秒级后缀（Windows pid 可复用，防上次运行残留互踩），先清再建
    fn session(app: TargetApp, tag: &str) -> Session {
        let data = std::env::temp_dir().join(format!(
            "sw-auth-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&data);
        let mut args = RunArgs {
            action: Action::Switch,
            target_app: app,
            user_id: Some("100".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
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
        let base = std::env::temp_dir().join(format!(
            "sw-authuid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
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
}
