//! Qoder IDE 存储账号发现/导入（F-80 M3；R-8 侦察结论固化为 L1 凭证通道）。
//!
//! R-8 实测结论（2026-09-27 本机侦察，详见设计文档 R-8）：
//! - 登录态真源：`%APPDATA%\QoderCN\User\globalStorage\state.vscdb`
//!   ItemTable 键 `secret://aicoding.auth.userInfo`——TEXT 列存 JSON
//!   `{"type":"Buffer","data":[...]}`，前 3 字节 ASCII "v10"（Chromium os_crypt 形态）
//! - 解密链路：`%APPDATA%\QoderCN\Local State`（dataDir 根目录，非 User\globalStorage 下）
//!   → `os_crypt.encrypted_key`（base64 + DPAPI 包裹）→ AES-256-GCM 密钥 →
//!   'v10' + nonce(12) + ct + tag(16)
//! - 解密后 userInfo JSON：id（36 位 uid）/ token（dt- 前缀）/ refreshToken（drt- 前缀）/
//!   expireTime·refreshTokenExpireTime（13 位毫秒字符串）/ name / login_source="qodercn"
//! - CN 版无国际版 auth.v1.dat 形态（R-8 裁决：以 state.vscdb 通道为准）
//!
//! 快照完整性：switcher QODER_IDE_ITEMS 白名单含 Local State 与 state.vscdb（+WAL/SHM），
//! 快照恢复后密钥与密文同槽走，IDE 可自解密——本模块只读不改。
//!
//! 【跨平台审查 2026-10-03】macOS 适配预留（本模块为 Qoder 域解密核心）：
//! - 跨平台可复用：`parse_buffer` / `decrypt_v10` / `read_vscdb_key` / `scan_ide_login`
//!   主流程与 state.vscdb 结构均为平台无关（Chromium os_crypt v10 密文同构）；
//! - 平台分支点：唯一差异在 AES 密钥获取（`ide_aes_key`）——Windows = Local State
//!   os_crypt.encrypted_key + DPAPI；macOS = Keychain「Chromium Safe Storage」service
//!   条目（service 名与 v10/v11 前缀需真机实测，预期走 security-cli 或 security-framework crate）；
//! - Work 凭据链路（scan_work_login_uid）同理：auth.v1.dat 与 Local State 同目录同链路，
//!   仅密文载体不同。检索标记：`macOS 适配预留`。
//!
//! 凭证红线：token 不进日志/事件/返回值；摘要只回 qd- id 与昵称。

// 消费点全部在 cfg(windows) 的扫描/解密链路——mac 构建未消费（macOS 适配预留）
#[cfg_attr(target_os = "macos", allow(unused_imports))]
use std::path::Path;

use serde::Serialize;
use tauri::State;

// 消费点集中在 cfg(windows) 的扫描/解密链路——mac 构建未消费（macOS 适配预留）
#[cfg_attr(target_os = "macos", allow(unused_imports))]
use super::common::{account_id_of, ide_data_dir, with_pool_mut, QoderAccount};
use crate::state::AppState;
#[cfg_attr(target_os = "macos", allow(unused_imports))]
use crate::tasks::qoder_common::{self, QoderCreds};
#[cfg_attr(target_os = "macos", allow(unused_imports))]
use crate::tasks::qoder_device::QoderDeviceProfile;

/// 登录态键（state.vscdb ItemTable）
#[cfg_attr(target_os = "macos", allow(dead_code))]
const KEY_USER_INFO: &str = "secret://aicoding.auth.userInfo";

// ── 解密链路（对齐 device_proxy/local_capture.rs Chrome AES 模板）──────────

/// Local State（dataDir 根目录）→ os_crypt.encrypted_key → base64 → DPAPI → AES 密钥
/// macOS 适配预留：Windows 专属密钥获取。macOS 等价实现 = 读 Keychain
/// 「Chromium Safe Storage」条目得到 16B 密钥（security-framework crate 或
/// `security find-generic-password` 子进程），后续 AES-256-GCM 解密逻辑
/// （decrypt_v10）无需改动；建议以同签名 `ide_aes_key(data_dir)` 按 cfg 并存，
/// scan_ide_login 调用链零改动
#[cfg(windows)]
fn ide_aes_key(data_dir: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let lp = data_dir.join("Local State");
    let raw = std::fs::read_to_string(&lp).map_err(|e| format!("Local State 读取失败: {e}"))?;
    let state: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Local State 解析失败: {e}"))?;
    let b64 = state
        .get("os_crypt")
        .and_then(|v| v.get("encrypted_key"))
        .and_then(serde_json::Value::as_str)
        .ok_or("Local State 无 os_crypt.encrypted_key")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("encrypted_key base64 解码失败: {e}"))?;
    if raw.len() < 5 || &raw[..5] != b"DPAPI" {
        return Err("encrypted_key 前缀非 DPAPI".into());
    }
    crate::vault::dpapi::unprotect(&raw[5..])
}

/// v10 密文解密：'v10' + nonce(12) + ct + tag(16) → AES-256-GCM
#[cfg(windows)]
fn decrypt_v10(enc: &[u8], key: &[u8]) -> Option<String> {
    if enc.len() < 19 || &enc[..3] != b"v10" {
        return None;
    }
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&enc[3..15]), &enc[15..])
        .ok()?;
    String::from_utf8(plain).ok()
}

/// secret:// 值形态：TEXT JSON `{"type":"Buffer","data":[...]}` → 字节
#[cfg(windows)]
fn parse_buffer(text: &str) -> Option<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("data")?.as_array().map(|a| {
        a.iter()
            .filter_map(|x| x.as_u64().map(|n| n as u8))
            .collect::<Vec<u8>>()
    })
}

/// 读 state.vscdb ItemTable 单键（TEXT 优先，BLOB 按 UTF-8 解码；文件缺失 → Ok(None)）。
/// I16：返回 Result 区分「无键」与「读库失败」——IDE 运行中可能短暂锁定库文件，
/// busy_timeout 1.5s 缓解，打开失败消息标注可能原因，避免一律误报「未登录」。
/// 自建只读连接（switcher::vscdb::read_key 为私有且语义面向全局键合并，不复用）
#[cfg(windows)]
fn read_vscdb_key(vscdb: &Path, key: &str) -> Result<Option<String>, String> {
    if !vscdb.is_file() {
        return Ok(None);
    }
    // P3 审查修复：IDE 异常退出会残留 -wal 而无 -shm，只读连接无法重建索引导致
    // 读取失败——检测该形态并在错误消息附修复指引，防一律误报「被 IDE 占用」
    let wal_orphan = vscdb.with_file_name("state.vscdb-wal").is_file()
        && !vscdb.with_file_name("state.vscdb-shm").is_file();
    let wal_hint = if wal_orphan {
        "；检测到异常退出残留的 WAL 索引，重启一次 IDE 可自动修复"
    } else {
        ""
    };
    let conn = rusqlite::Connection::open_with_flags(vscdb, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("state.vscdb 打开失败（可能被 IDE 占用{wal_hint}）: {e}"))?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))
        .map_err(|e| format!("state.vscdb busy_timeout 设置失败: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1")
        .map_err(|e| format!("state.vscdb 查询准备失败{wal_hint}: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params![key])
        .map_err(|e| format!("state.vscdb 查询执行失败{wal_hint}: {e}"))?;
    let row = match rows
        .next()
        .map_err(|e| format!("state.vscdb 读取行失败: {e}"))?
    {
        Some(r) => r,
        None => return Ok(None),
    };
    let val = row
        .get::<_, rusqlite::types::Value>(0)
        .map_err(|e| format!("state.vscdb 键值类型读取失败: {e}"))?;
    match val {
        rusqlite::types::Value::Text(s) => Ok(Some(s)),
        rusqlite::types::Value::Blob(b) => {
            String::from_utf8(b).map(Some).map_err(|e| format!("state.vscdb BLOB 非 UTF-8: {e}"))
        }
        _ => Ok(None),
    }
}

// ── 登录态读取 ─────────────────────────────────────────────────────────────

/// Work 客户端登录 uid（2026-10-04 实测修正：登录真源 = 数据目录根级 auth.v1.dat，
/// "v10" os_crypt 密文 JSON = token/refreshToken/expiresAt/user{id,name,...}，密钥
/// 与 IDE 同链路 = 根级 Local State os_crypt.encrypted_key + DPAPI。原 Cookies
/// qoderuid 探测已证伪——Work Cookies 库不存在该 cookie，守卫/徽标恒 None 失效）。
/// 文件缺失/解密失败/JSON 无 user.id → None（调用方 fail-open，不阻断流程）
#[cfg(windows)]
pub fn scan_work_login_uid(data_dir: &Path) -> Option<String> {
    let enc = std::fs::read(data_dir.join("auth.v1.dat")).ok()?;
    let key = ide_aes_key(data_dir).ok()?;
    let plain = decrypt_v10(&enc, &key)?;
    let v: serde_json::Value = serde_json::from_str(&plain).ok()?;
    v.get("user")?
        .get("id")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// IDE 存储当前登录账号（解密产物；token 仅供导入通路，不落日志/事件）
#[cfg(windows)]
pub struct IdeLogin {
    pub uid: String,
    pub token: String,
    pub refresh_token: String,
    pub name: String,
    /// expireTime（13 位毫秒 → i64 毫秒）
    pub expires_at_ms: Option<i64>,
}

/// expireTime 提取：13 位毫秒时间戳（字符串/数字形态兼容）。
/// P3 审查修复：秒/毫秒归一——>=1e12 视为毫秒原样返回，否则视为秒 ×1000
/// （workbuddy::common::as_ts_seconds 为 pub(super) 不跨模块可用，本地实现同款阈值），
/// 防秒级值被当作毫秒折算成 1970 年
#[cfg(windows)]
fn expire_ms_of(v: &serde_json::Value) -> Option<i64> {
    let raw = v.get("expireTime")?;
    let ts = raw.as_i64().or_else(|| raw.as_str()?.trim().parse::<i64>().ok())?;
    Some(if ts >= 1_000_000_000_000 { ts } else { ts * 1000 })
}

/// 读 IDE 存储当前登录态（未登录/解密失败 → Err，描述脱敏不含凭证）
#[cfg(windows)]
pub fn scan_ide_login(data_dir: &Path) -> Result<IdeLogin, String> {
    let key = ide_aes_key(data_dir)?;
    let vscdb = data_dir.join("User").join("globalStorage").join("state.vscdb");
    let raw = read_vscdb_key(&vscdb, KEY_USER_INFO)?
        .ok_or("state.vscdb 无登录态（可能未登录或已清除）")?;
    let bytes = parse_buffer(&raw).ok_or("登录态键值形态不识别")?;
    let plain = decrypt_v10(&bytes, &key).ok_or("登录态解密失败（密钥或密文不匹配）")?;
    let v: serde_json::Value =
        serde_json::from_str(&plain).map_err(|e| format!("userInfo 解析失败: {e}"))?;
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Ok(IdeLogin {
        uid: s("id"),
        token: s("token"),
        refresh_token: s("refreshToken"),
        name: s("name"),
        expires_at_ms: expire_ms_of(&v),
    })
}

// ── 扫描/导入命令 ──────────────────────────────────────────────────────────

/// 扫描结果摘要（脱敏：不含 token 本体）
#[derive(Serialize, Clone, Default)]
pub struct QoderIdeScanResult {
    pub found: bool,
    /// true = 新入池；false = 已在池中更新
    pub imported: bool,
    pub updated: bool,
    pub account_id: String,
    pub nickname: String,
    pub reason: String,
}

/// 扫描 IDE 存储当前登录账号并导入账号池（幂等：同 token 稳定同 id，重复=更新）。
/// 凭证入 token store（kind=client，dt-/drt- 原样入 store；effective_creds 请求侧
/// 仍按 §5.10 注入每账号绑定指纹——IDE 本机指纹不入 store，避免多账号共享单指纹）。
/// async：命令含 DPAPI 解密 + SQLite 读取等阻塞 IO，移出主线程避免卡 UI。
#[tauri::command(async)]
pub fn qoder_ide_scan(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<QoderIdeScanResult, String> {
    #[cfg(windows)]
    {
        let data_dir = ide_data_dir().ok_or("无法定位 QoderCN 数据目录（APPDATA 缺失）")?;
        let login = match scan_ide_login(&data_dir) {
            Ok(l) => l,
            Err(e) => {
                return Ok(QoderIdeScanResult {
                    found: false,
                    reason: e,
                    ..Default::default()
                })
            }
        };
        if login.token.is_empty() {
            return Ok(QoderIdeScanResult {
                found: false,
                reason: "登录态中无 token".into(),
                ..Default::default()
            });
        }
        let id = account_id_of(&login.token);
        // I09：持锁读-改-写，防并发整池覆盖丢更新。
        // P2 审查修复：入池匹配补 uid 兜底（对齐 oauth/data_io/PAT 导入的同款语义）——
        // 同账号先经其他通道入池（token 派生 id 不同）后再扫描 IDE 时，uid 命中保留
        // 原 id，防同 uid 重复账号；凭证按生效 id 落库，与池条目对齐
        let (updated, nickname, effective_id) = with_pool_mut(&state, |accounts| {
            if let Some(a) = accounts
                .iter_mut()
                .find(|a| a.id == id || (!login.uid.is_empty() && a.uid == login.uid))
            {
                // 已有账号保守回填：uid/nickname 只在为空时补，不覆盖用户手动改名
                if a.uid.is_empty() && !login.uid.is_empty() {
                    a.uid = login.uid.clone();
                }
                if a.nickname.is_empty() && !login.name.is_empty() {
                    a.nickname = login.name.clone();
                }
                // P2 审查修复：credential_source 保守更新——id 命中即同 token、凭证本体
                // 未变，仅来源字段为空时回填；uid 兜底命中且派生 id 不同 = 换了新凭证
                // （其他通道 → IDE 登录），徽标随之更新（对齐 oauth.rs 同款判据）
                if a.credential_source.is_empty() || a.id != id {
                    a.credential_source = "ide_store".into();
                }
                // 到期时间过域钳制（与 creds_of 同口径：超 (0, now+10y] 视为无过期信息，
                // 防服务端脏数据导致到期看板溢出/千年展示）。
                // P3 审查修复：仅在解析出有效到期时间时覆盖（对齐 data_io merge 的
                // is_some 才覆盖口径）——登录态无 expireTime/钳制失败时保留本地已知值
                if let Some(sec) = login
                    .expires_at_ms
                    .and_then(qoder_common::clamp_expires_at)
                    .map(|ms| ms / 1000)
                {
                    a.token_expires_at = Some(sec);
                }
                a.needs_relogin = false;
                a.relogin_reason = String::new();
                if a.device_profile.is_none() {
                    a.device_profile = Some(QoderDeviceProfile::generate());
                }
                Ok((true, a.nickname.clone(), a.id.clone()))
            } else {
                let nickname = if login.name.is_empty() {
                    format!("Qoder {}", &id[3..9])
                } else {
                    login.name.clone()
                };
                accounts.push(QoderAccount {
                    id: id.clone(),
                    uid: login.uid.clone(),
                    nickname: nickname.clone(),
                    credential_source: "ide_store".into(),
                    token_expires_at: login
                        .expires_at_ms
                        .and_then(qoder_common::clamp_expires_at)
                        .map(|ms| ms / 1000),
                    // 入池即生成稳定指纹（§5.10：一次生成永不轮换）
                    device_profile: Some(QoderDeviceProfile::generate()),
                    ..Default::default()
                });
                Ok((false, nickname, id.clone()))
            }
        })?;
        // 凭证入 token store（save_token_store 非空字段 merge，不抹掉存量字段）
        let creds = QoderCreds {
            access_token: login.token,
            refresh_token: login.refresh_token,
            expires_at_ms: login.expires_at_ms,
            uid: login.uid,
            nickname: login.name,
            kind: "client".into(),
            ..Default::default()
        };
        // 凭证按生效 id 落库（uid 兜底命中时为池内既有 id），与池条目对齐——
        // 若按派生 id 落库则凭证与池条目错位，后续扫描/导入按 id 查不到新凭证
        qoder_common::save_token_store(&state, &effective_id, &creds)?;
        crate::fs_utils::app_log(&state.data_dir, &format!("Qoder IDE 存储账号已发现并导入: {effective_id}"));
        // 新账号/凭证变更联动网关池热重载（fail-open 即时入池调度；服务未运行时 no-op）
        crate::commands::api_server::reload_pools_if_running(&state, runtime.inner());
        Ok(QoderIdeScanResult {
            found: true,
            imported: !updated,
            updated,
            account_id: effective_id,
            nickname,
            reason: String::new(),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        let _ = &runtime;
        // macOS 适配预留：macOS 分支实装后本分支删除——scan_ide_login 主流程
        // 跨平台复用，仅需补 Keychain 密钥获取（见模块头【跨平台审查】与 ide_aes_key 注释）
        Err("IDE 存储发现仅支持 Windows（DPAPI + AES-GCM）".into())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn parse_buffer_解析与非json容错() {
        assert_eq!(
            parse_buffer(r#"{"type":"Buffer","data":[118,49,48]}"#).unwrap(),
            b"v10".to_vec()
        );
        assert!(parse_buffer("not-json").is_none());
        assert!(parse_buffer(r#"{"type":"Buffer"}"#).is_none());
    }

    #[test]
    fn v10_加解密往返与错钥失败() {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        let key = [7u8; 32];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = Nonce::from_slice(&[9u8; 12]);
        let ct = cipher.encrypt(nonce, b"{\"id\":\"u1\"}" as &[u8]).unwrap();
        let mut enc = b"v10".to_vec();
        enc.extend_from_slice(nonce);
        enc.extend_from_slice(&ct);
        assert_eq!(decrypt_v10(&enc, &key).unwrap(), "{\"id\":\"u1\"}");
        assert!(decrypt_v10(&enc, &[8u8; 32]).is_none(), "错误密钥必须失败");
        assert!(decrypt_v10(b"v10", &key).is_none());
        assert!(decrypt_v10(b"DPx0\x01\x02", &key).is_none(), "非 v10 前缀拒绝");
    }

    #[test]
    fn vscdb读键_text与blob与缺文件() {
        let p = std::env::temp_dir().join(format!(
            "f80-ide-{}-{}.vscdb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")
            .unwrap();
        conn.execute(
            "INSERT INTO ItemTable VALUES(?1, ?2)",
            rusqlite::params![KEY_USER_INFO, r#"{"type":"Buffer","data":[1]}"#],
        )
        .unwrap();
        let blob_val: Vec<u8> = b"from-blob".to_vec();
        conn.execute(
            "INSERT INTO ItemTable VALUES(?1, ?2)",
            rusqlite::params!["secret://other.key", blob_val],
        )
        .unwrap();
        assert_eq!(
            read_vscdb_key(&p, KEY_USER_INFO).unwrap().as_deref(),
            Some(r#"{"type":"Buffer","data":[1]}"#)
        );
        assert_eq!(
            read_vscdb_key(&p, "secret://other.key").unwrap().as_deref(),
            Some("from-blob")
        );
        assert_eq!(read_vscdb_key(&p, "secret://missing.key").unwrap(), None);
        assert_eq!(
            read_vscdb_key(&std::env::temp_dir().join("f80-nope.vscdb"), KEY_USER_INFO).unwrap(),
            None
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn expire_time_字符串与数字形态兼容() {
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":"1791673619906"}"#).unwrap();
        assert_eq!(expire_ms_of(&v), Some(1_791_673_619_906));
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":1791673619906}"#).unwrap();
        assert_eq!(expire_ms_of(&v), Some(1_791_673_619_906));
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":"abc"}"#).unwrap();
        assert_eq!(expire_ms_of(&v), None);
        let v: serde_json::Value = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(expire_ms_of(&v), None);
        // P3：10 位秒级时间戳归一为毫秒（防被折算成 1970 年）
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":1791673619}"#).unwrap();
        assert_eq!(expire_ms_of(&v), Some(1_791_673_619_000));
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":"1791673619"}"#).unwrap();
        assert_eq!(expire_ms_of(&v), Some(1_791_673_619_000));
    }

    /// Work 守卫数据源全链路（2026-10-04 修正为 auth.v1.dat 通道）：Local State
    /// （DPAPI 包裹 AES 密钥）→ auth.v1.dat（"v10" 密文 JSON）→ user.id。
    /// 生产 scan_work_login_uid 的测试同构（凭据文件由测试生成）。
    #[test]
    fn work_auth_dat_解密全链路_roundtrip() {
        use base64::Engine as _;
        let base = std::env::temp_dir().join(format!(
            "f80-workauth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let data_dir = base.join("com.qodercn.app.stable");
        std::fs::create_dir_all(&data_dir).unwrap();

        // 1) AES 密钥经 DPAPI 包裹写入 Local State（os_crypt.encrypted_key，生产同构）
        let key = [42u8; 32];
        let protected = crate::vault::dpapi::protect(&key).unwrap();
        let mut wrapped = b"DPAPI".to_vec();
        wrapped.extend_from_slice(&protected);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&wrapped);
        std::fs::write(
            data_dir.join("Local State"),
            serde_json::json!({ "os_crypt": { "encrypted_key": b64 } }).to_string(),
        )
        .unwrap();

        // 2) auth.v1.dat：v10 加密的登录 JSON（生产实测同构：token/user.id 形态）
        let uid = "01a106b2d387786796f68391bdf6b179";
        let plain = serde_json::json!({
            "schemaVersion": 1,
            "token": "dt-test",
            "refreshToken": "drt-test",
            "user": { "id": uid, "name": "nick" }
        })
        .to_string();
        let enc = {
            use aes_gcm::aead::Aead;
            use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
            let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
            let nonce = Nonce::from_slice(&[9u8; 12]);
            let mut blob = b"v10".to_vec();
            blob.extend_from_slice(nonce);
            blob.extend_from_slice(&cipher.encrypt(nonce, plain.as_bytes()).unwrap());
            blob
        };
        std::fs::write(data_dir.join("auth.v1.dat"), &enc).unwrap();

        // 3) 全链路解密：Local State 密钥 → v10 → user.id
        assert_eq!(
            scan_work_login_uid(&data_dir).as_deref(),
            Some(uid),
            "auth.v1.dat 应解密出 user.id"
        );

        // 4) 未登录（无 auth.v1.dat）→ None（fail-open）
        let fresh = base.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        assert_eq!(scan_work_login_uid(&fresh), None);

        // 5) JSON 无 user.id（形态漂移防御）→ None
        let broken_plain = serde_json::json!({ "schemaVersion": 1 }).to_string();
        let enc_broken = {
            use aes_gcm::aead::Aead;
            use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
            let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
            let nonce = Nonce::from_slice(&[9u8; 12]);
            let mut blob = b"v10".to_vec();
            blob.extend_from_slice(nonce);
            blob.extend_from_slice(&cipher.encrypt(nonce, broken_plain.as_bytes()).unwrap());
            blob
        };
        std::fs::write(data_dir.join("auth.v1.dat"), &enc_broken).unwrap();
        assert_eq!(scan_work_login_uid(&data_dir), None, "无 user.id 应返回 None");
        let _ = std::fs::remove_dir_all(&base);
    }
}
