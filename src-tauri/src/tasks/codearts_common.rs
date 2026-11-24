//! CodeArts（华为云码道，国内站）tasks 层公共基础：凭据、账号 id、跨进程锁、
//! token store 的 vault 收敛与迁移。
//!
//! 设计要点（对齐 qoder_common / wb_common 模式）：
//! - 两种凭据共存：手工 AK/SK（长期密钥）与 OAuth 临时 AK/SK+security_token
//!   （PKCE+DPoP 换取，到期前静默续期，见 ensure_fresh_creds）；
//! - 凭证双源：「DB token store 占位行」× 「vault(ns=codearts) 明文」；
//! - 零明文红线：DB 的 token store 中 ak/sk/token/refresh 恒为空串，明文唯一驻留 vault；
//!   vault 写入失败时 DB 保持原状（凭据仅留内存，重新录入可恢复）；
//! - CrossProcLock 为 Windows 命名互斥体（单段点分名 + data_dir 短哈希），
//!   用于 CLI 计划任务与 GUI 进程同刻触发签到/积分时的幂等互斥；
//! - 本模块只依赖 store / vault / fs_utils / sha2，不引用任何 api_server 类型。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 暂态错误标记：网关/调度侧据此区分「网络/5xx 暂态失败」与「凭证永久失效」，
/// 避免一次抖动把本可自愈的账号永久禁用（对齐 qoder_common::TRANSIENT_ERR_TAG）。
pub const TRANSIENT_ERR_TAG: &str = "[codearts-transient]";

/// 凭据永久失效标记：OAuth refresh_token 被服务端拒绝 / 缺续期材料且旧凭据
/// 已过期——重试无解，必须重新登录。区别于 TRANSIENT_ERR_TAG（网络/5xx 抖动）。
/// 网关据此生成「请重新登录 CodeArts 账号」的用户可读提示。
pub const NEEDS_RELOGIN_TAG: &str = "[codearts-needs-relogin]";

/// 判断一段错误信息是否为暂态失败（两侧同源常量，无文案耦合面）。
pub fn is_transient_err(e: &str) -> bool {
    e.starts_with(TRANSIENT_ERR_TAG)
}

/// 判断是否凭据永久失效需重登（与 is_transient_err 互斥）
pub fn is_needs_relogin_err(e: &str) -> bool {
    e.starts_with(NEEDS_RELOGIN_TAG)
}

/// CodeArts 凭据（手写 Debug 脱敏，避免 derive 把任何密钥片段打进日志）。
///
/// 两种来源共存：
/// - 手工 AK/SK（`credential_source="manual"`）：长期密钥，无 security_token；
/// - OAuth（`credential_source="oauth"`）：PKCE+DPoP 换取的一次性 AK/SK +
///   security_token + refresh_token，到期前静默续期。
///
/// 所有敏感字段明文唯一驻留 vault（ns=codearts），DB 恒占位空串。
#[derive(Clone)]
pub struct CodeartsCreds {
    pub ak: String,
    pub sk: String,
    /// STS 临时安全令牌（OAuth 模式必填；手工 AK/SK 为空）
    pub security_token: String,
    /// 凭据到期时间（Unix 秒；0 = 未知/长期）
    pub expires_at: i64,
    /// 静默续期用 refresh_token（OAuth 模式）
    pub refresh_token: String,
    /// 静默续期用 PKCE code_verifier（OAuth 模式）
    pub code_verifier: String,
    /// 静默续期用 DPoP P-256 私钥（PKCS#8 PEM，OAuth 模式）
    pub dpop_key_pem: String,
    /// 来源标记："manual" | "oauth"（仅展示/分流，不含密钥）
    pub credential_source: String,
}

impl Default for CodeartsCreds {
    fn default() -> Self {
        Self {
            ak: String::new(),
            sk: String::new(),
            security_token: String::new(),
            expires_at: 0,
            refresh_token: String::new(),
            code_verifier: String::new(),
            dpop_key_pem: String::new(),
            credential_source: String::new(),
        }
    }
}

impl CodeartsCreds {
    /// 手工 AK/SK 构造函数（保持其他模块既有调用）。
    pub fn new(ak: impl Into<String>, sk: impl Into<String>) -> Self {
        Self {
            ak: ak.into(),
            sk: sk.into(),
            credential_source: "manual".into(),
            ..Default::default()
        }
    }

    /// 凭据是否可用于签名（ak/sk 两侧均非空白）。
    pub fn is_valid(&self) -> bool {
        !self.ak.trim().is_empty() && !self.sk.trim().is_empty()
    }

    /// 是否为 OAuth 临时凭据（需带 security_token 参与签名）。
    pub fn is_oauth(&self) -> bool {
        self.credential_source == "oauth" || !self.security_token.is_empty()
    }

    /// 距过期是否进入续期窗口（提前 30 分钟；0/unknown 视为无需续期）。
    pub fn needs_refresh(&self, lead_secs: i64) -> bool {
        if self.expires_at <= 0 {
            return false;
        }
        self.expires_at.saturating_sub(chrono::Utc::now().timestamp()) <= lead_secs
    }

    /// 凭据是否**此刻仍可签名**（未到期；expires_at 未知视为可用）。
    /// 续期失败时据此判断能否沿用旧凭据：STS 临时凭据在到期前一直有效，
    /// 续期只是提前准备，失败不该让一个还能用的账号直接掉线。
    pub fn usable_now(&self) -> bool {
        self.is_valid()
            && (self.expires_at <= 0 || self.expires_at > chrono::Utc::now().timestamp())
    }
}

impl std::fmt::Debug for CodeartsCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mask = |s: &str| if s.is_empty() { "" } else { "**" };
        f.debug_struct("CodeartsCreds")
            .field("ak", &mask(&self.ak))
            .field("sk", &mask(&self.sk))
            .field("security_token", &mask(&self.security_token))
            .field("refresh_token", &mask(&self.refresh_token))
            .field("code_verifier", &mask(&self.code_verifier))
            .field("dpop_key_pem", &mask(&self.dpop_key_pem))
            .field("expires_at", &self.expires_at)
            .field("credential_source", &self.credential_source)
            .finish()
    }
}

// ── 账号 id ─────────────────────────────────────────────────────────────────

/// 稳定账号 id：`ca-` + sha256("{ak}:{sk}") 前 12 位小写 hex。
/// 同 AK/SK 稳定同 id（防重复入池）；不含任何明文片段。
pub fn account_id_of(ak: &str, sk: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("{ak}:{sk}").as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("ca-{}", &hex[..12])
}

/// OAuth 账号稳定 id：`ca-o-` + sha256(user_id) 前 12 位小写 hex。
/// OAuth 凭据是一次性的（每次登录 AK/SK 都变），不能用 AK/SK 派生 id，
/// 改用上游稳定 user_id；无 user_id 时回退 domain_id，再回退 refresh_token
/// （至少保证同账号重复登录幂等，不重复入池）。
pub fn oauth_account_id_of(user_id: &str, domain_id: &str, refresh_token: &str) -> String {
    let seed = if !user_id.trim().is_empty() {
        format!("u:{user_id}")
    } else if !domain_id.trim().is_empty() {
        format!("d:{domain_id}")
    } else {
        format!("r:{refresh_token}")
    };
    let mut h = Sha256::new();
    h.update(seed.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("ca-o-{}", &hex[..12])
}

// ── token store（vault 收敛 + DB 占位）──────────────────────────────────────

/// token store 整表读改写互斥：load→merge→save 非原子，签到/积分/导入多通道
/// 并发写会互相覆盖丢更新；进程内全局锁串行化表级读改写（仅本地 IO）。
static TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// DB token store 记录里的敏感字段（明文唯一驻留 vault，DB 恒占位空串）。
/// OAuth 模式下 refresh_token/verifier/dpop 私钥同样是敏感字段，一并占位。
const SENSITIVE_KEYS: [&str; 6] = [
    "ak", "sk", "security_token", "refresh_token", "code_verifier", "dpop_key_pem",
];

/// 宽容取字符串（兼容 ak/access_key/accessKey、sk/secret_key/secretKey 候选键）。
fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn cred_from(v: Option<&Value>) -> CodeartsCreds {
    match v.filter(|x| x.is_object()) {
        Some(rec) => CodeartsCreds {
            ak: s_of(crate::fs_utils::dig(rec, &["ak", "access_key", "accessKey", "access_key_id"])),
            sk: s_of(crate::fs_utils::dig(rec, &["sk", "secret_key", "secretKey", "secret_access_key"])),
            security_token: s_of(crate::fs_utils::dig(rec, &["security_token", "securityToken", "x_security_token"])),
            expires_at: rec.get("expires_at").and_then(Value::as_i64).unwrap_or(0),
            refresh_token: s_of(crate::fs_utils::dig(rec, &["refresh_token", "refreshToken"])),
            code_verifier: s_of(crate::fs_utils::dig(rec, &["code_verifier", "codeVerifier"])),
            dpop_key_pem: s_of(crate::fs_utils::dig(rec, &["dpop_key_pem", "dpopKeyPem"])),
            credential_source: s_of(crate::fs_utils::dig(rec, &["credential_source"])),
        },
        None => CodeartsCreds::default(),
    }
}

/// 从 vault 读取指定账号的明文凭据（ns=codearts，键为账号 id）。
/// vault 不可用 / 无记录 → 返回空凭据（上层按 permanent_auth 处理，fail-secure）。
fn vault_creds(data_dir: &std::path::Path, account_id: &str) -> CodeartsCreds {
    let Some(sec) = crate::vault::ns_get(data_dir, "codearts", account_id) else {
        return CodeartsCreds::default();
    };
    cred_from(Some(&sec))
}

/// 生效凭证：优先读 vault(ns=codearts) 明文（明文唯一驻留地）；vault 无记录时
/// 回退 DB 行明文作为 legacy 迁移兜底（迁移前旧数据），下次 save_token 会收敛。
pub fn effective_creds(
    data_dir: &std::path::Path,
    account_id: &str,
) -> Result<CodeartsCreds, String> {
    let db = crate::store::db(data_dir);
    let store = crate::store::docs::codearts_token_store_load(&db);
    let row = store
        .get("tokens")
        .and_then(|t| t.get(account_id))
        .cloned();
    // vault 明文优先（正常路径）；为空再回退 DB 残留明文（legacy）
    let mut creds = vault_creds(data_dir, account_id);
    let legacy = cred_from(row.as_ref());
    if creds.ak.is_empty() {
        creds.ak = legacy.ak.clone();
    }
    if creds.sk.is_empty() {
        creds.sk = legacy.sk.clone();
    }
    if creds.security_token.is_empty() {
        creds.security_token = legacy.security_token.clone();
    }
    if creds.expires_at == 0 {
        creds.expires_at = legacy.expires_at;
    }
    if creds.refresh_token.is_empty() {
        creds.refresh_token = legacy.refresh_token.clone();
    }
    if creds.code_verifier.is_empty() {
        creds.code_verifier = legacy.code_verifier.clone();
    }
    if creds.dpop_key_pem.is_empty() {
        creds.dpop_key_pem = legacy.dpop_key_pem.clone();
    }
    if creds.credential_source.is_empty() {
        creds.credential_source = legacy.credential_source;
    }
    Ok(creds)
}

/// 生效凭证 + 静默续期：OAuth 凭据距到期 ≤30min 时用 refresh_token 换新并落 vault；
/// 手工 AK/SK（无 refresh 材料）与未到期凭据直接返回。所有上游热路径统一走此函数，
/// 确保 OAuth 账号无需再次打开浏览器。
pub fn ensure_fresh_creds(data_dir: &std::path::Path, account_id: &str) -> Result<CodeartsCreds, String> {
    // 快路径：未到续期窗口的绝大多数请求不进临界区
    let initial = effective_creds(data_dir, account_id)?;
    if !initial.is_oauth() || !initial.needs_refresh(30 * 60) {
        return Ok(initial);
    }
    // 按账号串行化续期（H-1 同构）：并发请求排队后重读，后到者直接拿刚写入的新凭据，
    // 避免 refresh_token 轮转下重复续期导致一方失效。
    let lock = refresh_lock_for(account_id);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let creds = effective_creds(data_dir, account_id)?;
    if !creds.needs_refresh(30 * 60) {
        return Ok(creds);
    }
    if creds.refresh_token.is_empty() || creds.code_verifier.is_empty() || creds.dpop_key_pem.is_empty() {
        return Err(format!("{NEEDS_RELOGIN_TAG} CodeArts OAuth 凭据缺少续期材料，请重新登录"));
    }
    // 续期失败不得一票否决：STS 临时凭据在 expires_at 之前一直可签名，
    // 提前 30 分钟续期只是「提前准备」，换新失败说明不了当前凭据已失效。
    // 修复前这里直接 `?` 上抛 → 一个还剩 20 分钟有效的健康账号被判死，
    // 用户侧表现为「CodeArts 反代突然全挂」，而真实原因只是 refresh_token
    // 被别处用过一次（STS5.1806）。
    match crate::tasks::codearts_oauth::refresh_credential(&creds) {
        Ok(next) => match save_token(data_dir, account_id, &next) {
            Ok(()) => Ok(next),
            Err(e) => {
                crate::fs_utils::app_log(data_dir, &format!("CodeArts 续期凭据写入失败（本轮用旧凭据）: {e}"));
                Ok(creds)
            }
        },
        Err(e) => {
            if creds.usable_now() {
                let left = creds.expires_at - chrono::Utc::now().timestamp();
                crate::fs_utils::app_log(
                    data_dir,
                    &format!("CodeArts 续期失败但旧凭据仍有效（剩余 {left}s），本轮沿用: {e}"),
                );
                Ok(creds)
            } else {
                // 确实已过期且续期失败（refresh_token 被拒/一次性轮换已消耗）：
                // 重试无解，标记永久失效让上层提示重新登录，而不是当暂态反复重试
                Err(format!("{NEEDS_RELOGIN_TAG} {e}；请重新登录 CodeArts 账号"))
            }
        }
    }
}

/// 每账号续期互斥锁（进程内，Arc 共享；账号删除不主动回收，条目数随账号量有界）。
static REFRESH_LOCKS: OnceLock<Mutex<std::collections::HashMap<String, std::sync::Arc<Mutex<()>>>>> =
    OnceLock::new();

fn refresh_lock_for(account_id: &str) -> std::sync::Arc<Mutex<()>> {
    let map = REFRESH_LOCKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(account_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

/// 写凭证：敏感字段进 vault（ns=codearts），DB token store 落占位行(ak/sk="")。
/// 顺序保证：vault 写入成功才落 DB；vault 失败时 DB 完全不动
///（否则会把未收敛的明文写进 SQLite，红线倒退）。
pub fn save_token(
    data_dir: &std::path::Path,
    id: &str,
    creds: &CodeartsCreds,
) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // ① vault 先写（失败直接返回，DB 保持原状）；全部敏感字段明文唯一驻留 vault
    let entry = json!({
        "ak": creds.ak,
        "sk": creds.sk,
        "security_token": creds.security_token,
        "expires_at": creds.expires_at,
        "refresh_token": creds.refresh_token,
        "code_verifier": creds.code_verifier,
        "dpop_key_pem": creds.dpop_key_pem,
        "credential_source": creds.credential_source,
    });
    crate::vault::ns_set(data_dir, "codearts", id, &entry).map_err(|e| {
        format!("CodeArts 凭据加密存储失败（数据库未改动，重新录入可恢复）: {e}")
    })?;
    // ② DB 落占位行（整表读改写，保留其他账号未知字段）
    let db = crate::store::db(data_dir);
    let mut store = crate::store::docs::codearts_token_store_load(&db);
    if !store.is_object() {
        store = json!({});
    }
    let obj = store.as_object_mut().unwrap();
    obj.insert("version".into(), json!(1));
    let tokens = obj.entry("tokens").or_insert_with(|| json!({}));
    let tokens_obj = tokens
        .as_object_mut()
        .ok_or_else(|| "token store 结构异常（tokens 非对象）".to_string())?;
    let mut rec = tokens_obj.get(id).cloned().unwrap_or_else(|| json!({}));
    if let Some(rm) = rec.as_object_mut() {
        for k in SENSITIVE_KEYS {
            rm.insert(k.to_string(), json!(""));
        }
        // 非敏感存在标记：DB 的 ak/sk 恒占位，commands 层判断 has_credential
        // 不能靠 ak 非空，应读此字段（或查 vault）；值不含任何密钥片段。
        rm.insert("credential_source".into(), json!("vault"));
        rm.insert("updated_at".into(), json!(crate::fs_utils::now_iso()));
    }
    tokens_obj.insert(id.to_string(), rec);
    crate::store::docs::codearts_token_store_save(&db, &store)
}

/// 删除凭证：先落 DB 摘行（成功才清 vault），避免「库里还在、密钥已删」悬挂态。
pub fn remove_token(data_dir: &std::path::Path, id: &str) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 必须读 DB 原始占位表，不可走 vault 回填后的整表（会把其他账号明文重新落库）
    let db = crate::store::db(data_dir);
    let mut store = crate::store::docs::codearts_token_store_load(&db);
    if let Some(t) = store.get_mut("tokens").and_then(Value::as_object_mut) {
        t.remove(id);
    }
    crate::store::docs::codearts_token_store_save(&db, &store)?;
    crate::vault::ns_remove(data_dir, "codearts", id);
    Ok(())
}

/// 启动迁移：DB 中残留的 ak/sk 明文收敛进 vault + DB 占位化（幂等，无明文时零写入）。
pub fn migrate_token_store(data_dir: &std::path::Path) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = crate::store::db(data_dir);
    let mut store = crate::store::docs::codearts_token_store_load(&db);
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let ids: Vec<String> = tokens.keys().cloned().collect();
    let mut dirty = false;
    for id in ids {
        let plain = cred_from(tokens.get(&id));
        if plain.ak.is_empty() && plain.sk.is_empty() {
            continue;
        }
        let mut entry = crate::vault::ns_get(data_dir, "codearts", &id)
            .unwrap_or_else(|| json!({}));
        if let Some(em) = entry.as_object_mut() {
            if !plain.ak.is_empty() {
                em.insert("ak".into(), json!(plain.ak));
            }
            if !plain.sk.is_empty() {
                em.insert("sk".into(), json!(plain.sk));
            }
        }
        crate::vault::ns_set(data_dir, "codearts", &id, &entry)?;
        if let Some(rm) = tokens.get_mut(&id).and_then(Value::as_object_mut) {
            for k in SENSITIVE_KEYS {
                rm.insert(k.to_string(), json!(""));
            }
        }
        dirty = true;
    }
    if dirty {
        crate::store::docs::codearts_token_store_save(&db, &store)?;
    }
    Ok(())
}

// ── 跨进程互斥（Windows 命名 Mutex；RAII 释放）────────────────────────────

/// 跨进程锁获取失败描述（字符串化，调用方直接拼日志/事件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockFail {
    /// CreateMutexW 内核错误码（锁机制不可用，非他方占用）
    CreateFailed(u32),
    /// 对方持有中，等待超时（真占用，幂等跳过无损失）
    Busy,
    /// WaitForSingleObject 内核错误码
    WaitFailed(u32),
}

impl LockFail {
    pub fn describe(&self) -> String {
        match self {
            Self::CreateFailed(e) => format!("锁创建失败（Win32 err={e}，非他方占用）"),
            Self::Busy => "他方进程持有中（等待超时）".to_string(),
            Self::WaitFailed(e) => format!("锁等待系统失败（Win32 err={e}）"),
        }
    }
}

/// 跨进程锁持有句柄（RAII：Drop 释放；进程崩溃由 OS 回收 Mutex）。
#[cfg(windows)]
pub struct CrossProcLock {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl CrossProcLock {
    /// 尝试在 wait_ms 内获取命名互斥体：
    /// 成功 → (Some(guard), "")；失败 → (None, 原因字符串)。
    /// 锁名单段点分 `Global\AIWorkAssistant.codearts.{scope}.{data_dir哈希前16}`：
    /// ⚠ 内核对象命名空间下无中间目录，多段路径（`Global\a\b\c`）会恒报 err=3，
    /// 故 scope 必须是单段名（如 "checkin"/"credits"）。
    pub fn try_acquire(
        data_dir: &std::path::Path,
        scope: &str,
        wait_ms: u64,
    ) -> (Option<Self>, String) {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let mut h = Sha256::new();
        h.update(data_dir.to_string_lossy().as_bytes());
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let name: Vec<u16> = format!(
            "Global\\AIWorkAssistant.codearts.{scope}.{}",
            &hex[..16]
        )
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
        // SAFETY：name 以 NUL 结尾；CreateMutexW 不拥有调用方内存
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            let err = unsafe { GetLastError() };
            return (None, LockFail::CreateFailed(err).describe());
        }
        // SAFETY：handle 由 CreateMutexW 返回且非 null
        let waited = unsafe { WaitForSingleObject(handle, wait_ms.min(u32::MAX as u64) as u32) };
        if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
            // ABANDONED：前持有进程崩溃，所有权已转移给本进程——正常获取
            (Some(Self { handle }), String::new())
        } else {
            let fail = if waited == WAIT_TIMEOUT {
                LockFail::Busy
            } else {
                LockFail::WaitFailed(unsafe { GetLastError() })
            };
            let reason = fail.describe();
            unsafe { CloseHandle(handle) };
            (None, reason)
        }
    }
}

#[cfg(windows)]
impl Drop for CrossProcLock {
    fn drop(&mut self) {
        // 仅 CloseHandle 不 Release 时：若他方尚持打开句柄，所有权仍挂本线程 → 后续
        // 竞争者全部超时假 busy；故先 ReleaseMutex 再 CloseHandle。
        unsafe { windows_sys::Win32::System::Threading::ReleaseMutex(self.handle) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// 非 Windows 平台占位（项目仅 Windows 发布，保 CI 单测可编译）。
#[cfg(not(windows))]
pub struct CrossProcLock;

#[cfg(not(windows))]
impl CrossProcLock {
    pub fn try_acquire(
        _data_dir: &std::path::Path,
        _scope: &str,
        _wait_ms: u64,
    ) -> (Option<Self>, String) {
        (Some(Self), String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth_creds(expires_at: i64) -> CodeartsCreds {
        CodeartsCreds {
            ak: "AK".into(),
            sk: "SK".into(),
            security_token: "ST".into(),
            expires_at,
            refresh_token: "rt".into(),
            code_verifier: "cv".into(),
            dpop_key_pem: "pem".into(),
            credential_source: "oauth".into(),
        }
    }

    /// 续期窗口内的凭据**仍是可用凭据**：STS 在到期前一直能签名。
    /// 修复前 ensure_fresh_creds 把「续期失败」等同于「账号失效」，
    /// 一个还剩 20 分钟的有效账号会被判死，网关整池掉线。
    #[test]
    fn usable_now_true_while_not_yet_expired() {
        let now = chrono::Utc::now().timestamp();
        // 还剩 20 分钟（已进 30 分钟续期窗口，但未过期）
        let c = oauth_creds(now + 20 * 60);
        assert!(c.needs_refresh(30 * 60), "应已进入续期窗口");
        assert!(c.usable_now(), "未过期就必须仍可用于签名");
    }

    #[test]
    fn usable_now_false_after_expiry() {
        let now = chrono::Utc::now().timestamp();
        assert!(!oauth_creds(now - 1).usable_now(), "已过期不得再用");
    }

    /// expires_at 未知（0）= 长期凭据，视为可用
    #[test]
    fn usable_now_treats_unknown_expiry_as_usable() {
        assert!(oauth_creds(0).usable_now());
        // 但 ak/sk 为空仍然不可用
        let mut c = oauth_creds(0);
        c.ak.clear();
        assert!(!c.usable_now());
    }

    /// 永久失效 vs 暂态失败的标记必须互斥：网关据此决定「提示重登」还是「换号重试」
    #[test]
    fn transient_and_needs_relogin_errors_are_disjoint() {
        let transient = format!("{TRANSIENT_ERR_TAG} 网络抖动");
        let permanent = format!("{NEEDS_RELOGIN_TAG} refresh token 已被使用");
        assert!(is_transient_err(&transient));
        assert!(!is_needs_relogin_err(&transient));
        assert!(is_needs_relogin_err(&permanent));
        assert!(!is_transient_err(&permanent));
    }

    /// needs_refresh 与 usable_now 的边界关系：窗口内且未过期 = 两者皆真
    #[test]
    fn refresh_window_and_usability_are_independent() {
        let now = chrono::Utc::now().timestamp();
        let in_window = oauth_creds(now + 10 * 60);
        assert!(in_window.needs_refresh(30 * 60));
        assert!(in_window.usable_now());

        let fresh = oauth_creds(now + 6 * 3600);
        assert!(!fresh.needs_refresh(30 * 60));
        assert!(fresh.usable_now());
    }
}
