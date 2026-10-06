//! Qoder IDE 存储账号发现/导入（F-80 M3；R-8 侦察结论固化为 L1 凭证通道）。
//!
//! R-8 实测结论（2026-09-27 Windows 侦察，详见设计文档 R-8）：
//! - 登录态真源：`%APPDATA%\QoderCN\User\globalStorage\state.vscdb`
//!   ItemTable 键 `secret://aicoding.auth.userInfo`——TEXT 列存 JSON
//!   `{"type":"Buffer","data":[...]}`，前 3 字节 ASCII "v10"（Chromium os_crypt 形态）
//! - 解密链路（Windows）：`%APPDATA%\QoderCN\Local State`（dataDir 根目录）
//!   → `os_crypt.encrypted_key`（base64 + DPAPI 包裹）→ AES-256-GCM 密钥 →
//!   'v10' + nonce(12) + ct + tag(16)
//! - 解密后 userInfo JSON：id（36 位 uid）/ token（dt- 前缀）/ refreshToken（drt- 前缀）/
//!   expireTime·refreshTokenExpireTime（13 位毫秒字符串）/ name / login_source="qodercn"
//! - CN 版无国际版 auth.v1.dat 形态（R-8 裁决：以 state.vscdb 通道为准）
//!
//! 【macOS 实装 2026-10-05 真机侦察】解密链路差异仅在密钥获取与算法形态：
//! - Keychain 实况（dump-keychain 全量清点）：login keychain 恰有两条 Qoder 自建
//!   条目——「Qoder CN App Safe Storage」(acct="Qoder CN App Key") 与「Qoder CN
//!   Safe Storage」(acct="Qoder CN Key")，均与 Work 登录同刻创建。Electron 43
//!   MAS safeStorage 补丁（feat_ensure_mas_builds...patch）的条目形态
//!   svce="<app> Safe Storage" / acct="<app> Key" 与前者完全吻合 → App 条目为
//!   safeStorage 真实密钥源的置信度最高（但其密码的真机解密验证因钥匙串授权框
//!   无人响应未完成——应用内首次扫描时完成，见安全存储候选与超时设计）
//! - 算法基线：Electron 43 safeStorage sync API 直通 Chromium os_crypt sync
//!   （mac = AES-128-CBC + PBKDF2-HMAC-SHA1(saltysalt, 1003, 16B) + IV=16×0x20 +
//!   PKCS7，MAS 补丁仅改 account 后缀不改算法）。实测排除了
//!   "Qoder CN Safe Storage" 密码在扩展矩阵（sha1/sha256 × 6 iter × 5 salt ×
//!   key16/24/32 × CBC 常数IV/随机IV前缀/GCM）下的全部组合——该条目属客户端
//!   自研加密体系，非 auth.v1.dat 密钥源
//! - auth.v1.dat 本机实测 403 字节、"v10" 前缀（CBC 25 块整除 / GCM
//!   12+372+16 双自洽；Windows 生产已在 GCM 布局验证同前缀）
//! - Work 通道守卫/徽标 fail-open：解密失败 → None（不阻断流程）；IDE 扫描
//!   显式报错附引导文案；oauth/PAT 导入通道不受影响（真机已验证可用）
//!
//! 快照完整性：switcher QODER_IDE_ITEMS 白名单含 Local State 与 state.vscdb（+WAL/SHM），
//! 快照恢复后密钥与密文同槽走，IDE 可自解密——本模块只读不改。
//!
//! 凭证红线：token 不进日志/事件/返回值；摘要只回 qd- id 与昵称。

use std::path::Path;

use serde::Serialize;
use tauri::State;

use super::common::{account_id_of, ide_data_dir, with_pool_mut, QoderAccount};
use crate::state::AppState;
use crate::tasks::qoder_common::{self, QoderCreds};
use crate::tasks::qoder_device::QoderDeviceProfile;

/// 登录态键（state.vscdb ItemTable）
const KEY_USER_INFO: &str = "secret://aicoding.auth.userInfo";

// ── 解密链路（对齐 device_proxy/local_capture.rs Chrome AES 模板）──────────

/// AES 密钥获取（Windows）：Local State（dataDir 根目录）→ os_crypt.encrypted_key
/// → base64 → DPAPI → AES-256-GCM 密钥（单候选）
#[cfg(windows)]
fn ide_aes_keys(data_dir: &Path) -> Result<Vec<Vec<u8>>, String> {
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
    Ok(vec![crate::vault::dpapi::unprotect(&raw[5..])?])
}

/// Electron safeStorage（mac）密钥候选：login Keychain「<name> Safe Storage」条目密码
/// → PBKDF2-HMAC-SHA1(pw, "saltysalt", 1003, 16B)（Chromium mac os_crypt 标准派生）。
/// 经 `security` CLI 读取：密码只在子进程与本进程内存间传递，绝不入日志/错误消息；
/// 首次访问弹钥匙串授权框（security 进程非客户端 ACL 白名单），点「始终允许」后不再弹。
///
/// service 名候选（2026-10-05 本机 dump-keychain 实测两条、均与 Work 登录同刻创建，
/// 真实使用者无法由属性裁决——密钥选择延迟到解密校验，见 ide_aes_keys 返回多候选；
/// 第三候选为 IDE 通道预案：IDE 是独立 Electron 产物，若未来本机出现该条目自动覆盖）：
/// - "Qoder CN App Safe Storage"（acct="Qoder CN App Key"，Work 实测存在）
/// - "Qoder CN Safe Storage"（acct="Qoder CN Key"，实测存在）
/// - "Qoder CN IDE Safe Storage"（未实测——IDE state.vscdb 通道预案，不存在时
///   查找阶段即失败不弹框、不弹授权，零成本候选）
#[cfg(target_os = "macos")]
const SAFE_STORAGE_SERVICES: [&str; 3] = [
    "Qoder CN App Safe Storage",
    "Qoder CN Safe Storage",
    "Qoder CN IDE Safe Storage",
];

/// 授权框无人响应（用户离开屏幕）时 security 子进程会无限阻塞——终端探针曾因此
/// 挂起 25 分钟。限制整体等待时长，超时杀进程并给出可操作指引；stdout/stderr 均
/// 少量输出（一行密码/报错），不会填满 pipe 缓冲区，轮询等待无死锁风险。
#[cfg(target_os = "macos")]
const SECURITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// 读 Keychain 条目密码（不校验用途；授权被拒/条目缺失/等待超时 → Err 附引导文案）
#[cfg(target_os = "macos")]
fn read_keychain_password(svc: &str) -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new("security")
        .args(["find-generic-password", "-w", "-s", svc])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("security CLI 启动失败: {e}"))?;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() >= SECURITY_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{svc}：钥匙串授权等待超时（90s）——弹出的授权框可能无人响应；请重试并在弹框点「始终允许」"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("security 子进程等待失败: {e}")),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut p) = child.stdout.take() {
        let _ = p.read_to_string(&mut stdout);
    }
    if let Some(mut p) = child.stderr.take() {
        let _ = p.read_to_string(&mut stderr);
    }
    if !status.success() {
        // 常见失败：条目不存在（客户端从未登录，查找阶段即失败不弹框）；授权拒绝/取消
        let err = stderr.trim();
        let hint = if err.contains("could not be found") {
            "未找到密钥条目（该形态可能未登录）"
        } else {
            "钥匙串授权被拒绝或取消——请重试并在弹出的授权框点「始终允许」"
        };
        return Err(format!("{svc}：{hint}"));
    }
    let pw = stdout.trim_end_matches(['\n', '\r']).to_string();
    if pw.is_empty() {
        return Err(format!("{svc}：Keychain 密钥条目密码为空"));
    }
    Ok(pw)
}

/// AES 密钥候选（macOS）：全部可读 service 各派生一把，逐个参与解密尝试
#[cfg(target_os = "macos")]
fn ide_aes_keys(_data_dir: &Path) -> Result<Vec<Vec<u8>>, String> {
    use hmac::Hmac;
    use sha1::Sha1;
    let mut keys = Vec::new();
    let mut errs = Vec::new();
    for svc in SAFE_STORAGE_SERVICES {
        match read_keychain_password(svc) {
            Ok(pw) => {
                let mut key = [0u8; 16];
                pbkdf2::pbkdf2::<Hmac<Sha1>>(pw.as_bytes(), b"saltysalt", 1003, &mut key)
                    .map_err(|e| format!("PBKDF2 密钥派生失败: {e}"))?;
                keys.push(key.to_vec());
            }
            Err(e) => errs.push(e),
        }
    }
    if keys.is_empty() {
        return Err(format!("Keychain 读取失败：{}", errs.join("；")));
    }
    Ok(keys)
}

/// 多候选密钥逐一尝试 v10 解密，命中即返回（Windows 单候选恒一步命中）
fn decrypt_v10_any(enc: &[u8], keys: &[Vec<u8>]) -> Option<String> {
    for key in keys {
        if let Some(plain) = decrypt_v10(enc, key) {
            return Some(plain);
        }
    }
    None
}

/// v10 密文解密（Windows）：'v10' + nonce(12) + ct + tag(16) → AES-256-GCM
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

/// v10 密文解密（macOS）：'v10' + AES-128-CBC(key16, IV=16×0x20) + PKCS7
///（Chromium mac os_crypt 形态；与 Windows 同前缀不同算法，见模块头）
#[cfg(target_os = "macos")]
fn decrypt_v10(enc: &[u8], key: &[u8]) -> Option<String> {
    if enc.len() < 19 || &enc[..3] != b"v10" {
        return None;
    }
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
    let ok_key: &[u8; 16] = key.try_into().ok()?;
    let dec = Aes128CbcDec::new(ok_key.into(), &[0x20u8; 16].into());
    let mut buf = enc[3..].to_vec();
    let plain = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

/// secret:// 值形态：TEXT JSON `{"type":"Buffer","data":[...]}` → 字节
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
/// "v10" os_crypt 密文 JSON = token/refreshToken/expiresAt/user{id,name,...}；密钥
/// Windows = 根级 Local State os_crypt.encrypted_key + DPAPI / mac = Keychain
/// Safe Storage，见模块头【macOS 实装】。原 Cookies qoderuid 探测已证伪——Work
/// Cookies 库不存在该 cookie，守卫/徽标恒 None 失效）。
/// 文件缺失/解密失败/JSON 无 user.id → None（调用方 fail-open，不阻断流程）
pub fn scan_work_login_uid(data_dir: &Path) -> Option<String> {
    let enc = std::fs::read(data_dir.join("auth.v1.dat")).ok()?;
    let keys = ide_aes_keys(data_dir).ok()?;
    let plain = decrypt_v10_any(&enc, &keys)?;
    let v: serde_json::Value = serde_json::from_str(&plain).ok()?;
    v.get("user")?
        .get("id")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// IDE 存储当前登录账号（解密产物；token 仅供导入通路，不落日志/事件）
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
///（workbuddy::common::as_ts_seconds 为 pub(super) 不跨模块可用，本地实现同款阈值），
/// 防秒级值被当作毫秒折算成 1970 年
fn expire_ms_of(v: &serde_json::Value) -> Option<i64> {
    let raw = v.get("expireTime")?;
    let ts = raw.as_i64().or_else(|| raw.as_str()?.trim().parse::<i64>().ok())?;
    Some(if ts >= 1_000_000_000_000 { ts } else { ts * 1000 })
}

/// 读 IDE 存储当前登录态（未登录/解密失败 → Err，描述脱敏不含凭证）
pub fn scan_ide_login(data_dir: &Path) -> Result<IdeLogin, String> {
    let keys = ide_aes_keys(data_dir)?;
    let vscdb = data_dir.join("User").join("globalStorage").join("state.vscdb");
    let raw = read_vscdb_key(&vscdb, KEY_USER_INFO)?
        .ok_or("state.vscdb 无登录态（可能未登录或已清除）")?;
    let bytes = parse_buffer(&raw).ok_or("登录态键值形态不识别")?;
    let plain =
        decrypt_v10_any(&bytes, &keys).ok_or("登录态解密失败（密钥或密文不匹配）")?;
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
/// 跨平台（2026-10-05 mac 实装）：Windows = DPAPI 密钥 + AES-256-GCM；
/// mac = Keychain Safe Storage + AES-128-CBC（见模块头【macOS 实装】），
/// 入池/落库主流程两端同构。
/// async：命令含 Keychain/DPAPI 解密 + SQLite 读取等阻塞 IO，移出主线程避免卡 UI。
#[tauri::command(async)]
pub fn qoder_ide_scan(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<QoderIdeScanResult, String> {
    let data_dir = ide_data_dir()
        .ok_or("无法定位 QoderCN 数据目录（平台数据根缺失）")?;
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

/// 平台无关测试（parse_buffer/expire/vscdb 全为跨平台函数；2026-10-05 终审
/// 修复覆盖不对称——原整模块 cfg(all(test, windows)) 使 mac 构建不执行本批测试）
#[cfg(test)]
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

    /// v10 加解密往返（Windows 形态 AES-256-GCM；mac CBC 形态见 mac_tests）
    #[cfg(windows)]
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
    /// DPAPI 为 Windows 专属；mac 解密链路见 mac_tests 的 CBC roundtrip。
    #[cfg(windows)]
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

/// mac 解密链路单测（2026-10-05 mac 实装）：AES-128-CBC + IV=16×0x20 + PKCS7
/// 往返、错钥失败、非 v10 前缀拒绝，与生产 decrypt_v10 同构。
#[cfg(all(test, target_os = "macos"))]
mod mac_tests {
    use super::*;

    /// 用与生产同构的加密方向构造 v10 密文（AES-128-CBC + IV=0x20×16 + PKCS7）
    fn encrypt_v10_mac(plain: &[u8], key: &[u8; 16]) -> Vec<u8> {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        let mut enc = b"v10".to_vec();
        let mut enc2 = Aes128CbcEnc::new(key.into(), &[0x20u8; 16].into())
            .encrypt_padded_vec_mut::<Pkcs7>(plain);
        enc.append(&mut enc2);
        enc
    }

    #[test]
    fn mac_v10_cbc_加解密往返与错钥失败() {
        let key = [7u8; 16];
        let plain = r#"{"schemaVersion":1,"token":"dt-x","user":{"id":"uid-1"}}"#;
        let enc = encrypt_v10_mac(plain.as_bytes(), &key);
        assert_eq!(decrypt_v10(&enc, &key).unwrap(), plain, "同钥往返应还原明文");
        assert!(decrypt_v10(&enc, &[8u8; 16]).is_none(), "错误密钥必须失败（PKCS7 校验）");
        assert!(decrypt_v10(b"v10", &key).is_none(), "过短密文拒绝");
        assert!(decrypt_v10(b"DPx0\x01\x02", &key).is_none(), "非 v10 前缀拒绝");
    }

    #[test]
    fn mac_scan_work_login_uid_全链路_roundtrip() {
        let base = std::env::temp_dir().join(format!(
            "f80-mac-workauth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let data_dir = base.join("com.qodercn.app.stable");
        std::fs::create_dir_all(&data_dir).unwrap();
        let uid = "01a106b2d387786796f68391bdf6b179";
        let plain = serde_json::json!({
            "schemaVersion": 1,
            "token": "dt-test",
            "refreshToken": "drt-test",
            "user": { "id": uid, "name": "nick" }
        })
        .to_string();
        std::fs::write(
            data_dir.join("auth.v1.dat"),
            encrypt_v10_mac(plain.as_bytes(), &[42u8; 16]),
        )
        .unwrap();
        // 密钥获取被 ide_aes_keys（Keychain/DPAPI）接管——本测试直接验证解密半链路
        //（Keychain 无法离线 mock，真机全链路见下方 ignore 探针）
        let enc = std::fs::read(data_dir.join("auth.v1.dat")).unwrap();
        let plain2 = decrypt_v10(&enc, &[42u8; 16]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plain2).unwrap();
        assert_eq!(v.get("user").unwrap().get("id").unwrap().as_str(), Some(uid));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 真机全链路探针（cargo test mac_真机 -- --ignored --nocapture）：
    /// Keychain 候选 service → 生产 ide_aes_keys → auth.v1.dat 解密 → JSON。
    /// 授权框仅首次弹出（每个条目各一次）；密码只在进程内存，绝不打印。
    #[test]
    #[ignore = "真机探针：读 login Keychain（弹授权框）+ 本机 auth.v1.dat"]
    fn mac_真机_keychain到auth_v1_dat全链路() {
        let dir = crate::switcher::profile::profile_for(
            crate::switcher::TargetApp::QoderWork,
            &std::env::temp_dir(),
        )
        .data_dir;
        println!("work data_dir = {}", dir.display());
        let keys = ide_aes_keys(&dir).expect("Keychain 密钥候选获取失败");
        println!("候选密钥数 = {}（各 service 派生，见生产实现）", keys.len());
        let enc = std::fs::read(dir.join("auth.v1.dat")).expect("auth.v1.dat 读取失败");
        println!("auth.v1.dat 长度 = {} 字节，前缀 = {:?}", enc.len(), &enc[..3]);
        match decrypt_v10_any(&enc, &keys) {
            Some(plain) => {
                let v: serde_json::Value =
                    serde_json::from_str(&plain).expect("解密成功但明文非 JSON");
                let uid = v
                    .get("user")
                    .and_then(|u| u.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or("<no user.id>");
                println!("auth.v1.dat 解密成功: JSON 解析通过, user.id = {uid}（链路验证通过）");
            }
            None => println!("全部候选密钥解密失败——跑下方矩阵探针定位真实参数"),
        }
    }

    /// 参数矩阵反推探针（cargo test mac_真机_矩阵 -- --ignored --nocapture）：
    /// 密码（进程内）× PBKDF2(hash×iter×salt×klen) × CBC(常数IV/随机IV前缀)/GCM
    /// × 直作 key。命中即打印参数组（固化到生产实现用），绝不打印密码/明文。
    #[test]
    #[ignore = "真机探针：读 login Keychain（弹授权框）"]
    fn mac_真机_参数矩阵反推() {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit, Nonce};

        let dir = crate::switcher::profile::profile_for(
            crate::switcher::TargetApp::QoderWork,
            &std::env::temp_dir(),
        )
        .data_dir;
        let enc = std::fs::read(dir.join("auth.v1.dat")).expect("auth.v1.dat 读取失败");
        println!("cipher len = {}，prefix = {:?}", enc.len(), &enc[..3]);

        let salts = [
            "saltysalt".to_string(),
            "Qoder CN App Safe Storage".to_string(),
            "Qoder CN Safe Storage".to_string(),
            "Qoder CN App".to_string(),
            "Qoder CN".to_string(),
        ];
        let iters = [1003u32, 1, 2, 4096, 10000, 100000];
        let mut hits = 0usize;
        for svc in SAFE_STORAGE_SERVICES {
            let pw = match read_keychain_password(svc) {
                Ok(p) => p,
                Err(e) => {
                    println!("{svc}: {e}");
                    continue;
                }
            };
            println!("--- {svc}: password 读取成功（长度不入日志）");
            for hash in ["sha1", "sha256"] {
                for iter in iters {
                    for salt in &salts {
                        for klen in [16usize, 24, 32] {
                            let mut key = vec![0u8; klen];
                            if hash == "sha1" {
                                pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
                                    pw.as_bytes(),
                                    salt.as_bytes(),
                                    iter,
                                    &mut key,
                                )
                                .expect("PBKDF2 失败");
                            } else {
                                pbkdf2::pbkdf2::<hmac::Hmac<sha2::Sha256>>(
                                    pw.as_bytes(),
                                    salt.as_bytes(),
                                    iter,
                                    &mut key,
                                )
                                .expect("PBKDF2 失败");
                            }
                            // CBC 常数 IV（v10 + ct）
                            for ivb in [0x20u8, 0x00] {
                                if let Some(p) = cbc_probe(&key, &[ivb; 16], &enc[3..]) {
                                    if serde_json::from_slice::<serde_json::Value>(&p).is_ok() {
                                        hits += 1;
                                        println!(
                                            ">>> HIT CBC: {svc} PBKDF2-{hash} iter={iter} salt={salt} key{klen} IV={ivb:#04x}"
                                        );
                                    }
                                }
                            }
                            // CBC 随机 IV 前缀（v10 + IV16 + ct）
                            if enc.len() > 19 && (enc.len() - 19) % 16 == 0 {
                                let mut ivp = [0u8; 16];
                                ivp.copy_from_slice(&enc[3..19]);
                                if let Some(p) = cbc_probe(&key, &ivp, &enc[19..]) {
                                    if serde_json::from_slice::<serde_json::Value>(&p).is_ok() {
                                        hits += 1;
                                        println!(
                                            ">>> HIT CBC-IVPREFIX: {svc} PBKDF2-{hash} iter={iter} salt={salt} key{klen}"
                                        );
                                    }
                                }
                            }
                            // GCM（nonce12 前缀 + tag16 尾）
                            if enc.len() > 31 {
                                let nonce = Nonce::from_slice(&enc[3..15]);
                                let tag = &enc[enc.len() - 16..];
                                let ct = &enc[15..enc.len() - 16];
                                let mut ct_tag = ct.to_vec();
                                ct_tag.extend_from_slice(tag);
                                let ok = match klen {
                                    16 => Aes128Gcm::new_from_slice(&key)
                                        .ok()
                                        .and_then(|c| c.decrypt(nonce, ct_tag.as_slice()).ok()),
                                    32 => Aes256Gcm::new_from_slice(&key)
                                        .ok()
                                        .and_then(|c| c.decrypt(nonce, ct_tag.as_slice()).ok()),
                                    _ => None,
                                };
                                if let Some(p) = ok {
                                    if serde_json::from_slice::<serde_json::Value>(&p).is_ok() {
                                        hits += 1;
                                        println!(
                                            ">>> HIT GCM: {svc} PBKDF2-{hash} iter={iter} salt={salt} key{klen} (nonce12+ct+tag16)"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if hits == 0 {
            println!("EXTENDED_MATRIX_NO_HIT——密钥来源可能非这两个条目密码（需换向：MITM/守卫 fail-open 通道）");
        } else {
            println!("共 {hits} 处命中——按命中参数固化生产实现");
        }
    }

    /// 矩阵 CBC 探针：key16/24/32 → AES-128/192/256-CBC + PKCS7，成功返回明文
    fn cbc_probe(key: &[u8], iv: &[u8; 16], ct: &[u8]) -> Option<Vec<u8>> {
        use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
        let mut buf = ct.to_vec();
        let plain = match key.len() {
            16 => {
                cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
                    .decrypt_padded_mut::<Pkcs7>(&mut buf)
                    .ok()?
                    .to_vec()
            }
            24 => {
                cbc::Decryptor::<aes::Aes192>::new(key.into(), iv.into())
                    .decrypt_padded_mut::<Pkcs7>(&mut buf)
                    .ok()?
                    .to_vec()
            }
            32 => {
                cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into())
                    .decrypt_padded_mut::<Pkcs7>(&mut buf)
                    .ok()?
                    .to_vec()
            }
            _ => return None,
        };
        Some(plain)
    }
}
