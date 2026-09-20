//! icube 布局快照（TraeWork/Trae，原 PS Backup-CurrentProfile/Restore-Profile 的
//! icube 分支 1262-1398 对译）：精准白名单备份/恢复 + .bak 单代回滚 + 恢复计数
//!（供 Switch 恢复后校验）。

use std::path::PathBuf;

use super::copy;
use super::{ProgressSink, Session, StepStatus};

/// 精准白名单（与 Backup/Restore 对称；表驱动替代 PS 的 14 段重复代码）。
/// 语义统一说明：icube 布局 PS 备份 `Local Storage\leveldb` 用「合并拷贝」、恢复用
/// 「清空后拷贝」；因备份前 .bak 轮转保证目标恒为新目录，合并 ≡ 替换，恢复侧
/// 「清空内容后拷」与「删目录重建后拷」终态恒等——Rust 统一为 copy_snapshot_item
/// 替换语义，结果集与 PS 完全一致。
enum Item {
    File(&'static str),
    Dir(&'static str),
}

impl Item {
    fn rel(&self) -> &'static str {
        match self {
            Item::File(r) | Item::Dir(r) => r,
        }
    }
}

const ICUBE_ITEMS: &[Item] = &[
    Item::File("User\\globalStorage\\storage.json"), // 1. 设备标识/遥测/认证信息
    Item::File("User\\globalStorage\\state.vscdb"), // 2. 登录令牌数据库
    // 2b. WAL/SHM 边车随主库快照（审查修复 2026-09-15）：优雅关闭超时强杀是常态
    //（switcher.log 实测 TRAE 每次切换都强杀），最新登录写入可能尚未 checkpoint 进
    // 主库——漏拷丢数据，且恢复侧依赖边车与主库成对回放（见 restore_icube）
    Item::File("User\\globalStorage\\state.vscdb-wal"),
    Item::File("User\\globalStorage\\state.vscdb-shm"),
    Item::File("User\\globalStorage\\state.vscdb.backup"),
    Item::File("machineid"),                        // 3. 机器标识
    Item::Dir("aha"),                               // 4. 设备认证数据
    Item::File("Preferences"),                      // 5.
    Item::File("Local State"),
    Item::Dir("Local Storage\\leveldb"),            // 6. web 侧登录/偏好 KV
    Item::File("Local Storage\\config.db"),
    Item::Dir("Network"),                           // 7. Cookie
    Item::Dir("Partitions\\trae-webview"),          // 8.
    Item::Dir("Partitions\\icube-web-crawler-shared-session-v1.0"),
    Item::Dir("Session Storage"),                   // 9.
];

/// mac 专属快照项（F-75 M-1 侦察 ①，2026-09-20 真机实测）：mac Trae CN / TRAE SOLO CN
/// 数据目录无 `Network/` 子目录，Chromium 根级文件放在数据根而非 `Network/` 下：
/// - `Cookies` + `Cookies-journal`（登录 Cookies，Windows 在 Network/ 下）
/// - `Network Persistent State`（Chromium 网络持久状态，Windows 在 Network/ 下）
///（Windows 的 `Dir("Network")` 项在 mac 由存在性检查自然跳过，无需删除）。
#[cfg(target_os = "macos")]
const ICUBE_ITEMS_MAC: &[&str] = &["Cookies", "Cookies-journal", "Network Persistent State"];

#[cfg(not(target_os = "macos"))]
const ICUBE_ITEMS_MAC: &[&str] = &[];

/// 快照相对路径组件化（F-75 M-1 侦察修复）：ICUBE_ITEMS 沿用 Windows 反斜杠常量
/// 形态，mac 上 `PathBuf::join("User\\globalStorage\\storage.json")` 会把整串当作
/// 单个文件名组件导致恒 miss——统一按 '\\' 切分组件逐级 join（Windows join 结果
/// 与原样一致，行为零变化；与 trae_apps.rs v2.7 组件化修复同思路）。
/// mod.rs 的恢复后校验（verify_restore）同样复用本函数。
pub(super) fn rel_path(rel: &str) -> PathBuf {
    rel.split('\\').fold(PathBuf::new(), |p, c| p.join(c))
}

/// 快照项全集（相对路径）：公共项 + 平台专属项
fn all_items() -> Vec<&'static str> {
    ICUBE_ITEMS.iter().map(|i| i.rel()).chain(ICUBE_ITEMS_MAC.iter().copied()).collect()
}

/// 精准备份：仅复制登录态关键文件（参考 traework-switcher）
pub fn backup_icube(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let src = &sess.prof.data_dir;
    if !src.exists() {
        sink.step("backup", StepStatus::Skip, "当前数据目录不存在，跳过备份");
        return Ok(());
    }
    let dest = sess.prof.profiles_dir.join(slot);
    // 审查修复：与 chromium/authfile 布局对齐——覆盖槽位前把旧快照挪到 .bak
    //（单代回滚保护），防止拷贝中断（断电/杀进程）永久丢失上一份快照
    copy::rotate_bak(&dest, slot, sink);
    std::fs::create_dir_all(&dest).map_err(|e| format!("创建快照目录失败: {e}"))?;

    let mut copied = 0usize;
    for rel in all_items() {
        if copy::copy_snapshot_item(&src.join(rel_path(rel)), &dest.join(rel_path(rel))) {
            copied += 1;
        }
    }
    sink.step(
        "backup",
        StepStatus::Ok,
        &format!("已备份当前登录态到 {slot} ({copied} 项)"),
    );
    Ok(())
}

/// 精准恢复：仅恢复登录态关键文件（与备份对称）。恢复项计数写入
/// Session.last_restored_count（Switch 恢复后校验用）。
pub fn restore_icube(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let (src, slot_label) = copy::resolve_slot(sess, slot, sink)?;
    let dest = sess.prof.data_dir.clone();
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

    // 删除 code.lock 防止启动冲突
    let _ = std::fs::remove_file(dest.join("code.lock"));

    // 审查修复（实测 2026-09-15，Trae「切换后账号不变/本机识别错乱」根因）：
    // 强杀后现场残留 state.vscdb-wal/-shm，SQLite WAL 模式下客户端启动打开恢复后的
    // 主库会把旧 WAL 回放，把切换前账号的登录/使用证据写回新库（authfile 布局同款
    // 问题已在 restore_authfile 处理）。恢复前必须先删边车。
    let gs_dir = dest.join("User").join("globalStorage");
    for stale in ["state.vscdb-wal", "state.vscdb-shm"] {
        let _ = std::fs::remove_file(gs_dir.join(stale));
    }

    // 对称恢复：槽位有的项覆盖，槽位没有的项删除现场残留——恢复后 Live 恒等于槽位
    // 内容，不携带上一账号的残留（如槽位缺 state.vscdb.backup 而现场有旧账号的）
    let mut restored = 0usize;
    for rel in all_items() {
        let src_item = src.join(rel_path(rel));
        let dst_item = dest.join(rel_path(rel));
        if src_item.exists() {
            if copy::copy_snapshot_item(&src_item, &dst_item) {
                restored += 1;
            }
        } else if dst_item.is_dir() {
            let _ = std::fs::remove_dir_all(&dst_item);
        } else if dst_item.exists() {
            let _ = std::fs::remove_file(&dst_item);
        }
    }
    sess.last_restored_count = restored as i64;
    sink.step(
        "restore",
        StepStatus::Ok,
        &format!("已恢复账号 {slot_label} 的登录态 ({restored} 项)"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switcher::{Action, RunArgs, TargetApp};

    struct QuietSink;
    impl ProgressSink for QuietSink {
        fn step(&self, _: &str, _: StepStatus, _: &str) {}
    }

    fn session(tag: &str) -> Session {
        let data = std::env::temp_dir().join(format!(
            "sw-icube-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&data);
        let mut args = RunArgs {
            action: Action::Switch,
            target_app: TargetApp::TraeWork,
            user_id: Some("2117003799429594".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            data_dir: data.clone(),
        };
        args.include_indexeddb = false;
        std::fs::create_dir_all(data.join("conf")).unwrap();
        let mut sess = Session::new(&args);
        // 关键隔离：profile_for 的应用数据目录来自环境变量（真实 %APPDATA%），
        // 测试必须覆盖到临时目录，绝不触碰真实应用数据
        sess.prof.data_dir = data.join("appdata");
        sess
    }

    #[test]
    fn backup_restore_round_trip_白名单13项() {
        let _guard = crate::switcher::test_io_lock();
        let mut sess = session("roundtrip");
        let sink = QuietSink;
        // 造 Live 数据：storage.json / state.vscdb / machineid / aha dir / Network dir
        let dd = sess.prof.data_dir.clone();
        std::fs::create_dir_all(dd.join("User").join("globalStorage")).unwrap();
        std::fs::write(dd.join("User").join("globalStorage").join("storage.json"), "{}").unwrap();
        std::fs::write(dd.join("User").join("globalStorage").join("state.vscdb"), "db").unwrap();
        std::fs::write(dd.join("machineid"), "M").unwrap();
        std::fs::create_dir_all(dd.join("aha")).unwrap();
        std::fs::write(dd.join("aha").join("t"), "a").unwrap();
        std::fs::create_dir_all(dd.join("Network")).unwrap();
        std::fs::write(dd.join("Network").join("Cookies"), "c").unwrap();

        backup_icube(&sess, "2117003799429594", &sink).unwrap();
        let slot = sess.prof.profiles_dir.join("2117003799429594");
        assert!(slot.join("User").join("globalStorage").join("storage.json").exists());
        assert!(slot.join("machineid").exists());
        assert!(slot.join("aha").join("t").exists());
        assert!(slot.join("Network").join("Cookies").exists());
        assert!(!slot.join("Preferences").exists(), "白名单外文件不入快照");

        // 破坏 Live 后恢复：快照内共 5 项（storage.json / state.vscdb / machineid /
        // aha / Network），全量恢复计数 = 5
        std::fs::remove_file(dd.join("machineid")).unwrap();
        std::fs::remove_dir_all(dd.join("aha")).unwrap();
        restore_icube(&mut sess, "2117003799429594", &sink).unwrap();
        assert_eq!(sess.last_restored_count, 5);
        assert!(dd.join("machineid").exists());
        assert!(dd.join("aha").join("t").exists());

        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }

    #[test]
    fn 恢复后校验missing判定() {
        let _guard = crate::switcher::test_io_lock();
        // 0 项恢复 / 缺 storage.json / 缺 vscdb 三种 missing 来源
        let mut sess = session("missing");
        // 0 项恢复（空快照目录）
        let slot = sess.prof.profiles_dir.join("2117003799429594");
        std::fs::create_dir_all(&slot).unwrap();
        let sink = QuietSink;
        restore_icube(&mut sess, "2117003799429594", &sink).unwrap();
        assert_eq!(sess.last_restored_count, 0);
        let _ = std::fs::remove_dir_all(&sess.data_dir);
    }
}
