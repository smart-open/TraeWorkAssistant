//! icube 布局快照（TraeWork/Trae，原 PS Backup-CurrentProfile/Restore-Profile 的
//! icube 分支 1262-1398 对译）：精准白名单备份/恢复 + .bak 两代回滚 + 恢复计数
//!（供 Switch 恢复后校验）+ 客户端日志身份探测 + 槽位 sidecar 元数据校验。

use std::path::Path;

use std::path::PathBuf;

use super::copy;
use super::{ProgressSink, Session, StepStatus};

/// 精准白名单（与 Backup/Restore 对称；表驱动替代 PS 的 14 段重复代码）。
/// 语义统一说明：icube 布局 PS 备份 `Local Storage\leveldb` 用「合并拷贝」、恢复用
/// 「清空后拷贝」；因备份前 .bak 轮转保证目标恒为新目录，合并 ≡ 替换，恢复侧
/// 「清空内容后拷」与「删目录重建后拷」终态恒等——Rust 统一为 copy_snapshot_item
/// 替换语义，结果集与 PS 完全一致。
pub(crate) enum Item {
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

/// TRAE 双应用（TraeWork/Trae）白名单（原 ICUBE_ITEMS；F-80 M3 档案化后由
/// AppProfile.icube_items 参数指向，backup/restore 不再依赖单一常量）
pub(crate) const TRAE_ICUBE_ITEMS: &[Item] = &[
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

/// 快照项全集（相对路径）：per-profile 白名单（F-80 M3 档案化：TRAE_ICUBE_ITEMS /
/// QODER_IDE_ITEMS 由 AppProfile.icube_items 指向）+ 平台专属项（mac 根级 Cookies 等）。
/// mac 项对所有 icube 布局档案（含 Qoder IDE，同为 VSCode fork）统一追加：
/// 备份/恢复同源对称（存在性检查自然跳过未命中项），同机快照无跨平台污染。
fn all_items(prof: &super::AppProfile) -> Vec<&'static str> {
    prof.icube_items
        .iter()
        .map(|i| i.rel())
        .chain(ICUBE_ITEMS_MAC.iter().copied())
        .collect()
}

/// Qoder CN IDE 白名单（F-80 M3 实测 2026-09-27：%APPDATA%\QoderCN）：与 TRAE 同构
/// 的 VSCode fork——根级 machineid/Local State/Preferences + User\globalStorage 登录
/// 真源（storage.json/state.vscdb±边车）。差异点：无 aha、无 Partitions、无
/// Local Storage\config.db；特有 SharedClientCache\cache 身份/凭证四小文件——
/// 仅纳入身份相关小文件，app-config/db/atlas/repowiki 等可再生缓存不入快照
pub(crate) const QODER_IDE_ITEMS: &[Item] = &[
    Item::File("User\\globalStorage\\storage.json"), // 设备标识/遥测/认证信息
    Item::File("User\\globalStorage\\state.vscdb"),  // 登录令牌数据库
    // WAL/SHM 边车随主库快照（同 TRAE：强杀后残留会被启动回放，恢复前先删）
    Item::File("User\\globalStorage\\state.vscdb-wal"),
    Item::File("User\\globalStorage\\state.vscdb-shm"),
    Item::File("User\\globalStorage\\state.vscdb.backup"),
    Item::File("machineid"),
    Item::File("Preferences"),
    Item::File("Local State"),
    Item::Dir("Local Storage\\leveldb"), // web 侧登录/偏好 KV
    Item::Dir("Network"),                // Cookie
    Item::Dir("Session Storage"),
    Item::File("SharedClientCache\\cache\\id"), // 客户端身份 id
    Item::File("SharedClientCache\\cache\\machine_token.json"), // 设备令牌
    Item::File("SharedClientCache\\cache\\client.json"),        // 客户端注册信息
    Item::File("SharedClientCache\\cache\\status.json"),        // 登录/激活状态
];

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
    for rel in all_items(&sess.prof) {
        if copy::copy_snapshot_item(&src.join(rel_path(rel)), &dest.join(rel_path(rel))) {
            copied += 1;
        }
    }
    // 槽位 sidecar 元数据（位于 profiles 根、不进快照目录）：记录保存时刻与保存
    // 时探测到的客户端身份。restore 前据此校验可发现历史污染事故（2026-09-29
    // 实测：1335 的登录态被存进 4487 槽）。写失败不阻断备份。
    let _ = std::fs::write(
        sess.prof.profiles_dir.join(format!("{slot}.meta.json")),
        serde_json::json!({
            "slot": slot,
            "savedAtMs": chrono::Utc::now().timestamp_millis(),
            "detectedUid": sess.detected_live_uid,
        })
        .to_string(),
    );
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
    warn_if_slot_identity_mismatch(sess, &src, slot, sink);
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
    for rel in all_items(&sess.prof) {
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

/// restore 前的 sidecar 身份校验：快照里存的登录态若来自别的账号（历史污染），
/// 提前 Warn 提醒。仅主槽校验（.bak 是覆盖前的旧代，sidecar 代际不对应，无法
/// 比对）；mismatch 只 Warn 不阻断——拦死会封死「切回该账号重新登录自救」的路；
/// 旧快照无 sidecar 时静默放行。
fn warn_if_slot_identity_mismatch(
    sess: &Session,
    src: &Path,
    slot: &str,
    sink: &dyn ProgressSink,
) {
    if src.file_name() != Some(std::ffi::OsStr::new(slot)) {
        return; // 回退自 .bak：不做代际比对
    }
    let Ok(text) =
        std::fs::read_to_string(sess.prof.profiles_dir.join(format!("{slot}.meta.json")))
    else {
        return; // 旧快照无 sidecar：静默放行
    };
    let Some(saved) = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("detectedUid").and_then(|x| x.as_str()).map(String::from))
    else {
        return;
    };
    if !saved.is_empty() && saved != slot {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!(
                "快照 {slot} 保存时检测到的客户端身份是账号 {saved}，与槽位账号不符，\
                 该快照可能已被其他账号的数据污染；建议恢复后重新登录并再次「保存当前登录态」覆盖修复"
            ),
        );
    }
}

/// 从客户端日志探测当前登录 uid（icube 布局）：
/// `<appdata>\logs\<yyyymmddThhmmss>\dynamicConfig.log` 的请求 URL 带明文
/// `uid=<10~20 位数字>`。会话目录名固定 15 字符且字典序=时间序，只看最新会话
/// （当前客户端这次运行）；最新会话无 uid 记录则返回 None——绝不回捞旧会话，
/// 旧会话的 uid 可能是切换前的账号，误报比不报更危险（调用方 fail-open）。
/// 注意：mac 数据根（~/Library/Application Support/Trae CN）2026-10-09 真机
/// 实测同构：`logs/yyyymmddThhmmss/` 会话目录与 `dynamicConfig.log` 均存在，
/// 且含 `uid=<16 位数字>` 请求 URL，本函数在 mac 同样生效（原仅 Windows 实测）。
pub fn detect_live_uid(app_data_dir: &Path) -> Option<String> {
    let logs_dir = app_data_dir.join("logs");
    let newest = std::fs::read_dir(&logs_dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(is_session_dir)
        .max();
    extract_last_log_uid(&newest?.join("dynamicConfig.log"))
}

/// 会话目录名 = 15 字符 `yyyymmddThhmmss`（byte 8 == 'T'，其余全为数字）。
fn is_session_dir(p: &std::path::PathBuf) -> bool {
    match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => {
            name.len() == 15
                && name.as_bytes()[8] == b'T'
                && name.bytes().all(|b| b.is_ascii_digit() || b == b'T')
        }
        None => false,
    }
}

/// 单个日志文件里取最后一次出现的 `uid=<10~20 位数字>`（只读尾部 2MB，日志
/// 可能很大）。边界检查：匹配处前一字符须非 ASCII 字母数字，防 `guid=` /
/// `ouid=` 误匹配；非 UTF-8 字节按 lossy 处理不影响 ASCII 数字。
fn extract_last_log_uid(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let meta = std::fs::metadata(path).ok()?;
    let take = meta.len().min(2 * 1024 * 1024);
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::End(-(take as i64))).ok()?;
    let mut bytes = Vec::with_capacity(take as usize);
    f.read_to_end(&mut bytes).ok()?;
    let buf = String::from_utf8_lossy(&bytes);
    let mut found = None;
    for (i, _) in buf.match_indices("uid=") {
        let prev_ok = match buf[..i].chars().next_back() {
            Some(c) => !c.is_ascii_alphanumeric(),
            None => true,
        };
        let digits: String = buf[i + 4..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if prev_ok && (10..=20).contains(&digits.len()) {
            found = Some(digits);
        }
    }
    found
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
            machine_id_override: None,
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
        // sidecar 写入 profiles 根（不进快照目录）；测试 appdata 无客户端日志
        // → detectedUid 为 null（探测失败 fail-open，不影响备份）
        let sidecar = sess.prof.profiles_dir.join("2117003799429594.meta.json");
        let sidecar_text = std::fs::read_to_string(&sidecar).unwrap();
        assert!(sidecar_text.contains(r#""slot":"2117003799429594""#));
        assert!(sidecar_text.contains(r#""detectedUid":null"#));

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

    #[test]
    fn detect_live_uid_最新会话与guid_ouid边界检查() {
        let appdata = std::env::temp_dir().join(format!(
            "sw-icube-detect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&appdata);
        let logs = appdata.join("logs");
        // 次新会话：另一账号的 uid（不应被采纳）
        let older = logs.join("20260928T101010");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::write(
            older.join("dynamicConfig.log"),
            "url https://x?uid=1335050000000000&tail=1",
        )
        .unwrap();
        // 最新会话：同行 guid=/ouid= 干扰 + 正确 uid（验证边界检查）
        let newest = logs.join("20260929T121212");
        std::fs::create_dir_all(&newest).unwrap();
        std::fs::write(
            newest.join("dynamicConfig.log"),
            "req guid=abc123&ouid=9988776655 url=https://x?uid=4487568582777872&ok=1",
        )
        .unwrap();
        // 非 15 字符/乱格式的目录一律忽略
        std::fs::create_dir_all(logs.join("not-a-session")).unwrap();
        std::fs::create_dir_all(logs.join("20260930T99999999999999")).unwrap();
        assert_eq!(
            detect_live_uid(&appdata).as_deref(),
            Some("4487568582777872")
        );

        // 最新会话无 uid 记录 → 不回捞次新（旧 uid 可能是切换前账号，宁缺勿错）
        std::fs::remove_file(newest.join("dynamicConfig.log")).unwrap();
        assert_eq!(detect_live_uid(&appdata), None);
        let _ = std::fs::remove_dir_all(&appdata);
    }

    #[test]
    fn sidecar_mismatch_restore仅warn不阻断() {
        let _guard = crate::switcher::test_io_lock();
        let sess = session("sidecar");
        let sink = QuietSink;
        let dd = sess.prof.data_dir.clone();
        std::fs::create_dir_all(dd.join("User").join("globalStorage")).unwrap();
        std::fs::write(dd.join("User").join("globalStorage").join("storage.json"), "{}").unwrap();

        // 模拟守卫探测到身份一致后备份：sidecar 记录该身份
        let mut with_uid = sess;
        with_uid.detected_live_uid = Some("2117003799429594".into());
        backup_icube(&with_uid, "2117003799429594", &sink).unwrap();
        let sidecar = with_uid.prof.profiles_dir.join("2117003799429594.meta.json");
        assert!(std::fs::read_to_string(&sidecar)
            .unwrap()
            .contains(r#""detectedUid":"2117003799429594""#));

        // 篡改 sidecar 模拟历史污染（保存进了别的账号身份）：restore 仍 Ok，仅 Warn
        std::fs::write(
            &sidecar,
            r#"{"slot":"2117003799429594","savedAtMs":1,"detectedUid":"1335050000000000"}"#,
        )
        .unwrap();
        restore_icube(&mut with_uid, "2117003799429594", &sink).unwrap();
        assert_eq!(with_uid.last_restored_count, 1);
        let _ = std::fs::remove_dir_all(&with_uid.data_dir);
    }
}
