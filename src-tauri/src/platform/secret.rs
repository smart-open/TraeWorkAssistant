//! 凭证保险库原语（F-75 M0-0.4）：
//! - Windows：`conf/vault_key.bin` 双格式自适应（issue #139：`V2:`+64hex 文本优先、
//!   DPAPI blob 回退；`AIWORK_VAULT_SHARED=1` 时一次性迁移 DPAPI→V2:）——原
//!   vault.rs 内联逻辑收口至此，文件布局/错误文案不变；
//! - macOS：`conf/vault_key.bin`（纯 hex 明文 0600 为主源，兼容读取服务端
//!   docker_main 写出的 `V2:` 共享格式；`AIWORK_VAULT_SHARED=1` 时一次性迁移
//!   纯 hex→`V2:`）+ Keychain generic password（keyring crate）兜底。
//!
//! mac 双源决策（实测教训，2026-09-20）：dev 场景二进制反复重编译，macOS 对
//! ad-hoc 签名应用的 Keychain 条目访问不稳定（多次 NoEntry），曾导致「静默重生成
//! 密码 → 与 vault 密文永久失配 → 凭据全丢」。key file 与 vault 快照同生共死
//! （store 先写 file 成功才可能创建 vault），恒有「vault 加密密码 == file 最新值」
//! 不变量 → 读顺序 file 优先；Keychain 读写失败一律降级容忍（fail-open），仅作
//! 存量兼容与兜底线索。Windows 构建零变化。
//!
//! mac key file 威胁模型注记（合并审查 #10）：hex 明文 + 0600 权限防的是「同机
//! 普通用户进程」越权读取；同机 root、磁盘镜像、物理接触本就无防护边界——Keychain
//! 条目同样可被 root 提取（security dump-keychain / 同 TCC 域直调）。这与 Windows
//! DPAPI 的用户级威胁模型完全一致（DPAPI blob 对同用户任意进程可解），不构成
//! mac 侧额外的安全降级，故维持现状不加额外加密层。
//!
//! 设计决策——为什么用 keyring crate 而非 `security` CLI：CLI 的 `-w <密码>` 参数
//! 会暴露在进程列表（`ps` 可见），是真实的秘密泄露面；keyring 直调 Security.framework
//! 无此问题（backlog F-75 表格既定迁移目标 hwchen/keyring-rs）。

/// Keychain 条目定位（macOS 专用）：服务名 = 应用域 + 账户名 = 用途
#[cfg(target_os = "macos")]
pub const KEYCHAIN_SERVICE: &str = "com.aiwork.assistant.vault";
#[cfg(target_os = "macos")]
pub const KEYCHAIN_ACCOUNT: &str = "stronghold-master";

/// 读取受保护的 vault 主密码；不存在（首次运行）返回 Ok(None)。
/// Windows：读 `conf/vault_key.bin` 双格式自适应（issue #139：V2: 文本优先、
/// DPAPI blob 回退；文件存在但解析失败仍为 Err——绝不静默重生成覆盖既有密文），
/// 共享模式开启时一次性迁移 DPAPI→V2:；
/// macOS：key file 主源（纯 hex，兼容 V2: 共享格式）+ Keychain 兜底（见模块注释），
/// 共享模式开启时一次性迁移纯 hex→V2:。
/// data_dir 参数化（vault 多 data_dir 化后 vault_password_at 直调；
/// `data_dir/conf/vault_key.bin` 与 state.conf_path 同一物理位置）。
pub fn load_vault_password_at(data_dir: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    #[cfg(windows)]
    {
        let key_path = data_dir.join("conf").join("vault_key.bin");
        if !key_path.exists() {
            return Ok(None);
        }
        // issue #153 修复 3：os error 448（不受信任的挂载点，conf 被 Junction 化）等
        // IO 错误经 vault_key_io_err 映射为可操作提示
        let blob =
            std::fs::read(&key_path).map_err(|e| crate::vault::vault_key_io_err("读取", &key_path, e))?;
        // issue #139 双格式自适应：V2: 文本优先，DPAPI blob 回退
        let pwd = crate::vault::parse_key_file(&blob, &key_path)?;
        // 共享模式一次性迁移：DPAPI 密钥 → V2: 文本（AIWORK_VAULT_SHARED=1 方生效）
        crate::vault::maybe_convert_to_v2(&key_path, &blob, &pwd, data_dir);
        Ok(Some(pwd))
    }
    #[cfg(target_os = "macos")]
    {
        // 1) 主源：key file（与 vault 同生共死的不变量，见模块注释）
        let key_path = data_dir.join("conf").join("vault_key.bin");
        if key_path.exists() {
            let text = match std::fs::read_to_string(&key_path) {
                Ok(t) => t,
                Err(e) => return Err(format!("读取 vault 密钥失败: {e}")),
            };
            // issue #139：V2: 共享格式（服务端 docker_main 写出的 conf 共享场景）优先，
            // 带 V2: 前缀但非法 → 直接报错（格式明确，静默降级会掩盖文件损坏）；
            // 无 V2: 前缀 → 本分支原生纯 hex 格式
            let file_pwd = match crate::vault::parse_v2_hex_text(&text)? {
                Some(k) => Some(k),
                None => decode_hex(text.trim()).ok(),
            };
            if let Some(pwd) = file_pwd {
                // 共享模式一次性迁移：纯 hex → V2: 文本（AIWORK_VAULT_SHARED=1，
                // 镜像 main 在 Windows 的 DPAPI→V2: 迁移语义，复用同一实现）
                crate::vault::maybe_convert_to_v2(&key_path, text.as_bytes(), &pwd, data_dir);
                return Ok(Some(pwd));
            }
            // file 损坏（如写一半崩溃）→ fail-open 到 Keychain 兜底线索
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

/// 首次生成后写入受保护的 vault 主密码（与 [load_vault_password_at] 配对）。
/// 共享模式（`AIWORK_VAULT_SHARED=1`）：直接原子写 `V2:` 文本到 key file
/// （审查修复 低-3：原先共享模式下仍按平台原生格式落盘，服务端读到 DPAPI blob /
/// 非 V2: hex 后无法解析，直至下次 load 触发迁移，首次写入与共享语义不一致）。
/// Windows：DPAPI 密文落 `conf/vault_key.bin`；macOS：hex 双写——key file（0600，
/// 成功为准）+ Keychain（尽力而为，失败降级容忍并记日志）。
pub fn store_vault_password_at(data_dir: &std::path::Path, pwd: &[u8]) -> Result<(), String> {
    // 共享开关参数注入版（审查修复 中-2）：进程级 env 判定收敛在此唯一入口，
    // 测试直调 [store_vault_password_at_impl] 注入布尔值，不触碰 AIWORK_VAULT_SHARED
    store_vault_password_at_impl(data_dir, pwd, crate::vault::vault_shared_mode())
}

/// [store_vault_password_at] 实现核心（共享开关注入）。
/// 共享模式优先：key file 主源直接写 V2: 共享格式（与服务端 docker_main 一致），
/// 原子写入；mac 额外做 Keychain 兜底（与原生路径同一策略）
fn store_vault_password_at_impl(
    data_dir: &std::path::Path,
    pwd: &[u8],
    shared: bool,
) -> Result<(), String> {
    if shared {
        let hex = crate::vault::bytes_to_hex(pwd);
        let key_path = data_dir.join("conf").join("vault_key.bin");
        crate::vault::write_key_file_atomic(&key_path, &format!("V2:{hex}"))?;
        #[cfg(target_os = "macos")]
        {
            // Keychain 兜底尽力而为（存纯 hex，无 V2: 前缀，与 load 兜底解码路径一致）
            if let Ok(entry) = keyring_entry() {
                if let Err(e) = entry.set_password(&hex) {
                    crate::fs_utils::app_log(
                        data_dir,
                        &format!("vault 密钥 Keychain 兜底写入失败（已由 key file 主源保障）: {e}"),
                    );
                }
            }
        }
        return Ok(());
    }
    #[cfg(windows)]
    {
        let blob = crate::vault::dpapi::protect(pwd)?;
        let key_path = data_dir.join("conf").join("vault_key.bin");
        std::fs::write(&key_path, &blob)
            .map_err(|e| crate::vault::vault_key_io_err("写入", &key_path, e))
    }
    #[cfg(target_os = "macos")]
    {
        let hex: String = pwd.iter().map(|b| format!("{b:02x}")).collect();
        // 1) 主源：key file（写失败必须报错——它是 vault 的解密真值）。
        //    审查修复（Low）：原 create+truncate 与 write_all 两步非原子，中途崩溃/
        //    断电会留下 0 字节 key file（decode_hex 已按损坏源防御，但会 fail-open
        //    到 Keychain 兜底，旧值可能已过期且与密文失配）——复用
        //    vault::write_key_file_atomic（tmp 0600 创建 + fsync + rename 原子替换，
        //    rename 失败清 tmp），写纯 hex 格式不变；rename 后新 inode 即 0600，
        //    原 set_permissions 兜底随之删除
        let key_path = data_dir.join("conf").join("vault_key.bin");
        crate::vault::write_key_file_atomic(&key_path, &hex)?;
        // 2) 兜底：Keychain 尽力而为（dev 重签场景实测写后读不回/可能失败，不影响主源）
        if let Ok(entry) = keyring_entry() {
            if let Err(e) = entry.set_password(&hex) {
                crate::fs_utils::app_log(
                    data_dir,
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
    // 审查修复：空串判为损坏源——store 已改为原子写（tmp+rename），不再产生
    // 0 字节 key file，但历史文件/外部写入仍可能为空；空输入若返回 Ok(vec![])，
    // 空密码会直达 Stronghold 解锁必败，且跳过 Keychain fail-open 兜底（Keychain
    // 可能仍持有旧值）。空串与损坏 hex 同等对待。
    if bytes.is_empty() {
        return Err("vault 密钥为空（key file 可能写入中断）".into());
    }
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
    use crate::state::AppState;

    #[cfg(windows)]
    #[test]
    fn vault密码_写读往返_行为与原内联实现一致() {
        // Windows 红线：走 DPAPI + conf/vault_key.bin，与 vault.rs 原内联逻辑等价
        let dir = std::env::temp_dir().join(format!("platform_secret_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        // 首次：无文件 → None
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), None);
        // 写入 → 读回一致
        let pwd: Vec<u8> = (0..32u8).collect();
        store_vault_password_at(&state.data_dir, &pwd).unwrap();
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), Some(pwd));
        // 文件落位与原布局一致：conf/vault_key.bin
        assert!(state.conf_path("vault_key.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hex编解码往返() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(decode_hex("").is_err());
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }

    /// 共享模式 store 分支（审查修复 中-2 之前零覆盖的两平台行为分叉点）：
    /// 参数注入 shared=true（不触碰 AIWORK_VAULT_SHARED env）——
    /// Windows：写 V2: 文本而非 DPAPI blob（服务端 docker_main 必须可解析）；
    /// macOS：key file 原子写 0600（Keychain 兜底尽力而为，测试环境失败被容忍）。
    /// 写入后经非共享 load 的 V2: 解析路径读回一致（与 store 配对的完整往返）。
    #[test]
    fn vault密码_共享模式store写v2文本_读回一致_不落dpapi() {
        let dir =
            std::env::temp_dir().join(format!("platform_secret_shared_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        let pwd: Vec<u8> = (0..32u8).collect();
        store_vault_password_at_impl(&state.data_dir, &pwd, true).unwrap();
        let key_path = state.conf_path("vault_key.bin");
        // 必须是 V2: 文本（若误落 DPAPI blob 则 read_to_string 直接失败）
        let content = std::fs::read_to_string(&key_path).unwrap();
        assert!(
            content.starts_with("V2:") && content.contains(&crate::vault::bytes_to_hex(&pwd)),
            "共享模式 store 应写 V2:<hex> 文本: {content}"
        );
        // 非共享 load 的 V2: 解析路径读回一致（store/load 配对往返）
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), Some(pwd.clone()));
        // 幂等：重复 store 内容稳定（原子写不残留临时文件）
        store_vault_password_at_impl(&state.data_dir, &pwd, true).unwrap();
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), content);
        assert!(!key_path.with_extension("bin.v2tmp").exists(), "不得残留临时文件");
        let _ = std::fs::remove_dir_all(&dir);
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
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        let pwd: Vec<u8> = (0..32u8).collect();
        let hex: String = pwd.iter().map(|b| format!("{b:02x}")).collect();
        let key_path = state.conf_path("vault_key.bin");
        std::fs::write(&key_path, &hex).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // file 主源读回一致
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), Some(pwd));
        // file 损坏 → fail-open 不报 Err（降级 Keychain 线索，测试环境 keychain
        // 大概率 None → Ok(None)）
        std::fs::write(&key_path, "zz").unwrap();
        let r = load_vault_password_at(&state.data_dir);
        assert!(r.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_key_file_v2共享格式可读_共享模式迁移为v2() {
        // issue #139 mac 适配：key file 兼容服务端 docker_main 写出的 V2: 共享格式；
        // AIWORK_VAULT_SHARED=1 时纯 hex → V2: 一次性迁移（镜像 main 在 Windows 的语义）
        let dir = std::env::temp_dir().join(format!("platform_secret_mac_v2_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        let pwd: Vec<u8> = (0..32u8).collect();
        let key_path = state.conf_path("vault_key.bin");
        // 1) V2: 共享格式直接可读
        std::fs::write(&key_path, format!("V2:{}", crate::vault::bytes_to_hex(&pwd))).unwrap();
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), Some(pwd.clone()));
        // 2) 纯 hex → 读回一致；再以参数注入直调迁移核心（审查修复 中-2：不触碰
        //    进程级全局环境变量，避免并行测试竞争与 panic 泄漏面），驱动纯 hex →
        //    V2: 一次性迁移（与 AIWORK_VAULT_SHARED=1 时 maybe_convert_to_v2 的
        //    内部行为一致，仅省去 env 门判定）
        std::fs::write(&key_path, crate::vault::bytes_to_hex(&pwd)).unwrap();
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), Some(pwd.clone()));
        let hex_text = std::fs::read_to_string(&key_path).unwrap();
        crate::vault::convert_to_v2_impl(&key_path, hex_text.as_bytes(), &pwd, &state.data_dir);
        let migrated = std::fs::read_to_string(&key_path).unwrap();
        assert!(migrated.trim().starts_with("V2:"), "迁移后应为 V2: 文本: {migrated}");
        // 迁移后再次读取仍一致（V2: 回读路径）；重复迁移幂等（内容不变）
        assert_eq!(load_vault_password_at(&state.data_dir).unwrap(), Some(pwd.clone()));
        crate::vault::convert_to_v2_impl(&key_path, migrated.as_bytes(), &pwd, &state.data_dir);
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap().trim(),
            migrated.trim(),
            "已是 V2: 格式时迁移核心应跳过（幂等）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
