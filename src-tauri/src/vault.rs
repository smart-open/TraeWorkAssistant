//! 敏感数据加密存储（Tauri Stronghold）。
//!
//! 设计：
//! - vault 快照 `conf/vault.stronghold` 权威保存各账号 jwt / refresh_token（按 uid 一条记录）；
//! - vault 主密码为随机 32 字节，经 Windows DPAPI 加密存放在 `conf/vault_key.bin`
//!   （仅本机当前用户可解，拷贝到其他机器/用户无法解密）；
//! - `checkin_accounts.json` 只保留占位（jwt/refresh_token 清空），其余字段（name/user_id 等）不动；
//! - 所有账号文件读写统一走 `load_accounts` / `save_accounts`：
//!   读：JSON 明文优先（更新鲜，如 MITM 新捕获）→ 否则从 vault 回填；
//!   写：非空凭据先写入 vault 并落盘快照 → JSON 占位化；
//!   vault 写失败时仅保存占位化 JSON 并返回 Err（禁止明文 jwt/refresh_token 落盘）；
//! - Rust 签到直调后凭据全程内存传递（原 Python 脚本方案需写解密临时文件，已移除；
//!   启动清理逻辑保留，兜底清理旧版本残留的临时凭据文件）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fs_utils;
use crate::models::AccountsFile;
use crate::state::AppState;
use tauri_plugin_stronghold::stronghold::Stronghold;

/// vault 单客户端路径（固定使用一个 client，简化存取）
const CLIENT_PATH: &[u8] = b"main";

/// 临时凭据文件名前缀（位于应用数据目录；启动时按此前缀清理残留）
const TEMP_ACCOUNTS_PREFIX: &str = "trae_checkin_accounts_";

/// 单账号凭据记录（vault 内 JSON 序列化存储）
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug, PartialEq)]
pub struct SecretEntry {
    #[serde(default)]
    pub jwt: String,
    #[serde(default)]
    pub refresh_token: String,
}

/// 全局 vault 句柄缓存：按 data_dir 分条懒加载（生产单 data_dir 只有一条缓存，
/// 与旧「全局单例」行为一致；分条隔离让多 data_dir 场景——如单元测试各用临时目录——
/// 互不串库）。持有锁期间完成读写 + 快照落盘，串行化访问
static VAULTS: std::sync::LazyLock<Mutex<HashMap<PathBuf, Option<Stronghold>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------- DPAPI（Windows 数据保护 API） ----------------

#[cfg(windows)]
pub(crate) mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// DPAPI 加密（当前用户维度，CryptProtectData）
    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: plain.len() as u32,
                pbData: plain.as_ptr() as *mut u8,
            };
            let mut out = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            let ok = CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            );
            if ok == 0 {
                return Err("CryptProtectData 调用失败".into());
            }
            let data = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            Ok(data)
        }
    }

    /// DPAPI 解密（仅本机当前用户可解）
    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: blob.len() as u32,
                pbData: blob.as_ptr() as *mut u8,
            };
            let mut out = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            let ok = CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            );
            if ok == 0 {
                return Err("CryptUnprotectData 调用失败（数据可能来自其他用户/机器）".into());
            }
            let data = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            Ok(data)
        }
    }
}

// mac 构建下 protect/unprotect 无消费点（Qoder 域 mac 实装时启用，见下注）
#[cfg(not(windows))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod dpapi {
    // macOS 适配预留（2026-10-03）：Qoder 域直接消费点为 ide_store.rs 的
    // Chromium os_crypt 密钥解包（Local State encrypted_key "DPAPI" 前缀段）。
    // macOS 等价物 = Keychain generic password（service≈"Chromium Safe Storage"，
    // 需真机实测 Qoder 实际条目名），取回 16B 密钥后 AES-GCM 逻辑不变。
    // 建议：以同签名 unprotect 语义做一个 mac 模块供 ide_store 按 cfg 选择，
    // 或在 ide_store 内直接分支——本占位仅服务 vault 自身（stronghold 主密码
    // 不依赖 DPAPI，见 generate_password，macOS 走 stronghold 原生即可）
    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("vault 仅支持 Windows".into())
    }
    pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
        Err("vault 仅支持 Windows".into())
    }
}

/// OS CSPRNG 填充（Windows=BCryptGenRandom 进程首选 RNG；mac/其他平台=uuid v4
/// 底层 getrandom CSPRNG，F-75 M0-0.4——macos_main 合并适配：main 预留的 Err 占位
/// 由 mac 实装替换）。失败直接返回 Err。
/// 审查 P2-4：**不允许任何弱熵回退路径**——CSPRNG 不可用属系统级故障，
/// 静默降级到可预测熵（时间/PID 哈希）等于密钥材料可被离线爆破，必须 fail-fast。
/// 供 vault 主密码生成与 Qoder 加密导出（salt/nonce）共用。
pub(crate) fn csprng_fill(dest: &mut [u8]) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        // 算法句柄传零值（等效 NULL）配合 BCRYPT_USE_SYSTEM_PREFERRED_RNG 使用进程首选 RNG
        let halg: windows_sys::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE =
            unsafe { std::mem::zeroed() };
        // STATUS_SUCCESS == 0
        let status = unsafe {
            BCryptGenRandom(halg, dest.as_mut_ptr(), dest.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status != 0 {
            return Err(format!(
                "OS CSPRNG 不可用（BCryptGenRandom 失败 status=0x{status:08X}）：拒绝生成密钥材料"
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        // mac/其他平台：uuid v4（getrandom 熵源）逐 16 字节拼接填满（CSPRNG 恒成功，
        // Err 通道仅为签名兼容与未来平台扩展保留）
        let mut filled = 0usize;
        while filled < dest.len() {
            let bytes = uuid::Uuid::new_v4().into_bytes();
            let n = (dest.len() - filled).min(bytes.len());
            dest[filled..filled + n].copy_from_slice(&bytes[..n]);
            filled += n;
        }
        Ok(())
    }
}
/// 生成 32 字节随机主密码：熵源 = OS CSPRNG（经 [csprng_fill]，Windows=BCrypt /
/// mac=uuid v4 getrandom）。
/// 审查 P2-4：失败直接返回 Err 终止 vault 初始化（上层报错），删除旧实现的
/// 「RandomState + 高精度时间 + 进程 ID 经 SHA-256 压缩」弱熵兜底路径。
fn generate_password() -> Result<Vec<u8>, String> {
    let mut buf = [0u8; 32];
    csprng_fill(&mut buf)?;
    Ok(buf.to_vec())
}

/// 读取（或首次生成）受保护的主密码（per-data_dir：主密码落 `<data_dir>/conf/`）。
/// 平台分派收口 `platform::secret`（F-75 M0-0.4）——
/// Windows 走 DPAPI + `conf/vault_key.bin`（main 行为零变化，错误文案逐字保留）；
/// macOS 走 key file 主源 + Keychain 兜底（首启无条目 → 生成随机主密码写入，用户无感）。
/// 防御（macos_main 实测缺陷修复，全平台生效）：主密码源缺失但 vault 快照已存在时，
/// 静默重生成新密码会与既有密文永久失配（macOS Keychain 条目丢失 → 每次 save_accounts
/// 报 BadFileKey，凭据不可恢复且读路径静默降级为空 JWT）。此时必须显式报错，
/// 由用户决定重置 vault 或恢复密码源，绝不静默换钥。
fn vault_password_at(data_dir: &Path) -> Result<Vec<u8>, String> {
    if let Some(pwd) = crate::platform::secret::load_vault_password_at(data_dir)? {
        return Ok(pwd);
    }
    let snapshot = data_dir.join("conf").join("vault.stronghold");
    if snapshot.exists() {
        return Err(format!(
            "vault 主密码源缺失但快照已存在（{}），拒绝静默重生成密码；\
             若快照内凭据已全部失效，可备份后删除该文件重置 vault",
            snapshot.display()
        ));
    }
    let pwd = generate_password()?;
    crate::platform::secret::store_vault_password_at(data_dir, &pwd)?;
    Ok(pwd)
}

/// 打开（并缓存）data_dir 对应的 vault，然后在持锁状态下执行 `f(sh)`。
/// 首次调用时加载快照或创建新 client；主密码/快照打开失败保留 None 条目待下次重试。
fn with_open<T>(
    data_dir: &Path,
    f: impl FnOnce(&Stronghold) -> Result<T, String>,
) -> Result<T, String> {
    // 锁中毒恢复：另一线程在持锁期间 panic 毒化锁时，直接恢复内部数据继续使用，
    // 而不是让「vault 锁已被毒化」错误在所有后续调用上永久传播
    let mut guard = VAULTS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.entry(data_dir.to_path_buf()).or_insert(None);
    if entry.is_none() {
        let conf = data_dir.join("conf");
        let _ = std::fs::create_dir_all(&conf);
        let path = conf.join("vault.stronghold");
        let password = vault_password_at(data_dir)?;
        let sh = Stronghold::new(&path, password).map_err(|e| format!("打开 vault 失败: {e}"))?;
        // 快照数据不会自动进入 clients map，必须显式 load_client（见单元测试 stronghold_快照往返）；
        // 仅当快照中不存在该 client（首次创建）时才新建，防止空 client 覆盖已有快照导致凭据丢失
        if sh.load_client(CLIENT_PATH.to_vec()).is_err() {
            sh.create_client(CLIENT_PATH.to_vec())
                .map_err(|e| format!("创建 vault client 失败: {e}"))?;
        }
        *entry = Some(sh);
    }
    f(entry.as_ref().expect("with_open: 条目刚初始化必有句柄"))
}

// ---------------- 通用命名空间凭证（WB / Qoder / 豆包等非 Trae 家族） ----------------
//
// 设计（审查 P0-1 凭证收敛）：Trae 家族凭据长期走 Stronghold + DPAPI，而 wb_tokens /
// qoder_tokens / doubao_accounts 表为 SQLite 明文行——本节提供按 (namespace, key)
// 寻址的通用加密存储，供上述家族把 token / sessionid 等敏感字段收敛进同一 vault。
// key 编码 `ns:<ns>:<key>`，与 Trae 账号裸 uid key 天然隔离（uid 不含冒号前缀）。

fn ns_vault_key(ns: &str, key: &str) -> Vec<u8> {
    format!("ns:{ns}:{key}").into_bytes()
}

/// 读取命名空间凭证（vault 不可用 / 记录缺失 / 解析失败均返回 None，fail-secure）。
/// 系统性故障（DPAPI/快照/锁）与「无凭证」在返回值上不可区分——错误先落日志
/// 留排查线索再回退 None（审查修复：读取侧原完全静默，vault 故障表现为
/// 「全体掉线」且无线索）
pub fn ns_get(data_dir: &Path, ns: &str, key: &str) -> Option<serde_json::Value> {
    if ns.is_empty() || key.is_empty() {
        return None;
    }
    with_open(data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        match client.store().get(&ns_vault_key(ns, key)).map_err(|e| format!("vault 读取失败: {e}"))? {
            Some(v) => {
                serde_json::from_slice(&v).map(Some).map_err(|e| format!("凭证解析失败: {e}"))
            }
            None => Ok(None),
        }
    })
    .inspect_err(|e| {
        crate::fs_utils::app_log(data_dir, &format!("[vault] ns_get {ns}:{key} 失败: {e}"));
    })
    .ok()
    .flatten()
}

/// 写入命名空间凭证 + 快照落盘。失败返回 Err（调用方对齐 Trae 红线：
/// 禁止在 vault 写失败时把明文落库，应只落占位并报错）。
pub fn ns_set(data_dir: &Path, ns: &str, key: &str, v: &serde_json::Value) -> Result<(), String> {
    if ns.is_empty() || key.is_empty() {
        return Err("命名空间凭证 key 为空".into());
    }
    with_open(data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        let value = serde_json::to_vec(v).map_err(|e| format!("凭证序列化失败: {e}"))?;
        client
            .store()
            .insert(ns_vault_key(ns, key), value, None)
            .map_err(|e| format!("vault 写入失败: {e}"))?;
        sh.save().map_err(|e| format!("vault 快照落盘失败: {e}"))
    })
}

/// 删除命名空间凭证（失败仅日志，不阻断上层删除流程——残留加密记录无碍安全）
pub fn ns_remove(data_dir: &Path, ns: &str, key: &str) {
    let result = with_open(data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        client
            .store()
            .delete(&ns_vault_key(ns, key))
            .map_err(|e| format!("vault 删除失败: {e}"))?;
        sh.save().map_err(|e| format!("vault 快照落盘失败: {e}"))
    });
    if let Err(e) = result {
        fs_utils::app_log(data_dir, &format!("vault: 删除 {ns}:{key} 凭证失败（残留加密记录无碍安全）: {e}"));
    }
}

/// 豆包代理抓包凭证快照读取（审查 P2 收敛配套）：优先 vault ns `doubao_captured`
/// （handler.rs 写入点已收敛至此），未命中回退旧 kv `doubao_captured_credentials`
/// 一次——兼容「应用升级后尚未重启、迁移未执行」场景下读取方仍能拿到快照；
/// 两处皆无返回 None。历史明文 kv 由 [migrate_ns_on_startup] ④ 迁入 vault 后清空。
/// 消费方：commands/doubao.rs（read_captured_uid / doubao_captured_credential /
/// doubao_credential_auto_apply）与 tasks/doubao_chats.rs（detect_uid）共 4 处。
pub fn doubao_captured_get(data_dir: &Path) -> Option<serde_json::Value> {
    if let Some(v) = ns_get(data_dir, "doubao_captured", "credentials") {
        return Some(v);
    }
    let v: serde_json::Value = crate::store::db(data_dir).kv_get("doubao_captured_credentials");
    if v.is_null() {
        None
    } else {
        Some(v)
    }
}

// ---------------- 公共 API ----------------

/// vault 打开失败的去重日志标记：读路径调用极频繁，仅首条降级写日志，
/// 恢复成功后复位（避免诊断盲区——本机曾因该静默降级整晚 401 无从定位）
static VAULT_DEGRADED_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 从磁盘加载账号文件，并从 vault 回填占位账号的明文凭据（仅内存，不落明文盘）。
/// JSON 中已有的明文凭据优先（更新鲜，例如 MITM 新捕获，待下次保存迁移进 vault）。
pub fn load_accounts(state: &AppState) -> AccountsFile {
    // SQLite 化（P3）：checkin_accounts.json → accounts 表（行保序、user_id 可空）
    let mut file: AccountsFile = crate::store::docs::accounts_load(&crate::store::db(&state.data_dir));
    // vault 不可用：降级返回 JSON 原样（占位 jwt 视为空，上层自行报错）；
    // 降级去重日志（macos_main 保留项）：读路径调用极频繁，仅首条落日志，
    // 恢复成功后复位（避免诊断盲区——本机曾因该静默降级整晚 401 无从定位）
    match with_open(&state.data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        for a in file.accounts.iter_mut() {
            let Some(uid) = a.user_id.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            if !a.jwt.trim().is_empty() {
                continue; // JSON 明文优先
            }
            if let Ok(Some(v)) = client.store().get(uid.as_bytes()) {
                if let Ok(entry) = serde_json::from_slice::<SecretEntry>(&v) {
                    if !entry.jwt.is_empty() {
                        a.jwt = entry.jwt;
                    }
                    if a.refresh_token.as_deref().map_or(true, |s| s.is_empty())
                        && !entry.refresh_token.is_empty()
                    {
                        a.refresh_token = Some(entry.refresh_token);
                    }
                }
            }
        }
        Ok(())
    }) {
        Ok(()) => {
            use std::sync::atomic::Ordering;
            VAULT_DEGRADED_LOGGED.store(false, Ordering::Relaxed);
        }
        Err(reason) => {
            use std::sync::atomic::Ordering;
            if !VAULT_DEGRADED_LOGGED.swap(true, Ordering::Relaxed) {
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("vault 不可用：账号凭据读取降级为空 JWT（后续同类错误不再重复记录）: {reason}"),
                );
            }
        }
    }
    file
}

/// 保存账号文件：非空凭据写入 vault（字段级合并）并落盘快照，JSON 占位化。
/// vault 写失败时**禁止明文落盘**（审查 P1）：仅保存占位化 JSON（凭据字段清空），
/// 返回 Err 明确告知「加密存储失败，签到功能不可用直至修复」——宁可丢本次凭据更新，
/// 也不把 jwt/refresh_token 明文写到磁盘。
/// 注意：成功与降级路径都会就地清空调用方结构体中的 jwt / refresh_token 字段。
pub fn save_accounts(state: &AppState, accounts: &mut AccountsFile) -> Result<(), String> {
    let vault_result = write_vault_secrets(state, accounts);
    // 无论 vault 写入成败，落盘前一律占位化：vault 失败时严禁明文凭据进入 JSON
    wipe_placeholders(accounts);
    if let Err(reason) = &vault_result {
        fs_utils::app_log(
            &state.data_dir,
            &format!("vault 写入失败，已仅保存账号占位信息（禁止明文落盘）: {reason}"),
        );
    }
    crate::store::docs::accounts_save(&crate::store::db(&state.data_dir), accounts)?;
    vault_result.map_err(|reason| {
        format!(
            "加密存储失败，已仅保存账号占位信息，签到功能不可用直至修复（vault 错误: {reason}）"
        )
    })
}

/// 将账号结构体中的明文凭据占位化（清空 jwt / refresh_token）
fn wipe_placeholders(accounts: &mut AccountsFile) {
    for a in accounts.accounts.iter_mut() {
        if a.user_id.as_deref().map_or(false, |u| !u.is_empty()) {
            a.jwt.clear();
            a.refresh_token = None;
        }
    }
}

/// 把非空凭据写入 vault：读旧值做字段级合并（防止空字段覆盖 vault 中的有效值），最后快照落盘
fn write_vault_secrets(state: &AppState, accounts: &AccountsFile) -> Result<(), String> {
    with_open(&state.data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        let mut count = 0usize;
        for a in &accounts.accounts {
            let Some(uid) = a.user_id.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            let jwt_new = a.jwt.trim();
            let rt_new = a.refresh_token.as_deref().unwrap_or("").trim();
            if jwt_new.is_empty() && rt_new.is_empty() {
                continue; // 占位账号无凭据可写
            }
            let entry = merge_entry(
                client
                    .store()
                    .get(uid.as_bytes())
                    .ok()
                    .flatten()
                    .and_then(|v| serde_json::from_slice::<SecretEntry>(&v).ok()),
                jwt_new,
                rt_new,
            );
            let value = serde_json::to_vec(&entry).map_err(|e| format!("凭据序列化失败: {e}"))?;
            client
                .store()
                .insert(uid.as_bytes().to_vec(), value, None)
                .map_err(|e| format!("vault 写入失败: {e}"))?;
            count += 1;
        }
        sh.save().map_err(|e| format!("vault 快照落盘失败: {e}"))?;
        if count > 0 {
            fs_utils::app_log(
                &state.data_dir,
                &format!("vault: 已加密写入 {count} 个账号凭据"),
            );
        }
        Ok(())
    })
}

/// 字段级合并：新值非空才覆盖（纯函数，便于单测）
fn merge_entry(existing: Option<SecretEntry>, jwt_new: &str, rt_new: &str) -> SecretEntry {
    let mut entry = existing.unwrap_or_default();
    if !jwt_new.is_empty() {
        entry.jwt = jwt_new.to_string();
    }
    if !rt_new.is_empty() {
        entry.refresh_token = rt_new.to_string();
    }
    entry
}

/// 删除账号时同步清理 vault 记录（失败仅记录，不阻断账号删除）
pub fn remove_secret(state: &AppState, uid: &str) {
    let result = with_open(&state.data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        client
            .store()
            .delete(uid.as_bytes())
            .map_err(|e| format!("vault 删除失败: {e}"))?;
        sh.save().map_err(|e| format!("vault 快照落盘失败: {e}"))
    });
    match result {
        Ok(()) => fs_utils::app_log(&state.data_dir, &format!("vault: 已删除账号 {uid} 的凭据记录")),
        Err(e) => fs_utils::app_log(
            &state.data_dir,
            &format!("vault: 删除账号 {uid} 凭据失败（残留记录无碍安全）: {e}"),
        ),
    }
}

/// 启动时幂等迁移：库中明文 jwt / refresh_token → vault，随后占位化。
/// （SQLite 化 P4：原读 checkin_accounts.json，现读 accounts 表——main.rs 已调整为
/// store 迁移先于本函数，JSON 导入的明文凭据在此收敛进 vault 并从库中抹除。）
/// 失败不阻断启动（下次启动重试；vault 异常时 save_accounts 仅落盘占位信息，禁止明文）。
pub fn migrate_on_startup(state: &AppState) {
    let raw: AccountsFile = crate::store::docs::accounts_load(&crate::store::db(&state.data_dir));
    let plaintext = raw
        .accounts
        .iter()
        .filter(|a| {
            !a.jwt.trim().is_empty() || a.refresh_token.as_deref().map_or(false, |s| !s.is_empty())
        })
        .count();
    if plaintext == 0 {
        return; // 无明文凭据，幂等退出
    }
    let mut accounts = load_accounts(state);
    match save_accounts(state, &mut accounts) {
        Ok(()) => fs_utils::app_log(
            &state.data_dir,
            &format!("启动迁移: 已将 {plaintext} 个账号的明文凭据加密写入 vault"),
        ),
        Err(e) => fs_utils::app_log(
            &state.data_dir,
            &format!("启动迁移: 写入 vault 失败（已仅保留占位信息，禁止明文落盘）: {e}"),
        ),
    }
}

/// 启动时幂等迁移（审查 P0-1 凭证收敛）：WB / Qoder token store 与豆包账号池中的
/// 明文凭证（access_token / refresh_token / pat / machine_token / sessionid / sid_guard /
/// ttwid）→ vault（Stronghold + DPAPI），SQLite 行占位化。失败不阻断启动（下次启动重试；
/// 运行时读写路径已全部 secure 化，明文只会在 JSON→SQLite 一次性迁移后短暂存在）。
pub fn migrate_ns_on_startup(state: &AppState) {
    // ① WB token store（wb_tokens 表）
    {
        let store = crate::store::docs::wb_token_store_load(&crate::store::db(&state.data_dir));
        let plain: Vec<(String, serde_json::Value)> = store
            .get("tokens")
            .and_then(|t| t.as_object())
            .map(|tokens| {
                tokens
                    .iter()
                    .filter(|(_, rec)| {
                        ["access_token", "accessToken", "refresh_token", "refreshToken"]
                            .iter()
                            .any(|k| {
                                rec.get(k)
                                    .and_then(|v| v.as_str())
                                    .map_or(false, |s| !s.is_empty())
                            })
                    })
                    .map(|(id, rec)| (id.clone(), rec.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if !plain.is_empty() {
            let mut ok = 0usize;
            for (id, rec) in &plain {
                match crate::tasks::wb_common::token_store_upsert_secure(&state.data_dir, id, rec)
                {
                    Ok(()) => ok += 1,
                    Err(e) => fs_utils::app_log(
                        &state.data_dir,
                        &format!(
                            "启动迁移: WB 账号 {id} 凭证入 vault 失败（已落占位，禁止明文）: {e}"
                        ),
                    ),
                }
            }
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "启动迁移: 已将 {ok}/{} 个 WB 账号的明文凭证加密写入 vault",
                    plain.len()
                ),
            );
        }
    }
    // ② Qoder token store（qoder_tokens 表）
    match crate::tasks::qoder_common::migrate_token_store(state) {
        Ok(n) if n > 0 => fs_utils::app_log(
            &state.data_dir,
            &format!("启动迁移: 已将 {n} 个 Qoder 账号的明文凭证加密写入 vault"),
        ),
        Ok(_) => {}
        Err(e) => fs_utils::app_log(
            &state.data_dir,
            &format!("启动迁移: Qoder 凭证入 vault 失败（已落占位，禁止明文）: {e}"),
        ),
    }
    // ③ 豆包账号池（doubao_accounts 表）
    {
        let pool = crate::store::docs::doubao_pool_load(&crate::store::db(&state.data_dir));
        let plain = pool
            .accounts
            .iter()
            .filter(|a| {
                [a.session_id.as_deref(), a.sid_guard.as_deref(), a.ttwid.as_deref()]
                    .iter()
                    .any(|v| v.map_or(false, |s| !s.is_empty()))
            })
            .count();
        if plain > 0 {
            match crate::commands::doubao::save_pool(state, &pool) {
                Ok(()) => fs_utils::app_log(
                    &state.data_dir,
                    &format!("启动迁移: 已将 {plain} 个豆包账号的明文凭证加密写入 vault"),
                ),
                Err(e) => fs_utils::app_log(
                    &state.data_dir,
                    &format!("启动迁移: 豆包凭证入 vault 失败（已落占位，禁止明文）: {e}"),
                ),
            }
        }
    }
    // ④ 豆包代理抓包凭证（审查 P2：kv `doubao_captured_credentials` 为明文 JSON
    //    快照，含 session_id/sid_guard/ttwid 会话凭证，违反「凭证只进 vault」红线
    //    → 迁入 vault ns `doubao_captured` 后清空该 kv）。幂等：kv 缺失/空直接跳过；
    //    vault 写成功后才删 kv（失败保留，下次启动重试）；读取侧统一走
    //    [doubao_captured_get]（vault 优先 → 旧 kv 回退一次，兼容未重启场景）。
    {
        const KV_KEY: &str = "doubao_captured_credentials";
        let v: serde_json::Value = crate::store::db(&state.data_dir).kv_get(KV_KEY);
        if v.is_null() {
            // 无该行（首次升级/已迁移）：幂等退出
        } else if v.get("session_id").and_then(|s| s.as_str()).map_or(false, |s| !s.is_empty()) {
            match ns_set(&state.data_dir, "doubao_captured", "credentials", &v) {
                Ok(()) => match crate::store::db(&state.data_dir).kv_delete(KV_KEY) {
                    Ok(()) => fs_utils::app_log(
                        &state.data_dir,
                        "启动迁移: 豆包抓包凭证快照已收敛进 vault 并清空明文 kv",
                    ),
                    Err(e) => fs_utils::app_log(
                        &state.data_dir,
                        &format!(
                            "启动迁移: 清空豆包抓包凭证明文 kv 失败（vault 已有加密副本，下次启动重试）: {e}"
                        ),
                    ),
                },
                Err(e) => fs_utils::app_log(
                    &state.data_dir,
                    &format!("启动迁移: 豆包抓包凭证入 vault 失败（保留明文 kv 下次重试，禁止丢凭证）: {e}"),
                ),
            }
        } else {
            // 残留行但无凭证内容（空对象/损坏数据）：直接清理，避免永远空转
            let _ = crate::store::db(&state.data_dir).kv_delete(KV_KEY);
        }
    }
}

/// 清理目录下残留的临时凭据文件（按前缀匹配，覆盖 write_json 的 .tmp 半成品），返回数量
fn cleanup_temp_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0usize;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with(TEMP_ACCOUNTS_PREFIX) {
            if std::fs::remove_file(entry.path()).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// 启动时清理残留的临时凭据文件（进程崩溃/被杀时未及删除的明文文件）
pub fn cleanup_temp_accounts(state: &AppState) {
    let n = cleanup_temp_in(&state.data_dir);
    if n > 0 {
        fs_utils::app_log(
            &state.data_dir,
            &format!("vault: 已清理 {n} 个残留临时凭据文件"),
        );
    }
}

// ---------------- 单元测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RawAccount;

    #[test]
    fn merge_entry_覆盖非空字段并保留旧值() {
        let old = SecretEntry {
            jwt: "old-jwt".into(),
            refresh_token: "old-rt".into(),
        };
        // 新 jwt 非空、rt 为空 → 只覆盖 jwt
        let merged = merge_entry(Some(old.clone()), "new-jwt", "");
        assert_eq!(merged.jwt, "new-jwt");
        assert_eq!(merged.refresh_token, "old-rt");
        // 全空 → 不变
        let merged = merge_entry(Some(old), "", "");
        assert_eq!(merged.jwt, "old-jwt");
        assert_eq!(merged.refresh_token, "old-rt");
        // 无旧值 → 取新值
        let merged = merge_entry(None, "j", "r");
        assert_eq!(
            merged,
            SecretEntry {
                jwt: "j".into(),
                refresh_token: "r".into()
            }
        );
    }

    #[test]
    fn wipe_placeholders_仅清空凭据保留元数据() {
        let mut file = AccountsFile {
            accounts: vec![
                RawAccount {
                    name: "A".into(),
                    user_id: Some("10001".into()),
                    jwt: "secret-jwt".into(),
                    refresh_token: Some("secret-rt".into()),
                    added_at: Some("t".into()),
                    updated_at: Some("t".into()),
                    dc_id: None,
                    refresh_token_expires_at: None,
                    refresh_token_fails: 0,
                    refresh_token_invalid: false,
                    auth_saved_at: None,
                },
                // 无 uid 的账号不占位（vault 无法按 uid 键存储）
                RawAccount {
                    name: "B".into(),
                    user_id: None,
                    jwt: "keep-jwt".into(),
                    refresh_token: None,
                    added_at: None,
                    updated_at: None,
                    dc_id: None,
                    refresh_token_expires_at: None,
                    refresh_token_fails: 0,
                    refresh_token_invalid: false,
                    auth_saved_at: None,
                },
            ],
        };
        wipe_placeholders(&mut file);
        assert_eq!(file.accounts[0].jwt, "");
        assert_eq!(file.accounts[0].refresh_token, None);
        assert_eq!(file.accounts[0].name, "A");
        assert_eq!(file.accounts[0].user_id, Some("10001".into()));
        assert_eq!(file.accounts[0].added_at, Some("t".into()));
        // 无 uid 账号保持原样
        assert_eq!(file.accounts[1].jwt, "keep-jwt");
    }

    #[test]
    fn cleanup_temp_in_仅删前缀文件() {
        let dir = std::env::temp_dir().join(format!("vault_cleanup_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("trae_checkin_accounts_1.json"), "{}").unwrap();
        std::fs::write(dir.join("trae_checkin_accounts_2.tmp"), "{}").unwrap();
        std::fs::write(dir.join("other.json"), "{}").unwrap();
        std::fs::create_dir(dir.join("trae_checkin_accounts_dir")).unwrap();
        let n = cleanup_temp_in(&dir);
        assert_eq!(n, 2);
        assert!(dir.join("other.json").exists());
        assert!(dir.join("trae_checkin_accounts_dir").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_加解密往返() {
        let plain = b"trae-vault-test-1234567890abcdef";
        let blob = dpapi::protect(plain).expect("protect");
        assert_ne!(blob, plain.to_vec());
        let round = dpapi::unprotect(&blob).expect("unprotect");
        assert_eq!(round, plain.to_vec());
    }

    #[cfg(windows)]
    #[test]
    fn stronghold_快照往返() {
        // 验证插件 Rust API 用法：创建 client → store 写入 → save → 重新加载读取
        let dir = std::env::temp_dir().join(format!("vault_rs_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.stronghold");
        // KeyProvider 要求主密码恰好 32 字节（NC_DATA_SIZE），与生产 generate_password 输出一致
        let password = vec![7u8; 32];
        {
            let sh = Stronghold::new(&path, password.clone()).expect("open new");
            if sh.get_client(CLIENT_PATH.to_vec()).is_err() {
                sh.create_client(CLIENT_PATH.to_vec()).expect("create client");
            }
            let entry = SecretEntry {
                jwt: "jwt-value".into(),
                refresh_token: "rt-value".into(),
            };
            let value = serde_json::to_vec(&entry).unwrap();
            let client = sh.get_client(CLIENT_PATH.to_vec()).unwrap();
            client
                .store()
                .insert("10001".as_bytes().to_vec(), value, None)
                .expect("insert");
            sh.save().expect("save");
        }
        {
            let sh = Stronghold::new(&path, password).expect("reopen");
            // 快照数据不会自动进入 clients map，必须显式 load_client
            sh.load_client(CLIENT_PATH.to_vec()).expect("load client");
            let client = sh.get_client(CLIENT_PATH.to_vec()).unwrap();
            let v = client.store().get("10001".as_bytes()).expect("get");
            let entry: SecretEntry = serde_json::from_slice(&v.expect("record exists")).unwrap();
            assert_eq!(entry.jwt, "jwt-value");
            assert_eq!(entry.refresh_token, "rt-value");
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
