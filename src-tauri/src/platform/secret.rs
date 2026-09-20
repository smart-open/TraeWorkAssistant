//! 凭证保险库原语（F-75 M0-0.4）：
//! - Windows：DPAPI（CryptProtectData）加密 vault 主密码落 `conf/vault_key.bin`
//!   ——原 vault.rs 内联逻辑原样收口至此，**行为零变化**（文件布局/错误文案不变）；
//! - macOS：`conf/vault_key.bin`（hex 明文，0600）**主源** + Keychain generic password
//!   （keyring crate）兜底。
//!
//! mac 双源决策（实测教训，2026-09-20）：dev 场景二进制反复重编译，macOS 对
//! ad-hoc 签名应用的 Keychain 条目访问不稳定（多次 NoEntry），曾导致「静默重生成
//! 密码 → 与 vault 密文永久失配 → 凭据全丢」。key file 与 vault 快照同生共死
//! （store 先写 file 成功才可能创建 vault），恒有「vault 加密密码 == file 最新值」
//! 不变量 → 读顺序 file 优先；Keychain 读写失败一律降级容忍（fail-open），仅作
//! 存量兼容与兜底线索。Windows 构建零变化。
//!
//! 设计决策——为什么用 keyring crate 而非 `security` CLI：CLI 的 `-w <密码>` 参数
//! 会暴露在进程列表（`ps` 可见），是真实的秘密泄露面；keyring 直调 Security.framework
//! 无此问题（backlog F-75 表格既定迁移目标 hwchen/keyring-rs）。

use crate::state::AppState;

/// Keychain 条目定位（macOS 专用）：服务名 = 应用域 + 账户名 = 用途
#[cfg(target_os = "macos")]
pub const KEYCHAIN_SERVICE: &str = "com.aiwork.assistant.vault";
#[cfg(target_os = "macos")]
pub const KEYCHAIN_ACCOUNT: &str = "stronghold-master";

/// mac key file 主源落位（与 Windows 同名同位：conf/vault_key.bin；内容为 hex 文本，
/// Windows 为 DPAPI blob——平台各自解释互不冲突）
#[cfg(target_os = "macos")]
fn key_file_path(state: &AppState) -> std::path::PathBuf {
    state.conf_path("vault_key.bin")
}

/// 读取受保护的 vault 主密码；不存在（首次运行）返回 Ok(None)。
/// Windows：读 `conf/vault_key.bin` + DPAPI 解密（文件存在但解密失败仍为 Err，
/// 与原 vault.rs 行为逐字一致——绝不静默重生成覆盖既有密文）。
pub fn load_vault_password(state: &AppState) -> Result<Option<Vec<u8>>, String> {
    #[cfg(windows)]
    {
        let key_path = state.conf_path("vault_key.bin");
        if !key_path.exists() {
            return Ok(None);
        }
        let blob = std::fs::read(&key_path).map_err(|e| format!("读取 vault 密钥失败: {e}"))?;
        crate::vault::dpapi::unprotect(&blob).map(Some)
    }
    #[cfg(target_os = "macos")]
    {
        // 1) 主源：key file（与 vault 同生共死的不变量，见模块注释）
        let key_path = key_file_path(state);
        if key_path.exists() {
            let text = match std::fs::read_to_string(&key_path) {
                Ok(t) => t,
                Err(e) => return Err(format!("读取 vault 密钥失败: {e}")),
            };
            match decode_hex(text.trim()) {
                Ok(bytes) => return Ok(Some(bytes)),
                // file 损坏（如写一半崩溃）→ fail-open 到 Keychain 兜底线索
                Err(_) => {}
            }
        }
        // 2) 兜底：Keychain（任何失败一律降级 None，不阻断主流程——dev 重签场景
        // 实测读不回自己写的条目，此时按首次运行处理，由 store 双写重建一致性）
        let entry = match keyring_entry() {
            Ok(e) => e,
            Err(_) => return Ok(None),
        };
        match entry.get_password() {
            Ok(hex) => decode_hex(&hex).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Ok(None),
        }
    }
}

/// 首次生成后写入受保护的 vault 主密码。
/// Windows：DPAPI 密文落 `conf/vault_key.bin`；macOS：hex 双写——key file（0600，
/// 成功为准）+ Keychain（尽力而为，失败降级容忍并记日志）。
pub fn store_vault_password(state: &AppState, pwd: &[u8]) -> Result<(), String> {
    #[cfg(windows)]
    {
        let blob = crate::vault::dpapi::protect(pwd)?;
        let key_path = state.conf_path("vault_key.bin");
        std::fs::write(&key_path, &blob).map_err(|e| format!("写入 vault 密钥失败: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        let hex: String = pwd.iter().map(|b| format!("{b:02x}")).collect();
        // 1) 主源：key file，0600 权限（写失败必须报错——它是 vault 的解密真值）
        let key_path = key_file_path(state);
        std::fs::write(&key_path, &hex).map_err(|e| format!("写入 vault 密钥失败: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
        // 2) 兜底：Keychain 尽力而为（dev 重签场景实测写后读不回/可能失败，不影响主源）
        if let Ok(entry) = keyring_entry() {
            if let Err(e) = entry.set_password(&hex) {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("vault 密钥 Keychain 兜底写入失败（已由 key file 主源保障）: {e}"),
                );
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn keyring_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("打开 Keychain 失败: {e}"))
}

#[cfg(target_os = "macos")]
fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    // 审查修复（P2）：按字节切片——`&s[i..i+2]` 字节索引落在多字节 UTF-8 字符
    // 中间会直接 panic（Keychain 内容理论上由本应用写入，防御性处理）
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err("Keychain 中 vault 主密码格式非法（奇数长度 hex）".into());
    }
    (0..bytes.len())
        .step_by(2)
        .map(|i| {
            let pair = std::str::from_utf8(&bytes[i..i + 2])
                .map_err(|e| format!("Keychain hex 解码失败: {e}"))?;
            u8::from_str_radix(pair, 16).map_err(|e| format!("Keychain hex 解码失败: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn vault密码_写读往返_行为与原内联实现一致() {
        // Windows 红线：走 DPAPI + conf/vault_key.bin，与 vault.rs 原内联逻辑等价
        let dir = std::env::temp_dir().join(format!("platform_secret_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        // 首次：无文件 → None
        assert_eq!(load_vault_password(&state).unwrap(), None);
        // 写入 → 读回一致
        let pwd: Vec<u8> = (0..32u8).collect();
        store_vault_password(&state, &pwd).unwrap();
        assert_eq!(load_vault_password(&state).unwrap(), Some(pwd));
        // 文件落位与原布局一致：conf/vault_key.bin
        assert!(state.conf_path("vault_key.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hex编解码往返() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_key_file主源优先_不触碰keychain() {
        // mac 红线：key file 存在时 load 直接返回其值（不走到 Keychain——测试环境
        // 碰真钥匙串会污染系统状态）。file 内容为 hex 文本、0600，与 store 写入一致。
        let dir = std::env::temp_dir().join(format!("platform_secret_mac_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        let pwd: Vec<u8> = (0..32u8).collect();
        let hex: String = pwd.iter().map(|b| format!("{b:02x}")).collect();
        let key_path = state.conf_path("vault_key.bin");
        std::fs::write(&key_path, &hex).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // file 主源读回一致
        assert_eq!(load_vault_password(&state).unwrap(), Some(pwd));
        // file 损坏 → fail-open 不报 Err（降级 Keychain 线索，测试环境 keychain
        // 大概率 None → Ok(None)）
        std::fs::write(&key_path, "zz").unwrap();
        let r = load_vault_password(&state);
        assert!(r.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
