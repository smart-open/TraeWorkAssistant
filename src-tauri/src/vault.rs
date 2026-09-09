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
//!   vault 写失败时降级为明文落盘（保功能可用，仅记录告警）；
//! - Python 签到脚本通过 `write_temp_accounts` 获取解密临时文件（用后即删；
//!   文件落在应用数据目录而非全局 %TEMP%，启动时统一清理残留）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fs_utils;
use crate::models::{AccountsFile, RawAccount};
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

/// 全局 vault 句柄缓存：懒加载；持有锁期间完成读写 + 快照落盘，串行化访问
static VAULT: Mutex<Option<Stronghold>> = Mutex::new(None);

// ---------------- DPAPI（Windows 数据保护 API） ----------------

#[cfg(windows)]
mod dpapi {
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

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("vault 仅支持 Windows".into())
    }
    pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
        Err("vault 仅支持 Windows".into())
    }
}

/// 生成 32 字节随机主密码：
/// 熵源 = 多轮 RandomState（OS 随机种子）+ 高精度时间 + 进程 ID，经 SHA-256 压缩成 256bit
fn generate_password() -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::hash::{BuildHasher, Hasher};
    let mut entropy: Vec<u8> = Vec::new();
    for _ in 0..8 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        entropy.extend(h.finish().to_le_bytes());
    }
    entropy.extend(std::process::id().to_le_bytes());
    let mut hasher = Sha256::new();
    hasher.update(&entropy);
    hasher.finalize().to_vec()
}

/// 读取（或首次生成）DPAPI 保护的主密码
fn vault_password(state: &AppState) -> Result<Vec<u8>, String> {
    let key_path = state.conf_path("vault_key.bin");
    if key_path.exists() {
        let blob = std::fs::read(&key_path).map_err(|e| format!("读取 vault 密钥失败: {e}"))?;
        dpapi::unprotect(&blob)
    } else {
        let pwd = generate_password();
        let blob = dpapi::protect(&pwd)?;
        std::fs::write(&key_path, &blob).map_err(|e| format!("写入 vault 密钥失败: {e}"))?;
        Ok(pwd)
    }
}

/// 打开（并缓存）vault：首次调用时加载快照或创建新 client
fn open(state: &AppState) -> Result<std::sync::MutexGuard<'static, Option<Stronghold>>, String> {
    let mut guard = VAULT.lock().map_err(|_| "vault 锁已被毒化".to_string())?;
    if guard.is_none() {
        let path = state.conf_path("vault.stronghold");
        let password = vault_password(state)?;
        let sh = Stronghold::new(&path, password).map_err(|e| format!("打开 vault 失败: {e}"))?;
        // 快照数据不会自动进入 clients map，必须显式 load_client（见单元测试 stronghold_快照往返）；
        // 仅当快照中不存在该 client（首次创建）时才新建，防止空 client 覆盖已有快照导致凭据丢失
        if sh.load_client(CLIENT_PATH.to_vec()).is_err() {
            sh.create_client(CLIENT_PATH.to_vec())
                .map_err(|e| format!("创建 vault client 失败: {e}"))?;
        }
        *guard = Some(sh);
    }
    Ok(guard)
}

// ---------------- 公共 API ----------------

/// 从磁盘加载账号文件，并从 vault 回填占位账号的明文凭据（仅内存，不落明文盘）。
/// JSON 中已有的明文凭据优先（更新鲜，例如 MITM 新捕获，待下次保存迁移进 vault）。
pub fn load_accounts(state: &AppState) -> AccountsFile {
    let mut file: AccountsFile = fs_utils::read_json(&state.path("checkin_accounts.json"));
    let Ok(guard) = open(state) else {
        return file; // vault 不可用：降级返回 JSON 原样（占位 jwt 视为空，上层自行报错）
    };
    let Some(sh) = guard.as_ref() else {
        return file;
    };
    let Ok(client) = sh.get_client(CLIENT_PATH.to_vec()) else {
        return file;
    };
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
    file
}

/// 保存账号文件：非空凭据写入 vault（字段级合并）并落盘快照，JSON 占位化。
/// vault 写失败时降级为明文落盘（保证功能不中断，记录告警日志）。
/// 注意：成功路径会就地清空调用方结构体中的 jwt / refresh_token 字段。
pub fn save_accounts(state: &AppState, accounts: &mut AccountsFile) -> Result<(), String> {
    let vault_result = write_vault_secrets(state, accounts);
    if vault_result.is_ok() {
        wipe_placeholders(accounts);
    } else {
        let reason = vault_result.as_ref().err().cloned().unwrap_or_default();
        fs_utils::app_log(
            &state.data_dir,
            &format!("vault 写入失败，凭据保留明文落盘（下次启动重试迁移）: {reason}"),
        );
    }
    fs_utils::write_json(&state.path("checkin_accounts.json"), accounts)?;
    vault_result
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
    let guard = open(state)?;
    let Some(sh) = guard.as_ref() else {
        return Err("vault 未初始化".into());
    };
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
    let result = (|| -> Result<(), String> {
        let guard = open(state)?;
        let Some(sh) = guard.as_ref() else {
            return Err("vault 未初始化".into());
        };
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        client
            .store()
            .delete(uid.as_bytes())
            .map_err(|e| format!("vault 删除失败: {e}"))?;
        sh.save().map_err(|e| format!("vault 快照落盘失败: {e}"))
    })();
    match result {
        Ok(()) => fs_utils::app_log(&state.data_dir, &format!("vault: 已删除账号 {uid} 的凭据记录")),
        Err(e) => fs_utils::app_log(
            &state.data_dir,
            &format!("vault: 删除账号 {uid} 凭据失败（残留记录无碍安全）: {e}"),
        ),
    }
}

/// 启动时幂等迁移：JSON 中的明文 jwt / refresh_token → vault，随后 JSON 占位化。
/// 失败不阻断启动（下次启动重试；vault 异常时 save_accounts 会降级保留明文）。
pub fn migrate_on_startup(state: &AppState) {
    let raw: AccountsFile = fs_utils::read_json(&state.path("checkin_accounts.json"));
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
            &format!("启动迁移: 写入 vault 失败（保留明文，下次启动重试）: {e}"),
        ),
    }
}

/// 为 Python 签到脚本生成解密临时账号文件（仅含候选账号），返回路径；调用方用后必须删除。
/// 文件写入应用数据目录（而非全局 %TEMP%），避免明文凭据散落系统临时区；残留由启动清理兜底。
pub fn write_temp_accounts(state: &AppState, uids: &[String]) -> Result<PathBuf, String> {
    let accounts = load_accounts(state);
    let set: std::collections::HashSet<&str> = uids.iter().map(|s| s.as_str()).collect();
    let filtered: Vec<RawAccount> = accounts
        .accounts
        .into_iter()
        .filter(|a| a.user_id.as_deref().map_or(false, |u| set.contains(u)))
        .collect();
    if filtered.is_empty() {
        return Err("候选账号均无可用凭据".into());
    }
    let path = state.data_dir.join(format!(
        "{}{}.json",
        TEMP_ACCOUNTS_PREFIX,
        chrono::Local::now().timestamp_millis()
    ));
    fs_utils::write_json(&path, &AccountsFile { accounts: filtered })?;
    Ok(path)
}

/// 清理目录下残留的临时凭据文件（按前缀匹配，覆盖 write_json 的 .tmp 半成品），返回删除数量
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
