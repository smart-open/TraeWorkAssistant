//! icube 布局快照（Trae/Qoder，原 PS Backup-CurrentProfile/Restore-Profile 的
//! icube 分支 1262-1398 对译）：精准白名单备份/恢复 + .bak 两代回滚 + 恢复计数
//!（供 Switch 恢复后校验）+ 客户端日志身份探测 + 槽位 sidecar 元数据校验。

use std::path::Path;

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

/// TRAE 白名单（原 ICUBE_ITEMS；F-80 M3 档案化后由
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

/// 精准备份：仅复制登录态关键文件（参考旧版 PS 切换器）
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
    for item in sess.prof.icube_items {
        if copy::copy_snapshot_item(&src.join(item.rel()), &dest.join(item.rel())) {
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
    for item in sess.prof.icube_items {
        let src_item = src.join(item.rel());
        let dst_item = dest.join(item.rel());
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
    // 合并版实测（2026-10-14）：快照冻结保存时刻的窗口类型，恢复旧快照后客户端
    // 按快照打开 IDE 窗口（用户预期恒开 Agent）。Trae 档案开启修正（best-effort，
    // 失败不影响登录态恢复）。
    if sess.prof.force_agent_window {
        if let Err(e) = apply_agent_window_fixup(&dest) {
            sink.step(
                "restore",
                StepStatus::Warn,
                &format!("Agent 窗口修正失败（不影响登录态恢复）: {e}"),
            );
        }
    }
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

/// 读取快照 sidecar 的（保存时间, 最近一次切换验证失败时间）。
/// sidecar 缺失/损坏 → (None, None)，不阻断账号列表渲染。
pub(crate) fn read_snapshot_sidecar(profiles_dir: &Path, slot: &str) -> (Option<i64>, Option<i64>) {
    let Ok(text) = std::fs::read_to_string(profiles_dir.join(format!("{slot}.meta.json"))) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, None);
    };
    (
        v.get("savedAtMs").and_then(|x| x.as_i64()),
        v.get("verifyFailedAtMs").and_then(|x| x.as_i64()),
    )
}

/// 标记「切换到该账号时客户端未确认登录（超时/身份不符）」——账号列表红色徽标的数据源。
/// sidecar 不存在（无快照）时静默跳过；成功切换或重新保存登录态时由 clear/backup 清除。
pub(crate) fn mark_snapshot_verify_failed(profiles_dir: &Path, slot: &str) {
    let path = profiles_dir.join(format!("{slot}.meta.json"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    obj.insert(
        "verifyFailedAtMs".into(),
        serde_json::json!(chrono::Utc::now().timestamp_millis()),
    );
    let _ = std::fs::write(&path, v.to_string());
}

/// 清除切换验证失败标记（切换到该账号并确认登录，或重新保存快照时调用）。
pub(crate) fn clear_snapshot_verify_failed(profiles_dir: &Path, slot: &str) {
    let path = profiles_dir.join(format!("{slot}.meta.json"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    if obj.remove("verifyFailedAtMs").is_some() {
        let _ = std::fs::write(&path, v.to_string());
    }
}

/// 「客户端当前登录」探测三态（2026-10-14 合并版实测增强）：
/// - [`LiveUidProbe::Uid`]：最新会话日志有明文 uid——当前登录的直接证据；
/// - [`LiveUidProbe::SessionNoUid`]：最新会话的 dynamicConfig.log 存在但无 uid——
///   客户端本次运行期间**未登录**（登录客户端启动数秒内即产生带 uid 的 config
///   query，本机实测 093008 会话 boot+1s 出现；观测完整性由切换/保存流程的
///   「先优雅关闭再探测」保证）。旧 Option 语义把它与「无法判定」合并 fail-open，
///   曾把未登录现场回写进标记槽（实测 2803 槽被 2026-10-14 09:33:40 的未登录
///   现场覆盖），三态分派后由调用方 fail-closed；
/// - [`LiveUidProbe::NoEvidence`]：无 logs/ 会话目录或最新会话无日志文件（含 0 字节
///   空日志：强杀/缓冲未 flush 场景无法据此判定「未登录」）——无法判定。
///
/// 已知局限（2026-10-14 审查登记）：`uid=` 仅在登录/鉴权请求中出现，用户在会话中
/// **手动退出登录**不产生反向记录，日志最后 uid 仍是退出前账号，三态探测恒报
/// `Uid(旧账号)`——此场景下保存守卫的身份校验不可达。需彻底杜绝须改用「日志尾部
/// 无 uid 请求的持续时间」等时序启发，误拒代价更高，暂不实施；用户手册应提示
/// 「退出登录后请勿立即保存当前登录态」。
#[derive(Debug)]
pub(crate) enum LiveUidProbe {
    Uid(String),
    SessionNoUid,
    NoEvidence,
}

/// 最新会话目录（15 字符 `yyyymmddThhmmss`，字典序=时间序）
pub(crate) fn newest_session_dir(app_data_dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(app_data_dir.join("logs"))
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        // 只认目录：同名格式普通文件（误拷贝/压缩包）混入会被 .max() 选中压过真会话
        .filter(|p| p.is_dir())
        .filter(is_session_dir)
        .max()
}

/// 三态探测当前登录（见 [`LiveUidProbe`]）
pub(crate) fn probe_live_uid(app_data_dir: &Path) -> LiveUidProbe {
    let Some(session) = newest_session_dir(app_data_dir) else {
        return LiveUidProbe::NoEvidence;
    };
    let log = session.join("dynamicConfig.log");
    if !log.exists() {
        return LiveUidProbe::NoEvidence;
    }
    // 0 字节空日志 ≠「确凿未登录」（强杀时日志缓冲可能整体丢失，已登录会话同样
    // 产生空日志）→ 判 NoEvidence 维持 fail-open，避免「已登录却存不了」误拒
    if std::fs::metadata(&log).map(|m| m.len() == 0).unwrap_or(true) {
        return LiveUidProbe::NoEvidence;
    }
    match extract_last_log_uid(&log) {
        Some(uid) => LiveUidProbe::Uid(uid),
        None => LiveUidProbe::SessionNoUid,
    }
}

/// 从客户端日志探测当前登录 uid（icube 布局）：
/// `<appdata>\logs\<yyyymmddThhmmss>\dynamicConfig.log` 的请求 URL 带明文
/// `uid=<10~20 位数字>`。会话目录名固定 15 字符且字典序=时间序，只看最新会话
/// （当前客户端这次运行）；最新会话无 uid 记录则返回 None——绝不回捞旧会话，
/// 旧会话的 uid 可能是切换前的账号，误报比不报更危险（调用方 fail-open）。
pub fn detect_live_uid(app_data_dir: &Path) -> Option<String> {
    match probe_live_uid(app_data_dir) {
        LiveUidProbe::Uid(uid) => Some(uid),
        _ => None,
    }
}

/// 会话目录名 = 15 字符 `yyyymmddThhmmss`（byte 8 == 'T'，其余全为数字）。
fn is_session_dir(p: &std::path::PathBuf) -> bool {
    match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => {
            name.len() == 15
                && name.as_bytes()[8] == b'T'
                && name
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| if i == 8 { b == b'T' } else { b.is_ascii_digit() })
        }
        None => false,
    }
}

/// 切换后登录身份确认结果（2026-10-14 合并版实测增强）
pub(crate) enum SwitchVerify {
    /// 客户端实际登录 = 切换目标
    Ok,
    /// 客户端实际登录为其他账号（快照污染/服务端会话回退，实测 2328 槽恢复后
    /// 静默登录为 1448）
    Mismatch(String),
    /// deadline 内未产生带 uid 的新会话（未登录或未联网）
    Timeout,
}

/// 轮询最新会话日志确认「切换后客户端实际登录身份」：合并版客户端启动即创建
/// 新会话目录与 dynamicConfig.log，其 uid 记录是「实际登录是谁」的直接证据。
/// 只认比 `baseline`（start_app 前的最新会话）更新的会话，防止把切换前会话的
/// 旧 uid 误读为切换后身份。
pub(crate) fn confirm_switch_uid(
    app_data_dir: &Path,
    uid: &str,
    baseline: Option<std::path::PathBuf>,
    timeout: std::time::Duration,
) -> SwitchVerify {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let Some(newest) = newest_session_dir(app_data_dir) else {
            continue;
        };
        if baseline.as_ref().is_some_and(|b| &newest <= b) {
            continue; // 客户端尚未完成启动产生新会话
        }
        if let Some(actual) = extract_last_log_uid(&newest.join("dynamicConfig.log")) {
            return if actual == uid {
                SwitchVerify::Ok
            } else {
                SwitchVerify::Mismatch(actual)
            };
        }
        // 新会话暂无 uid 记录：继续等（NEED_REFRESH/ExchangeToken 流程未完成）
    }
    SwitchVerify::Timeout
}

/// 「Agent 窗口」启动修正（2026-10-14 合并版实测）：合并版客户端启动窗口类型取
/// `User\globalStorage\storage.json` 的 `runtimeWindow.lastTask.windowType`
/// （枚举 Agent/IDE/Floating/TraeWork，缺省回落 Agent——resources/app/out/main.js
/// `_8e`）。快照冻结的是保存时刻的窗口类型，恢复旧快照会让客户端按快照打开
/// IDE 窗口；写回 Agent 使所有路径（切换恢复/顶部打开）恒开 Agent 窗口。
/// storage.json 缺失时 Ok（客户端首启缺省即 Agent，无需修正）。
pub(crate) fn apply_agent_window_fixup(data_dir: &Path) -> Result<(), String> {
    let path = data_dir.join("User").join("globalStorage").join("storage.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("storage.json 解析失败: {e}"))?;
    let Some(obj) = v.as_object_mut() else {
        return Err("storage.json 非对象".into());
    };
    obj.insert(
        "runtimeWindow.lastTask".into(),
        serde_json::json!({
            "windowType": "Agent",
            "startedAt": chrono::Utc::now().timestamp_millis()
        }),
    );
    let out = serde_json::to_string(&v).map_err(|e| format!("storage.json 序列化失败: {e}"))?;
    // 原子替换（2026-10-14 审查修复）：storage.json 是白名单登录态关键文件且 fixup
    // 不产生 .bak 代际，原地覆写遇崩溃/断电/磁盘满会截断毁掉登录态。同盘临时文件 +
    // rename 原子替换；serde_json 重序列化仅键序/空白与原文不同，客户端解析语义等价
    let tmp = path.with_extension("json.aiwork-tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("storage.json 临时写入失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("storage.json 原子替换失败: {e}"))?;
    Ok(())
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

    #[test]
    fn sidecar_快照验证失败标记_读写与清除() {
        let dir = std::env::temp_dir().join(format!(
            "sw-sidecar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let slot = "1234567890123456";
        assert_eq!(read_snapshot_sidecar(&dir, slot), (None, None));
        std::fs::write(
            dir.join(format!("{slot}.meta.json")),
            r#"{"slot":"1234567890123456","savedAtMs":123,"detectedUid":"1234567890123456"}"#,
        )
        .unwrap();
        assert_eq!(read_snapshot_sidecar(&dir, slot), (Some(123), None));
        mark_snapshot_verify_failed(&dir, slot);
        assert!(read_snapshot_sidecar(&dir, slot).1.is_some());
        // 清除后保存时间保持不变
        clear_snapshot_verify_failed(&dir, slot);
        assert_eq!(read_snapshot_sidecar(&dir, slot), (Some(123), None));
        let _ = std::fs::remove_dir_all(&dir);
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
            target_app: TargetApp::Trae,
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
    fn probe_live_uid_三态分派() {
        let appdata = std::env::temp_dir().join(format!(
            "sw-icube-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&appdata);
        // NoEvidence：无 logs 目录
        assert!(matches!(
            probe_live_uid(&appdata),
            LiveUidProbe::NoEvidence
        ));
        // NoEvidence：最新会话无 dynamicConfig.log
        let logs = appdata.join("logs");
        std::fs::create_dir_all(logs.join("20261014T090000")).unwrap();
        assert!(matches!(
            probe_live_uid(&appdata),
            LiveUidProbe::NoEvidence
        ));
        // Uid：会话日志带 uid
        std::fs::write(
            logs.join("20261014T090000").join("dynamicConfig.log"),
            "url https://x?uid=2328112497170937&ok=1",
        )
        .unwrap();
        match probe_live_uid(&appdata) {
            LiveUidProbe::Uid(u) => assert_eq!(u, "2328112497170937"),
            other => panic!("期望 Uid，实际 {other:?}"),
        }
        // SessionNoUid：会话与日志存在但无 uid（客户端未登录运行的直接证据）
        std::fs::write(
            logs.join("20261014T090000").join("dynamicConfig.log"),
            "request url: https://x/config/query?mid=abc&did=1",
        )
        .unwrap();
        assert!(matches!(
            probe_live_uid(&appdata),
            LiveUidProbe::SessionNoUid
        ));
        let _ = std::fs::remove_dir_all(&appdata);
    }

    #[test]
    fn agent窗口修正覆盖last_task且容忍缺失() {
        let dir = std::env::temp_dir().join(format!(
            "sw-icube-agentfix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let sg = dir.join("User").join("globalStorage").join("storage.json");
        // storage.json 缺失：Ok（客户端首启缺省即 Agent）
        apply_agent_window_fixup(&dir).unwrap();
        // 快照冻结 IDE 窗口 → 补丁为 Agent；其余键保留
        std::fs::create_dir_all(sg.parent().unwrap()).unwrap();
        std::fs::write(
            &sg,
            r#"{"runtimeWindow.lastTask":{"windowType":"IDE","startedAt":1},"theme":"vs-dark"}"#,
        )
        .unwrap();
        apply_agent_window_fixup(&dir).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&sg).unwrap())
            .unwrap();
        assert_eq!(v["runtimeWindow.lastTask"]["windowType"], "Agent");
        assert!(v["runtimeWindow.lastTask"]["startedAt"].as_i64().unwrap() > 0);
        assert_eq!(v["theme"], "vs-dark");
        // 非 JSON：Err（调用方 Warn 不阻断）
        std::fs::write(&sg, "not-json").unwrap();
        assert!(apply_agent_window_fixup(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_switch_uid零超时立即返回() {
        let dir = std::env::temp_dir().join("sw-icube-confirm-zero");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(
            confirm_switch_uid(
                &dir,
                "2328112497170937",
                None,
                std::time::Duration::ZERO
            ),
            SwitchVerify::Timeout
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
