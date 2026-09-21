//! 敏感数据加密存储（Stronghold，Web/无桌面部署适配）。
//!
//! 设计：
//! - vault 快照 `conf/vault.stronghold` 权威保存各账号 jwt / refresh_token（按 uid 一条记录）；
//! - vault 主密码（KeyProvider，ADR-2 去 DPAPI）：
//!   1. 环境变量 `AIWORK_VAULT_KEY` 非空 → 任意字符串 SHA-256 归一为 32 字节（Docker/跨机部署可复现）；
//!   2. 否则读 `conf/vault_key.bin`（"V2:"+hex(32B) 格式，首启自动生成，unix 下 0600 权限）；
//!   3. 检测到旧桌面版 DPAPI 加密 blob → 报明确迁移错误（见 parse_key_file）；
//! - `checkin_accounts.json` 只保留占位（jwt/refresh_token 清空），其余字段（name/user_id 等）不动；
//! - 所有账号文件读写统一走 `load_accounts` / `save_accounts`：
//!   读：JSON 明文优先（更新鲜，如 MITM 新捕获）→ 否则从 vault 回填；
//!   写：非空凭据先写入 vault 并落盘快照 → JSON 占位化；
//!   vault 写失败时仅保存占位化 JSON 并返回 Err（禁止明文 jwt/refresh_token 落盘）；
//! - Rust 签到直调后凭据全程内存传递（原 Python 脚本方案需写解密临时文件，已移除；
//!   启动清理逻辑保留，兜底清理旧版本残留的临时凭据文件）。

use std::path::Path;
use std::sync::Mutex;

use crate::fs_utils;
use crate::models::AccountsFile;
use crate::state::AppState;
use iota_stronghold::{KeyProvider, SnapshotPath, Stronghold};
use zeroize::Zeroizing;

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

/// vault 会话句柄：engine 句柄 + 快照路径 + 密钥提供者（快照落盘 commit 时需要）
struct VaultHandle {
    sh: Stronghold,
    snapshot: SnapshotPath,
    keyprovider: KeyProvider,
}

/// 全局 vault 句柄缓存：懒加载；持有锁期间完成读写 + 快照落盘，串行化访问
static VAULT: Mutex<Option<VaultHandle>> = Mutex::new(None);

/// 生成 32 字节随机主密码（OS CSPRNG：rand OsRng，跨平台；
/// 满足 Stronghold KeyProvider 恰好 32 字节 NC_DATA_SIZE 的要求）
fn generate_password() -> [u8; 32] {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

/// KeyProvider（ADR-2，去 DPAPI，Web/无桌面部署）：
/// 1. 环境变量 `AIWORK_VAULT_KEY` 非空（trim 后）→ 任意字符串 SHA-256 归一为 32 字节
///    （Docker/跨机部署可复现，不依赖本机文件）；
/// 2. 否则读 `conf/vault_key.bin`（"V2:" + hex(32B) 格式，首启自动生成，unix 下 0600 权限）；
/// 3. 检测到旧桌面版 DPAPI 加密 blob → 返回明确迁移错误（见 parse_key_file）。
fn vault_password(state: &AppState) -> Result<Vec<u8>, String> {
    if let Ok(v) = std::env::var("AIWORK_VAULT_KEY") {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(normalize_key(v.as_bytes()).to_vec());
        }
    }
    let key_path = state.conf_path("vault_key.bin");
    if key_path.exists() {
        let raw = std::fs::read(&key_path).map_err(|e| format!("读取 vault 密钥失败: {e}"))?;
        parse_key_file(&raw, &key_path)
    } else {
        let key = generate_password();
        let content = format!("V2:{}", bytes_to_hex(&key));
        write_key_file(&key_path, content.as_bytes())?;
        Ok(key.to_vec())
    }
}

/// 任意字符串密钥归一为 32 字节（SHA-256；env 密钥用，满足 NC_DATA_SIZE）
fn normalize_key(input: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}

/// 字节 → 小写 hex（密钥文件存储格式）
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 解析密钥文件：`V2:` 前缀 + 64 位 hex → 32 字节；
/// 其他内容视为旧桌面版 DPAPI 二进制 blob，返回明确迁移错误。
fn parse_key_file(raw: &[u8], path: &Path) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(raw);
    let Some(hex) = text.trim().strip_prefix("V2:") else {
        return Err(format!(
            "检测到旧版桌面应用的 vault 密钥（DPAPI 加密，Web/无桌面环境无法解密）。\
             迁移：先在桌面版导出账号 JSON（含凭据），然后删除 {} 与同目录 vault.stronghold，\
             再重启服务并在 Web 端导入该 JSON",
            path.display()
        ));
    };
    if hex.len() != 64 {
        return Err(format!("vault 密钥格式异常（期望 64 位 hex，实际 {} 字符）", hex.len()));
    }
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("vault 密钥 hex 解码失败: {e}"))?;
    }
    Ok(key.to_vec())
}

/// 写密钥文件（unix 下 0600，避免其他用户读取）
fn write_key_file(path: &Path, content: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("写入 vault 密钥失败: {e}"))?;
        f.write_all(content).map_err(|e| format!("写入 vault 密钥失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content).map_err(|e| format!("写入 vault 密钥失败: {e}"))
    }
}

/// 打开（并缓存）vault：首次调用时加载快照或创建新 client。
/// 对齐 tauri-plugin-stronghold 2.3.2 的包装方式（iota_stronghold 2.1 engine API：
/// Stronghold::default() + load_snapshot / commit_with_keyprovider）。
fn open(state: &AppState) -> Result<std::sync::MutexGuard<'static, Option<VaultHandle>>, String> {
    // 锁中毒恢复：另一线程在持锁期间 panic 毒化锁时，直接恢复内部数据继续使用，
    // 而不是让「vault 锁已被毒化」错误在所有后续调用上永久传播
    let mut guard = VAULT.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let path = state.conf_path("vault.stronghold");
        let password = vault_password(state)?;
        let snapshot = SnapshotPath::from_path(&path);
        // KeyProvider 要求主密码恰好 32 字节（NC_DATA_SIZE），vault_password 已保证归一
        let keyprovider = KeyProvider::try_from(Zeroizing::new(password))
            .map_err(|e| format!("vault 密钥初始化失败: {e}"))?;
        let sh = Stronghold::default();
        // 快照文件存在才加载（首次启动无快照，直接新建 client）
        if path.exists() {
            sh.load_snapshot(&keyprovider, &snapshot)
                .map_err(|e| format!("打开 vault 快照失败: {e}"))?;
        }
        // 快照数据不会自动进入 clients map，必须显式 load_client（见单元测试 stronghold_快照往返）；
        // 仅当快照中不存在该 client（首次创建）时才新建，防止空 client 覆盖已有快照导致凭据丢失
        if sh.load_client(CLIENT_PATH.to_vec()).is_err() {
            sh.create_client(CLIENT_PATH.to_vec())
                .map_err(|e| format!("创建 vault client 失败: {e}"))?;
        }
        *guard = Some(VaultHandle { sh, snapshot, keyprovider });
    }
    Ok(guard)
}

// ---------------- 公共 API ----------------

/// 从磁盘加载账号文件，并从 vault 回填占位账号的明文凭据（仅内存，不落明文盘）。
/// JSON 中已有的明文凭据优先（更新鲜，例如 MITM 新捕获，待下次保存迁移进 vault）。
pub fn load_accounts(state: &AppState) -> AccountsFile {
    // SQLite 化（P3）：checkin_accounts.json → accounts 表（行保序、user_id 可空）
    let mut file: AccountsFile = crate::store::docs::accounts_load(&crate::store::db(&state.data_dir));
    let Ok(guard) = open(state) else {
        return file; // vault 不可用：降级返回 JSON 原样（占位 jwt 视为空，上层自行报错）
    };
    let Some(handle) = guard.as_ref() else {
        return file;
    };
    let Ok(client) = handle.sh.get_client(CLIENT_PATH.to_vec()) else {
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
    let guard = open(state)?;
    let Some(handle) = guard.as_ref() else {
        return Err("vault 未初始化".into());
    };
    let client = handle
        .sh
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
    handle
        .sh
        .commit_with_keyprovider(&handle.snapshot, &handle.keyprovider)
        .map_err(|e| format!("vault 快照落盘失败: {e}"))?;
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
        let Some(handle) = guard.as_ref() else {
            return Err("vault 未初始化".into());
        };
        let client = handle
            .sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        client
            .store()
            .delete(uid.as_bytes())
            .map_err(|e| format!("vault 删除失败: {e}"))?;
        handle
            .sh
            .commit_with_keyprovider(&handle.snapshot, &handle.keyprovider)
            .map_err(|e| format!("vault 快照落盘失败: {e}"))
    })();
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

    #[test]
    fn key_provider_V2文件往返与旧blob拒绝() {
        // "V2:" + hex 文件 → 正确解码 32 字节
        let key = [7u8; 32];
        let content = format!("V2:{}", bytes_to_hex(&key));
        let parsed = parse_key_file(content.as_bytes(), Path::new("vault_key.bin")).expect("parse");
        assert_eq!(parsed, key.to_vec());

        // 旧版 DPAPI blob（无 V2 前缀）→ 明确迁移错误
        let err = parse_key_file(&[1, 2, 3, 4], Path::new("vault_key.bin")).unwrap_err();
        assert!(err.contains("旧版桌面"));

        // env 密钥归一化：任意字符串 → 确定性 32 字节
        assert_eq!(normalize_key(b"my-vault-key"), normalize_key(b"my-vault-key"));
        assert_eq!(normalize_key(b"my-vault-key").len(), 32);
    }

    #[cfg(windows)]
    #[test]
    fn stronghold_快照往返() {
        // 验证 engine API 用法：create_client → store 写入 → commit 快照 → 重载快照 → load_client 读取
        let dir = std::env::temp_dir().join(format!("vault_rs_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.stronghold");
        let snapshot = SnapshotPath::from_path(&path);
        // KeyProvider 要求主密码恰好 32 字节（NC_DATA_SIZE），与生产 generate_password 输出一致
        let keyprovider = KeyProvider::try_from(Zeroizing::new(vec![7u8; 32])).expect("keyprovider");
        {
            let sh = Stronghold::default();
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
            sh.commit_with_keyprovider(&snapshot, &keyprovider).expect("commit");
        }
        {
            let sh = Stronghold::default();
            sh.load_snapshot(&keyprovider, &snapshot).expect("load snapshot");
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
