//! 凭证保险库原语（F-75 M0-0.4）：
//! - Windows：DPAPI（CryptProtectData）加密 vault 主密码落 `conf/vault_key.bin`
//!   ——原 vault.rs 内联逻辑原样收口至此，**行为零变化**（文件布局/错误文案不变）；
//! - macOS：Keychain generic password（keyring crate，依赖隔离在
//!   `[target.'cfg(target_os = "macos")'.dependencies]`，Windows 构建零新依赖）。
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
        let _ = state; // mac 走 Keychain，无 conf 路径消费（Windows 分支用 state.conf_path）
        let entry = keyring_entry()?;
        match entry.get_password() {
            Ok(hex) => decode_hex(&hex).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("读取 Keychain 失败: {e}")),
        }
    }
}

/// 首次生成后写入受保护的 vault 主密码。
/// Windows：DPAPI 密文落 `conf/vault_key.bin`；macOS：32 字节 hex 编码入 Keychain
/// （Keychain 值必须为字符串，hex 无损往返且不引入 Base64 变体分歧）。
pub fn store_vault_password(state: &AppState, pwd: &[u8]) -> Result<(), String> {
    #[cfg(windows)]
    {
        let blob = crate::vault::dpapi::protect(pwd)?;
        let key_path = state.conf_path("vault_key.bin");
        std::fs::write(&key_path, &blob).map_err(|e| format!("写入 vault 密钥失败: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = state; // mac 走 Keychain，无 conf 路径消费（Windows 分支用 state.conf_path）
        let hex: String = pwd.iter().map(|b| format!("{b:02x}")).collect();
        keyring_entry()?
            .set_password(&hex)
            .map_err(|e| format!("写入 Keychain 失败: {e}"))
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
}
