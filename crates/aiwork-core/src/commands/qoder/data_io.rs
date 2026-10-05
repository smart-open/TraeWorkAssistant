//! Qoder 账号池导出/导入（F-80 M4 平移，对照 WorkBuddy F-46 扩展同语义）。
//!
//! 导出：`kind: "aiwork-qoder-pool"` + version 强校验；include_credentials 开关
//! （池/凭证分离）。含凭证导出**强制密码加密**（审查 P1-1）：载荷 JSON 以
//! AES-256-GCM 加密（口令 KDF = Argon2id，疑点⑤迁移：19MiB/t=2，旧版为迭代
//! SHA-256 10 万轮——导入按信封 `kdf` 字段分派，旧文件兼容可解），信封 = 魔数
//! `AIWQENC1` + salt(16) + nonce(12) + 密文（含 16 字节 tag），base64 后作
//! `data` 字段随信封 JSON 导出——凭证不再明文落盘；不设密码直接拒绝导出
//! （兑现「含凭证必须设密码」承诺）。不含凭证导出行为不变（明文本就无敏感数据）。
//! 导入：检测魔数信封 → 必须提供密码解密（密码错误给友好错误）→ 走统一
//! kind/version 校验；无魔数走原明文路径（旧明文导出文件向后兼容）。
//! id 三重校验（非空/ensure_uid_safe/qd-<12位十六进制小写>）→
//! find_uid_or_id 幂等原位更新（uid 优先 id 兜底）→ credential 回写 token store。
//!
//! 与 WorkBuddy 的关键差异（device_profile 指纹红线，§5.10）：指纹入池生成一次
//! 永不轮换——命中已有账号时 device_profile 仅在本地为空才补入，绝不覆盖
//! （覆盖等于轮换指纹）；新增账号时采用导出文件携带的指纹。
//!
//! docker 适配：`crate::vault::csprng_fill`（Windows BCryptGenRandom）无对应实现，
//! salt/nonce 随机源改 `rand::rngs::OsRng`（等价 OS CSPRNG，跨平台可用）。

use base64::Engine as _;
use rand::RngCore;
use serde_json::Value;

use super::common::{load_pool, load_pool_checked, save_pool, QoderAccount};
use crate::fs_utils;
use crate::state::AppState;

// ── 凭证加密信封（审查 P1-1）────────────────────────────────────────────────

/// 加密信封魔数（8 字节）。完整布局：magic(8) + salt(16) + nonce(12) + AES-256-GCM
/// 密文（末尾自带 16 字节认证 tag）。
const EXPORT_MAGIC: &[u8; 8] = b"AIWQENC1";
/// 口令派生盐长度（随机生成，防彩虹表/跨文件重用；Argon2 要求盐 ≥8 字节，16 充分）
const SALT_LEN: usize = 16;
/// AES-GCM 标准 nonce 长度（96 位）
const NONCE_LEN: usize = 12;
/// 旧版口令 KDF 迭代轮数（sha2 链式慢哈希）：仅用于解密历史导出文件——
/// 新导出一律 Argon2id（疑点⑤），此常量不再用于新加密
const KDF_ITERATIONS: u32 = 100_000;

/// OS CSPRNG 填充（docker 版：rand OsRng 等价 main 版 vault::csprng_fill 的
/// BCryptGenRandom；失败即 Err，不降级弱熵）
fn csprng_fill(buf: &mut [u8]) -> Result<(), String> {
    rand::rngs::OsRng.try_fill_bytes(buf)
        .map_err(|e| format!("系统随机源不可用: {e}"))
}

/// 口令 KDF 算法（疑点⑤ Argon2 迁移）：
/// - Argon2id：新导出格式（信封 `kdf: "argon2id"`，19MiB/t=2/p=1，抗 GPU/ASIC）
/// - Sha256Iters：历史格式（`kdf: "sha256-iter-N"`），兼容解密保留
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KdfScheme {
    Argon2id,
    Sha256Iters(u32),
}

/// Argon2id 口令派生（19MiB 内存困难 / t=2 / p=1 / 32 字节输出，OWASP 推荐档）
fn kdf_password_argon2(password: &[u8], salt: &[u8]) -> Result<[u8; 32], String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| format!("口令派生参数无效: {e}"))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    a2.hash_password_into(password, salt, &mut key)
        .map_err(|e| format!("口令派生失败: {e}"))?;
    Ok(key)
}

/// 旧版口令 KDF：链式迭代 SHA-256（10 万轮）。仅解密历史导出文件时按信封
/// JSON `kdf` 字段记录的导出时轮数派生（sha2 为既有依赖，依赖层已开优化）；
/// 新导出一律 Argon2id（疑点⑤）
fn kdf_password_iters(password: &[u8], salt: &[u8], iters: u32) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password);
    let mut acc = h.finalize();
    for _ in 1..iters {
        let mut h = Sha256::new();
        h.update(acc);
        h.update(salt);
        acc = h.finalize();
    }
    acc.into()
}

/// 明文 JSON → 加密信封字节（魔数+salt+nonce+密文）。salt/nonce 取自 OS CSPRNG
/// （CSPRNG 失败直接报错，不降级弱熵）。口令派生 = Argon2id（疑点⑤）。
pub(crate) fn seal_payload(plain: &[u8], password: &str) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit};
    let mut salt = [0u8; SALT_LEN];
    csprng_fill(&mut salt)?;
    let mut nonce = [0u8; NONCE_LEN];
    csprng_fill(&mut nonce)?;
    let key = kdf_password_argon2(password.as_bytes(), &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("加密初始化失败: {e}"))?;
    let ct = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), plain)
        .map_err(|e| format!("载荷加密失败: {e}"))?;
    let mut out = Vec::with_capacity(EXPORT_MAGIC.len() + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(EXPORT_MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 加密信封字节 → 明文 JSON。魔数不符/数据过短 = 非本应用生成的加密导出；
/// 解密失败（GCM 认证不过）= 密码错误或文件损坏，均给友好错误。
/// 口令派生按信封 `kdf` 字段分派（疑点⑤：argon2id 新格式 / sha256-iter-N 旧格式）。
pub(crate) fn open_payload(
    sealed: &[u8],
    password: &str,
    scheme: KdfScheme,
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit};
    let head = EXPORT_MAGIC.len() + SALT_LEN + NONCE_LEN;
    if sealed.len() <= head || &sealed[..EXPORT_MAGIC.len()] != EXPORT_MAGIC {
        return Err("文件不是本应用生成的加密导出（缺少 AIWQENC1 魔数头）".into());
    }
    let salt = &sealed[EXPORT_MAGIC.len()..EXPORT_MAGIC.len() + SALT_LEN];
    let nonce = &sealed[EXPORT_MAGIC.len() + SALT_LEN..head];
    let key = match scheme {
        KdfScheme::Argon2id => kdf_password_argon2(password.as_bytes(), salt)?,
        KdfScheme::Sha256Iters(n) => kdf_password_iters(password.as_bytes(), salt, n),
    };
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("解密初始化失败: {e}"))?;
    cipher
        .decrypt(aes_gcm::Nonce::from_slice(nonce), &sealed[head..])
        .map_err(|_| "解密失败：导出密码错误或文件已损坏".to_string())
}

/// 识别载荷是否加密信封：`data` 字段 base64 解码后以魔数开头 → 返回信封字节；
/// 其余形态（明文导出 JSON）返回 None，走原明文导入路径（向后兼容）。
fn envelope_data_of(payload: &Value) -> Option<Vec<u8>> {
    let data = payload.get("data")?.as_str()?;
    let raw = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    raw.starts_with(EXPORT_MAGIC).then_some(raw)
}

/// 解析信封 JSON 的 `kdf` 字段 → 派生算法分派（疑点⑤）。
/// - `argon2id`：新版导出
/// - `sha256-iter-N`：旧版导出，按各自导出时轮数解密
/// - 缺失：旧版 sha256 时代的首版加密文件，按旧默认轮数回退
/// - 其他前缀 / 轮数越界（0 或超 1000 万，防伪造大轮数拖死导入线程）=
///   更新版本格式，明确报错引导升级，不静默降级
fn kdf_scheme_of(payload: &Value) -> Result<KdfScheme, String> {
    const MAX_KDF_ITERS: u32 = 10_000_000;
    let raw = match payload.get("kdf").and_then(Value::as_str) {
        None => return Ok(KdfScheme::Sha256Iters(KDF_ITERATIONS)),
        Some(s) => s,
    };
    if raw == "argon2id" {
        return Ok(KdfScheme::Argon2id);
    }
    let n = raw
        .strip_prefix("sha256-iter-")
        .ok_or("导出文件的 KDF 算法不识别（可能由更新版本的应用生成），请升级后重试")?;
    let n: u32 = n
        .parse()
        .map_err(|_| "导出文件的 KDF 参数不识别（可能由更新版本的应用生成），请升级后重试".to_string())?;
    if n == 0 || n > MAX_KDF_ITERS {
        return Err("导出文件的 KDF 轮数超出支持范围（可能由更新版本的应用生成），请升级后重试".into());
    }
    Ok(KdfScheme::Sha256Iters(n))
}

// ── 导出 ────────────────────────────────────────────────────────────────────

/// 导出账号池：元数据必含；include_credentials=true 时附工具侧凭证副本
/// （迁移场景用）。**含凭证必须提供 password**：载荷以 AES-256-GCM 加密为信封
/// JSON（`encrypted: true` + `data: <base64>`），拒绝明文凭证导出；不含凭证导出
/// 行为不变（明文，无敏感数据）。池字段全量导出（device_profile 恒带，
/// 供异机导入沿用同一指纹）；凭证副本剥离 machine_id/machine_token（§5.10 对齐
/// 导入侧：token store 正常不落指纹，历史残留防泄漏）。
pub fn qoder_accounts_export(
    state: &AppState,
    include_credentials: Option<bool>,
    password: Option<String>,
) -> Result<Value, String> {
    let pool = load_pool(state);
    let include_cred = include_credentials.unwrap_or(false);
    // 密码归一化：trim 后为空视为未提供（防误触空白密码）
    let password = password.filter(|p| !p.trim().is_empty());
    if include_cred && password.is_none() {
        return Err(
            "含凭证导出必须设置导出密码：凭证将以 AES-256-GCM 加密后写入导出文件，\
             不提供密码将拒绝导出（不含凭证的元数据导出无需密码）"
                .into(),
        );
    }
    let tokens = if include_cred {
        crate::tasks::qoder_common::load_token_store(state)
            .get("tokens")
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let accounts: Vec<Value> = pool
        .iter()
        .map(|a| {
            let mut v = serde_json::to_value(a).map_err(|e| format!("序列化失败: {e}"))?;
            if include_cred {
                // §5.10 对齐导入侧（审查 P3）：导出前剥离设备字段——正常链路
                // token store 不落指纹，但历史版本/旁路写入存在时防泄漏
                //（machine_token 等同设备级凭证，与 access_token 同敏感）
                let mut cred = tokens.get(&a.id).cloned().unwrap_or(Value::Null);
                if let Some(obj) = cred.as_object_mut() {
                    obj.remove("machine_id");
                    obj.remove("machine_token");
                }
                v["credential"] = cred;
            }
            Ok(v)
        })
        .collect::<Result<Vec<_>, String>>()?;
    // 分组定义随载荷导出（审查修复）：成员关系存在账号 group_id 上，异机导入时
    // 无分组定义会产生幽灵 group_id（聚合落空）。旧版导入器忽略未知字段，向后兼容
    let groups: Vec<crate::models::Group> =
        crate::store::db(&state.data_dir).kv_get("qoder_groups");
    let payload = serde_json::json!({
        "kind": "aiwork-qoder-pool",
        "version": 1,
        "exported_at": fs_utils::now_iso(),
        "include_credentials": include_cred,
        "groups": groups,
        "accounts": accounts,
    });
    match (include_cred, password) {
        (true, Some(pwd)) => {
            // 加密信封导出：明文载荷序列化后整体加密，base64 随信封 JSON 落盘
            let plain = serde_json::to_vec(&payload).map_err(|e| format!("序列化失败: {e}"))?;
            let sealed = seal_payload(&plain, &pwd)?;
            Ok(serde_json::json!({
                "kind": "aiwork-qoder-pool",
                "version": 1,
                "exported_at": fs_utils::now_iso(),
                "include_credentials": true,
                "encrypted": true,
                "cipher": "AES-256-GCM",
                // 疑点⑤：新导出一律 Argon2id（导入侧按此字段分派，旧 sha256-iter-N 兼容）
                "kdf": "argon2id",
                "data": base64::engine::general_purpose::STANDARD.encode(&sealed),
            }))
        }
        // 不含凭证：明文元数据导出（无敏感数据）；密码即使误传也忽略
        _ => Ok(payload),
    }
}

// ── 导入 ────────────────────────────────────────────────────────────────────

/// 单账号合并结果：Ok((生效 id, 是否新增))；Err = 拒绝原因。
type MergeResult = Result<(String, bool), String>;

/// 单账号幂等合并（纯函数，可单测）：uid 优先 id 兜底原位更新，保留原 id
/// （换 token / 异机 id 的同账号不再产生重复条目，分组引用不悬空）。
///
/// 覆盖规则：
/// - 更新：uid/nickname/phone_masked/plan/credential_source/token_expires_at
///   非空（Some）才覆盖；group_id/note 本地可编辑不覆盖；needs_relogin/
///   credits_* 属本地运行态不覆盖；device_profile 仅本地为空才补入（指纹红线）
/// - 新增：导出文件字段全量采用（含 group_id/note/device_profile）
fn merge_account(pool: &mut Vec<QoderAccount>, a: &Value) -> MergeResult {
    let parsed: QoderAccount = serde_json::from_value(a.clone())
        .map_err(|e| format!("账号字段解析失败: {e}"))?;
    let id = parsed.id.clone();
    if id.is_empty() {
        return Err("缺少 id".into());
    }
    // 字符集白名单，杜绝 `..`/绝对路径/分隔符注入
    if let Err(e) = fs_utils::ensure_uid_safe(&id) {
        return Err(e);
    }
    // id 规则校验：须与池内生成规则一致——qd-<sha256 前 12 位十六进制小写>
    let hex = id.strip_prefix("qd-").unwrap_or("");
    if hex.len() != 12 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("id 不符合 qd-<12位十六进制小写> 规则".into());
    }
    // find_uid_or_id：uid 优先，id 兜底（两次顺序查找，避免同时借用 pool 两次）
    let hit = if !parsed.uid.is_empty() {
        pool.iter_mut().find(|x| x.uid == parsed.uid)
    } else {
        None
    };
    let hit = match hit {
        Some(x) => Some(x),
        None => pool.iter_mut().find(|x| x.id == id),
    };
    if let Some(x) = hit {
        if !parsed.uid.is_empty() {
            x.uid = parsed.uid.clone();
        }
        if !parsed.nickname.is_empty() {
            x.nickname = parsed.nickname.clone();
        }
        if !parsed.phone_masked.is_empty() {
            x.phone_masked = parsed.phone_masked.clone();
        }
        if !parsed.plan.is_empty() {
            x.plan = parsed.plan.clone();
        }
        if !parsed.credential_source.is_empty() {
            x.credential_source = parsed.credential_source.clone();
        }
        if parsed.token_expires_at.is_some() {
            x.token_expires_at = parsed.token_expires_at;
        }
        // 指纹红线：本地为空才补入，绝不覆盖已有档案（覆盖 = 轮换指纹）
        if x.device_profile.is_none() {
            x.device_profile = parsed.device_profile.clone();
        }
        return Ok((x.id.clone(), false));
    }
    pool.push(parsed);
    Ok((id, true))
}

/// 明文含凭证检测（F-80-余 v2 迁移提示）：任一账号 credential 含非空
/// access_token / pat 即视为明文凭证文件（与导入回写落库判据同口径）
fn has_plaintext_credentials(payload: &Value) -> bool {
    payload
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter().any(|a| {
                a.get("credential")
                    .filter(|c| c.is_object())
                    .map(|c| {
                        !c.get("access_token")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .is_empty()
                            || !c.get("pat").and_then(Value::as_str).unwrap_or("").is_empty()
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// 账号池导入：解析导出文件 → 逐账号幂等入池 + 凭证回写 token store
/// （含凭证时按生效 id 写回，与池条目对齐）。
/// 加密信封（AIWQENC1 魔数）必须提供 password 解密后再走统一校验链路；
/// 无魔数走原明文路径（旧明文导出文件向后兼容，password 提供了也忽略）。
/// 明文含凭证文件导入成功时结果带 `plaintext_credentials: true`（F-80-余 v2
/// 迁移提示：前端提醒重新以加密格式导出归档；不自动改动用户文件）。
pub fn qoder_accounts_import(
    state: &AppState,
    payload: Value,
    password: Option<String>,
) -> Result<Value, String> {
    // 加密信封解包（审查 P1-1）：识别在 kind 校验之前——信封外层无 accounts 字段
    let mut plaintext_credentials = false;
    let payload = match envelope_data_of(&payload) {
        Some(sealed) => {
            let pwd = password.filter(|p| !p.trim().is_empty()).ok_or(
                "该导出文件已加密（AIWQENC1 信封），需要提供导出时设置的密码才能导入",
            )?;
            let plain = open_payload(&sealed, &pwd, kdf_scheme_of(&payload)?)?;
            serde_json::from_slice::<Value>(&plain)
                .map_err(|e| format!("解密后内容解析失败: {e}"))?
        }
        None => {
            plaintext_credentials = has_plaintext_credentials(&payload);
            payload
        }
    };
    if payload.get("kind").and_then(Value::as_str) != Some("aiwork-qoder-pool") {
        return Err("文件格式无法识别（缺少 aiwork-qoder-pool 标记）".into());
    }
    // 导出方恒写 version:1；导入同样校验（后续格式演进时可按版本分支）
    if payload.get("version").and_then(Value::as_i64) != Some(1) {
        return Err("导出文件版本不识别（version 必须为 1）".into());
    }
    let accounts = payload
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("导出文件缺少 accounts 数组")?;
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut with_cred = 0usize;
    let mut rejected: Vec<Value> = Vec::new();
    // 全程持池锁：签到/刷新/积分回写等通道的「load→改→save」并发时整池覆盖会丢导入。
    // 写路径必须走 checked 版：池存在损坏行时拒绝导入（坏行静默丢弃后 save 会整池
    // 覆盖永久丢账号，违反 common.rs 红线）
    let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_pool_checked(state)?;
    // 分组定义合并（审查修复）：导出载荷携带 groups（旧版无此字段则跳过），
    // 按 id 幂等 upsert——本地已有同 id 分组保留本地定义（用户可能已改名），仅补缺失。
    // 持池锁执行：与并发导入互斥，防 kv("qoder_groups") 读改写竞争丢定义；
    // 且池损坏提前 Err 时不再产生分组定义副作用
    let store = crate::store::db(&state.data_dir);
    let mut defs: Vec<crate::models::Group> = store.kv_get("qoder_groups");
    if let Some(groups) = payload.get("groups").and_then(Value::as_array) {
        let mut defs_changed = false;
        for g in groups {
            let Ok(def) = serde_json::from_value::<crate::models::Group>(g.clone()) else {
                continue;
            };
            if def.id.is_empty() || defs.iter().any(|x| x.id == def.id) {
                continue;
            }
            defs.push(def);
            defs_changed = true;
        }
        if defs_changed {
            store.kv_set("qoder_groups", &defs)?;
        }
    }
    let valid_group_ids: std::collections::HashSet<String> =
        defs.iter().map(|g| g.id.clone()).collect();
    // 凭证写入先收集、落池成功后再执行（审查修复）：原实现循环内先落凭证
    // （token store + vault 永久提交）、save_pool 在循环后执行——save_pool 失败时
    // 凭证已落库而池条目丢失，产生列表不可见、移除接口原先拒绝清理的孤儿凭证。
    // 调序后最坏情况是凭证单条失败（rejected 聚合、重新导入可补齐），语义不变
    let mut cred_writes: Vec<(String, crate::tasks::qoder_common::QoderCreds)> = Vec::new();
    for a in accounts {
        match merge_account(&mut pool, a) {
            Ok((final_id, is_new)) => {
                if is_new {
                    added += 1;
                } else {
                    updated += 1;
                }
                // 凭证副本回写（导出时含凭证才有效）：至少有作业令牌或 PAT 才落库
                if let Some(cred) = a.get("credential").filter(|c| c.is_object()) {
                    // §5.10 红线纵深：入库前剥离设备字段（指纹只存账号池 device_profile，
                    // token store 不落指纹——与 ensure_fresh 客户端通道落库语义一致）
                    let mut creds = crate::tasks::qoder_common::creds_of(cred);
                    creds.machine_id.clear();
                    creds.machine_token.clear();
                    if !creds.access_token.is_empty() || !creds.pat.is_empty() {
                        cred_writes.push((final_id.clone(), creds));
                    }
                }
            }
            Err(reason) => {
                let id = a.get("id").and_then(Value::as_str).unwrap_or("");
                rejected.push(serde_json::json!({ "id": id, "reason": reason }));
            }
        }
    }
    // 幽灵分组防御（审查修复）：合并分组定义后仍指向不存在分组的账号回落「未分组」。
    // 同机重导不受影响（id 命中本地/已合并定义）；旧版无 groups 载荷在同机导入同样命中
    for a in pool.iter_mut() {
        if !a.group_id.is_empty() && !valid_group_ids.contains(&a.group_id) {
            a.group_id.clear();
        }
    }
    save_pool(state, &pool)?;
    for (final_id, creds) in cred_writes {
        // 单账号凭证落库失败不中断整体（部分导入比整体中断更糟）：
        // 聚合进 rejected 继续导入；账号信息已入池，重新导入可补齐凭证
        match crate::tasks::qoder_common::save_token_store(state, &final_id, &creds) {
            Ok(()) => with_cred += 1,
            Err(e) => rejected.push(serde_json::json!({
                "id": final_id,
                "reason": format!("账号已入池，但凭证落库失败：{e}（重新导入可补齐）"),
            })),
        }
    }
    // 导入完成联动网关池热重载（fail-open 新账号即时入池调度；服务未运行时 no-op）
    crate::api_server::runtime::reload_pools_after_change(state);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "qoder: 账号池导入 新增 {added} / 更新 {updated} / 带凭证 {with_cred} / 拒绝 {}{}",
            rejected.len(),
            if plaintext_credentials { " / 明文凭证文件（迁移提示）" } else { "" },
        ),
    );
    Ok(serde_json::json!({
        "added": added,
        "updated": updated,
        "skipped": 0,
        "with_credentials": with_cred,
        "rejected": rejected,
        // F-80-余 v2 迁移提示：true = 历史明文含凭证文件，前端提醒重新加密导出归档
        "plaintext_credentials": plaintext_credentials,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        envelope_data_of, has_plaintext_credentials, kdf_password_iters, kdf_scheme_of,
        merge_account, open_payload, seal_payload, KdfScheme, EXPORT_MAGIC, KDF_ITERATIONS,
        NONCE_LEN, SALT_LEN,
    };
    use crate::commands::qoder::common::QoderAccount;
    use crate::tasks::qoder_device::QoderDeviceProfile;

    /// F-80-余 v2 迁移提示判据：任一账号 credential 含非空 access_token/pat
    #[test]
    fn plaintext_credential_detection() {
        let with = serde_json::json!({"accounts":[{"id":"a","credential":{"access_token":"pt-x"}}]});
        assert!(has_plaintext_credentials(&with));
        let with_pat = serde_json::json!({"accounts":[{"id":"a","credential":{"pat":"jt-x"}}]});
        assert!(has_plaintext_credentials(&with_pat));
        let without = serde_json::json!({"accounts":[{"id":"a"}]});
        assert!(!has_plaintext_credentials(&without));
        let empty_cred = serde_json::json!({"accounts":[{"id":"a","credential":{"access_token":""}}]});
        assert!(!has_plaintext_credentials(&empty_cred));
        let empty = serde_json::json!({"other": 1});
        assert!(!has_plaintext_credentials(&empty));
    }

    /// KDF 算法分派解析（疑点⑤ Argon2 迁移 + 审查 P3 兼容）：缺失回退旧默认
    /// 轮数 / argon2id 新格式 / sha256-iter-N 按导出轮数 / 前缀不识别、非数字、
    /// 越界（0/超上限）明确报错（防静默降级与伪造大轮数 DoS）
    #[test]
    fn kdf_scheme_解析() {
        assert_eq!(
            kdf_scheme_of(&serde_json::json!({})).unwrap(),
            KdfScheme::Sha256Iters(KDF_ITERATIONS),
            "字段缺失 = sha256 时代首版文件，回退旧默认轮数"
        );
        assert_eq!(
            kdf_scheme_of(&serde_json::json!({"kdf": "argon2id"})).unwrap(),
            KdfScheme::Argon2id
        );
        assert_eq!(
            kdf_scheme_of(&serde_json::json!({"kdf": "sha256-iter-100000"})).unwrap(),
            KdfScheme::Sha256Iters(KDF_ITERATIONS)
        );
        // 未来上调轮数后旧文件按自身轮数解密
        assert_eq!(
            kdf_scheme_of(&serde_json::json!({"kdf": "sha256-iter-250000"})).unwrap(),
            KdfScheme::Sha256Iters(250_000)
        );
        assert!(kdf_scheme_of(&serde_json::json!({"kdf": "argon2-x"})).is_err());
        assert!(kdf_scheme_of(&serde_json::json!({"kdf": "scrypt"})).is_err());
        assert!(kdf_scheme_of(&serde_json::json!({"kdf": "sha256-iter-abc"})).is_err());
        assert!(kdf_scheme_of(&serde_json::json!({"kdf": "sha256-iter-0"})).is_err());
        assert!(kdf_scheme_of(&serde_json::json!({"kdf": "sha256-iter-99999999"})).is_err());
    }

    /// 疑点⑤ 双格式 roundtrip：新导出（Argon2id）可解；旧格式（sha256-iter-N，
    /// 手工按旧算法构造信封）仍可解——升级用户的存量加密备份不受影响
    #[test]
    fn 加密信封_双kdf格式_roundtrip() {
        // 显式切片类型：br#"..."# 推断为 &[u8; N]，aes-gcm 的 Into<Payload> 仅对 &[u8] 实现
        let plain: &[u8] = br#"{"kind":"aiwork-qoder-pool","version":1}"#;

        // 新格式：seal（Argon2id）→ 按信封 kdf 字段解析 → open
        let sealed = seal_payload(plain, "pw123").unwrap();
        let env = serde_json::json!({ "kdf": "argon2id" });
        let out = open_payload(&sealed, "pw123", kdf_scheme_of(&env).unwrap()).unwrap();
        assert_eq!(out, plain);
        // 密码错误必须失败（GCM 认证）
        assert!(open_payload(&sealed, "wrong", kdf_scheme_of(&env).unwrap()).is_err());
        // 算法标错（声明 sha256 但实际 argon2 加密）必须解不开，不静默跨算法兜底
        let env_sha = serde_json::json!({ "kdf": "sha256-iter-100000" });
        assert!(open_payload(&sealed, "pw123", kdf_scheme_of(&env_sha).unwrap()).is_err());

        // 旧格式：按 sha2 链式 10 万轮手工构造信封（历史版本 seal 的复刻），
        // kdf 字段缺失（首版）与 sha256-iter-N 两种信封形态均可解
        for kdf_field in [None, Some("sha256-iter-100000")] {
            use aes_gcm::aead::Aead;
            use aes_gcm::{Aes256Gcm, KeyInit};
            let mut sealed_old = Vec::new();
            sealed_old.extend_from_slice(EXPORT_MAGIC);
            let salt = [7u8; SALT_LEN];
            let nonce = [9u8; NONCE_LEN];
            sealed_old.extend_from_slice(&salt);
            sealed_old.extend_from_slice(&nonce);
            let key = kdf_password_iters(b"pw123", &salt, KDF_ITERATIONS);
            let ct = Aes256Gcm::new_from_slice(&key)
                .unwrap()
                .encrypt(aes_gcm::Nonce::from_slice(&nonce), plain)
                .unwrap();
            sealed_old.extend_from_slice(&ct);
            let env_old = match kdf_field {
                None => serde_json::json!({}),
                Some(k) => serde_json::json!({ "kdf": k }),
            };
            let out = open_payload(&sealed_old, "pw123", kdf_scheme_of(&env_old).unwrap())
                .unwrap_or_else(|e| panic!("旧格式应可解（kdf={kdf_field:?}）: {e}"));
            assert_eq!(out, plain);
            assert!(open_payload(&sealed_old, "wrong", kdf_scheme_of(&env_old).unwrap()).is_err());
        }
    }

    fn acct(id: &str, uid: &str, nickname: &str) -> QoderAccount {
        QoderAccount {
            id: id.into(),
            uid: uid.into(),
            nickname: nickname.into(),
            ..Default::default()
        }
    }

    fn profile(machine_id: &str) -> QoderDeviceProfile {
        QoderDeviceProfile {
            machine_id: machine_id.into(),
            ..Default::default()
        }
    }

    const GOOD_ID: &str = "qd-0123456789ab";

    #[test]
    fn merge_new_adds_and_adopts_fields() {
        let mut pool = vec![acct("qd-ffffffffffff", "u-old", "旧账号")];
        let payload = serde_json::json!({
            "id": GOOD_ID, "uid": "u-1", "nickname": "张三", "plan": "pro",
            "group_id": "g1", "note": "备注",
            "device_profile": serde_json::to_value(profile("abc123")).unwrap(),
        });
        let (final_id, is_new) = merge_account(&mut pool, &payload).unwrap();
        assert!(is_new);
        assert_eq!(final_id, GOOD_ID);
        assert_eq!(pool.len(), 2);
        let x = &pool[1];
        assert_eq!(x.uid, "u-1");
        assert_eq!(x.plan, "pro");
        assert_eq!(x.group_id, "g1");
        assert_eq!(
            x.device_profile.as_ref().unwrap().machine_id,
            "abc123",
            "新增账号采用导出指纹"
        );
    }

    #[test]
    fn merge_hit_by_uid_updates_in_place_keeping_original_id() {
        let mut pool = vec![acct("qd-aaaaaaaaaaaa", "u-1", "本地名")];
        let payload = serde_json::json!({
            "id": GOOD_ID, "uid": "u-1", "nickname": "云端名", "plan": "pro+",
        });
        let (final_id, is_new) = merge_account(&mut pool, &payload).unwrap();
        assert!(!is_new);
        assert_eq!(final_id, "qd-aaaaaaaaaaaa", "命中 uid 原位更新保留原 id");
        let x = &pool[0];
        assert_eq!(x.nickname, "云端名");
        assert_eq!(x.plan, "pro+");
        assert_eq!(pool.len(), 1, "不重复入池");
    }

    #[test]
    fn merge_hit_by_id_when_uid_empty() {
        let mut pool = vec![acct("qd-aaaaaaaaaaaa", "", "")];
        let payload = serde_json::json!({ "id": "qd-aaaaaaaaaaaa", "nickname": "云端名" });
        let (final_id, is_new) = merge_account(&mut pool, &payload).unwrap();
        assert!(!is_new);
        assert_eq!(final_id, "qd-aaaaaaaaaaaa");
    }

    #[test]
    fn merge_update_never_overwrites_device_profile() {
        let mut pool = vec![QoderAccount {
            device_profile: Some(profile("local-mid")),
            ..acct("qd-aaaaaaaaaaaa", "u-1", "")
        }];
        let payload = serde_json::json!({
            "id": "qd-aaaaaaaaaaaa", "uid": "u-1",
            "device_profile": serde_json::to_value(profile("cloud-mid")).unwrap(),
        });
        merge_account(&mut pool, &payload).unwrap();
        assert_eq!(
            pool[0].device_profile.as_ref().unwrap().machine_id,
            "local-mid",
            "指纹红线：本地已有档案绝不覆盖"
        );
    }

    #[test]
    fn merge_update_backfills_missing_device_profile() {
        let mut pool = vec![acct("qd-aaaaaaaaaaaa", "u-1", "")];
        let payload = serde_json::json!({
            "id": "qd-aaaaaaaaaaaa", "uid": "u-1",
            "device_profile": serde_json::to_value(profile("cloud-mid")).unwrap(),
        });
        merge_account(&mut pool, &payload).unwrap();
        assert_eq!(
            pool[0].device_profile.as_ref().unwrap().machine_id,
            "cloud-mid",
            "本地为空才补入"
        );
    }

    #[test]
    fn merge_update_keeps_local_group_and_note() {
        let mut pool = vec![QoderAccount {
            group_id: "local-g".into(),
            note: "本地备注".into(),
            ..acct("qd-aaaaaaaaaaaa", "u-1", "")
        }];
        let payload = serde_json::json!({
            "id": "qd-aaaaaaaaaaaa", "uid": "u-1",
            "group_id": "cloud-g", "note": "云端备注",
        });
        merge_account(&mut pool, &payload).unwrap();
        assert_eq!(pool[0].group_id, "local-g", "group_id 仅新增采用");
        assert_eq!(pool[0].note, "本地备注", "note 仅新增采用");
    }

    #[test]
    fn merge_rejects_bad_ids() {
        for (id, why) in [
            ("", "缺少 id"),
            ("qd-ZZ0123456789", "非十六进制"),
            ("qd-0123456789", "长度 10 不是 12"),
            ("wb-0123456789ab", "wb 前缀"),
            ("qd-0123456789ab/../x", "路径注入字符"),
        ] {
            let mut pool = Vec::new();
            let payload = serde_json::json!({ "id": id });
            assert!(merge_account(&mut pool, &payload).is_err(), "应拒绝: {why}");
        }
    }

    // ==================== 加密信封（审查 P1-1）====================

    use base64::Engine as _;

    const KDF_FAST_ITERS: u32 = 1024;

    /// 测试专用快速 KDF：与 kdf_password 同构，仅降低迭代轮数
    /// （单测跑 10 万轮 × 多用例过慢；生产路径轮数由常量控制）
    fn kdf_fast(password: &[u8], salt: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(salt);
        h.update(password);
        let mut acc = h.finalize();
        for _ in 1..KDF_FAST_ITERS {
            let mut h = Sha256::new();
            h.update(acc);
            h.update(salt);
            acc = h.finalize();
        }
        acc.into()
    }

    fn seal_fast(plain: &[u8], password: &str) -> Result<Vec<u8>, String> {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit};
        let mut salt = [0u8; SALT_LEN];
        super::csprng_fill(&mut salt)?;
        let mut nonce = [0u8; NONCE_LEN];
        super::csprng_fill(&mut nonce)?;
        let key = kdf_fast(password.as_bytes(), &salt);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ct = cipher
            .encrypt(aes_gcm::Nonce::from_slice(&nonce), plain)
            .unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(EXPORT_MAGIC);
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn open_fast(sealed: &[u8], password: &str) -> Result<Vec<u8>, String> {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit};
        let head = EXPORT_MAGIC.len() + SALT_LEN + NONCE_LEN;
        if sealed.len() <= head || &sealed[..EXPORT_MAGIC.len()] != EXPORT_MAGIC {
            return Err("文件不是本应用生成的加密导出（缺少 AIWQENC1 魔数头）".into());
        }
        let salt = &sealed[EXPORT_MAGIC.len()..EXPORT_MAGIC.len() + SALT_LEN];
        let nonce = &sealed[EXPORT_MAGIC.len() + SALT_LEN..head];
        let key = kdf_fast(password.as_bytes(), salt);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        cipher
            .decrypt(aes_gcm::Nonce::from_slice(nonce), &sealed[head..])
            .map_err(|_| "解密失败：导出密码错误或文件已损坏".to_string())
    }

    #[test]
    fn envelope_加解密往返() {
        let plain = br#"{"kind":"aiwork-qoder-pool","version":1,"accounts":[]}"#;
        let sealed = seal_fast(plain, "p@ss-口令").expect("seal");
        // 密文不等于明文且不含明文片段（凭证不明文落盘的基本面）
        assert_ne!(&sealed[..], &plain[..]);
        assert!(!sealed.windows(9).any(|w| w == b"aiwork-qo"));
        // 魔数识别：信封以 AIWQENC1 开头
        assert!(sealed.starts_with(EXPORT_MAGIC));
        let round = open_fast(&sealed, "p@ss-口令").expect("open");
        assert_eq!(round, plain.to_vec());
        // envelope_data_of 识别：信封 JSON → Some；明文载荷 → None
        let b64 = base64::engine::general_purpose::STANDARD.encode(&sealed);
        assert!(envelope_data_of(&serde_json::json!({ "data": b64 })).is_some());
        assert!(envelope_data_of(&serde_json::json!({ "kind": "aiwork-qoder-pool" })).is_none());
    }

    #[test]
    fn envelope_错误密码解密失败() {
        let plain = b"secret-payload";
        let sealed = seal_fast(plain, "correct-horse").expect("seal");
        let err = open_fast(&sealed, "wrong-password").expect_err("错误密码应解密失败");
        assert!(err.contains("密码错误"), "应给友好错误: {err}");
        // 篡改密文同样认证失败（GCM 完整性）
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(open_fast(&tampered, "correct-horse").is_err());
    }

    #[test]
    fn envelope_魔数不符拒绝() {
        let mut sealed = seal_fast(b"x", "p").expect("seal");
        sealed[0] = b'X'; // 破坏魔数
        let err = open_fast(&sealed, "p").expect_err("魔数不符应拒绝");
        assert!(err.contains("AIWQENC1"), "应提示缺少魔数头: {err}");
    }

    #[test]
    fn envelope_明文载荷兼容() {
        // 明文导出文件（无 data 字段 / 无魔数）→ envelope_data_of 返回 None → 走原明文路径
        let legacy = serde_json::json!({
            "kind": "aiwork-qoder-pool", "version": 1,
            "include_credentials": false, "accounts": [],
        });
        assert!(envelope_data_of(&legacy).is_none());
        // data 字段存在但非魔数开头（如普通 base64 文本）→ 不误判为信封
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"plain text not sealed");
        assert!(envelope_data_of(&serde_json::json!({ "data": b64 })).is_none());
        // data 非 base64 → None
        assert!(envelope_data_of(&serde_json::json!({ "data": "!!!" })).is_none());
    }
}
