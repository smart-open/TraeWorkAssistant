//! 敏感数据加密存储（Tauri Stronghold）。
//!
//! 设计：
//! - vault 快照 `conf/vault.stronghold` 权威保存各账号 jwt / refresh_token（按 uid 一条记录）；
//! - vault 主密码为随机 32 字节，存放在 `conf/vault_key.bin`，读取双格式自适应（issue #139）：
//!   `V2:`+64hex 文本（服务端 docker_main 格式，conf 目录共享场景）优先，
//!   否则按 Windows DPAPI blob 解密（仅本机当前用户可解）；
//!   新建文件默认 DPAPI；设置 `AIWORK_VAULT_SHARED=1` 时把已有 DPAPI 密钥一次性转换为
//!   V2: 文本以便与服务端共享 conf（代价：密钥明文 hex 落盘，显式开启方生效）；
//! - `checkin_accounts.json` 只保留占位（jwt/refresh_token 清空），其余字段（name/user_id 等）不动；
//! - 所有账号文件读写统一走 `load_accounts` / `save_accounts`：
//!   读：JSON 明文优先（更新鲜，如 MITM 新捕获）→ 否则从 vault 回填；
//!   写：非空凭据先写入 vault 并落盘快照 → JSON 占位化；
//!   vault 写失败时数据库保持原状并返回 Err（禁止明文落盘，也不销毁库中既有凭据——
//!   issue #153 修复 1）；
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

#[cfg(not(windows))]
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

/// OS CSPRNG 填充（Windows BCryptGenRandom 进程首选 RNG）；失败直接返回 Err。
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
        let _ = dest;
        // macOS 适配预留（2026-10-03）：等价替换 = SecRandomCopyBytes（Security
        // framework）或 libc getrandom/getentropy；qoder data_io.rs 加密导出的
        // salt/nonce 与 vault 主密码生成都经此函数，macOS 分支实装本分支即可
        // 全链路生效（fail-fast 语义保持：RNG 不可用必须 Err，不许弱熵兜底）
        Err("vault 仅支持 Windows（无可用 CSPRNG 封装）".into())
    }
}

/// 生成 32 字节随机主密码：熵源 = OS CSPRNG（BCryptGenRandom）。
/// 审查 P2-4：失败直接返回 Err 终止 vault 初始化（上层报错），删除旧实现的
/// 「RandomState + 高精度时间 + 进程 ID 经 SHA-256 压缩」弱熵兜底路径。
fn generate_password() -> Result<Vec<u8>, String> {
    let mut buf = [0u8; 32];
    csprng_fill(&mut buf)?;
    Ok(buf.to_vec())
}

/// 读取（或首次生成）vault 主密码。密钥文件双格式自适应（issue #139）：
/// `V2:`+64hex 文本（服务端 docker_main 格式）优先，否则按 DPAPI blob 解密；
/// 新建文件默认 DPAPI（保持「仅本机当前用户可解」基线）
fn vault_password_at(data_dir: &Path) -> Result<Vec<u8>, String> {
    let conf = data_dir.join("conf");
    let _ = std::fs::create_dir_all(&conf);
    let key_path = conf.join("vault_key.bin");
    if key_path.exists() {
        let blob =
            std::fs::read(&key_path).map_err(|e| vault_key_io_err("读取", &key_path, e))?;
        let pwd = parse_key_file(&blob, &key_path)?;
        // 共享模式一次性迁移：DPAPI 密钥 → V2: 文本（显式开启方生效，见 maybe_convert_to_v2）
        maybe_convert_to_v2(&key_path, &blob, &pwd, data_dir);
        Ok(pwd)
    } else {
        let pwd = generate_password()?;
        let blob = dpapi::protect(&pwd)?;
        std::fs::write(&key_path, &blob).map_err(|e| vault_key_io_err("写入", &key_path, e))?;
        Ok(pwd)
    }
}

/// vault 密钥文件 IO 错误映射（issue #153 修复 3）：os error 448 =
/// ERROR_UNTRUSTED_MOUNT_POINT，典型场景是 conf 目录被改成了 Junction/目录联接
/// （如用户手工迁移目录），系统拒绝跨联接访问——给出可操作提示而不是裸 OS 码
fn vault_key_io_err(op: &str, path: &Path, e: std::io::Error) -> String {
    if e.raw_os_error() == Some(448) {
        return format!(
            "{op} vault 密钥失败（os error 448：不受信任的挂载点）——{} 可能位于 Junction/目录联接之下，Windows 拒绝跨联接访问。请将 conf 目录恢复为真实目录（删除联接、把内容移回原位）后重启应用",
            path.display()
        );
    }
    format!("{op} vault 密钥失败: {e}")
}

/// 同一错误消息只落一次日志（issue #153 修复 3）：vault 故障期 ns_get 会被
/// 每账号每次 ensure_fresh 高频触发，相同失败原因会把 app_log 刷爆——进程内
/// 按消息去重，仅首条落盘；不同消息（不同 key/不同故障）各自首条仍落
fn log_once(data_dir: &Path, msg: &str) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    if seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(msg.to_string())
    {
        crate::fs_utils::app_log(data_dir, msg);
    }
}

/// 解析密钥文件（issue #139 双格式自适应）：
/// - `V2:` 前缀 + 64 位 hex → 32 字节（服务端 docker_main 格式，conf 目录共享场景）；
/// - 其余内容按 DPAPI blob 解密（桌面版原生格式）。
/// 失败时区分两种格式分别报错，替代原先笼统的 CryptUnprotectData 提示。
fn parse_key_file(raw: &[u8], path: &Path) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(raw);
    let Some(hex) = text.trim().strip_prefix("V2:") else {
        return dpapi::unprotect(raw).map_err(|e| {
            format!(
                "vault 密钥解析失败（{}：既非 V2: 文本格式，也非本机可解的 DPAPI blob）: {e}",
                path.display()
            )
        });
    };
    if hex.len() != 64 {
        return Err(format!(
            "vault 密钥格式异常（V2: 后期望 64 位 hex，实际 {hex_len} 字符）",
            hex_len = hex.len()
        ));
    }
    // hexdigit 全量校验：len() 是字节长度，多字节 UTF-8 字符可凑足 64 字节，
    // 直接按字节切片会在 char boundary 中间 panic；先统一收敛为友好 Err。
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "vault 密钥格式异常（V2: 后应为 64 位 ASCII hex，实际含非 hex 字符）"
        ));
    }
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("vault 密钥 hex 解码失败: {e}"))?;
    }
    Ok(key.to_vec())
}

/// 字节 → 小写 hex（V2: 密钥文件存储格式，与服务端 docker_main 一致）
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 共享模式开关：环境变量 AIWORK_VAULT_SHARED（1/true，大小写不敏感，容忍首尾空白）
fn vault_shared_mode() -> bool {
    std::env::var("AIWORK_VAULT_SHARED")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"))
        .unwrap_or(false)
}

/// 共享模式一次性迁移（issue #139）：`AIWORK_VAULT_SHARED=1` 且文件当前为 DPAPI blob 时，
/// 就地重写为 `V2:` 文本格式，使 aiwork-server（docker_main）可读同一 conf 目录。
/// 结果与失败均落日志；失败不阻断本次打开（下次启动重试）。
/// 注意：转换后密钥以明文 hex 落盘（失去 DPAPI 的「仅本机当前用户可解」保护），
/// 由用户显式开启共享模式来接受该权衡。
fn maybe_convert_to_v2(key_path: &Path, blob: &[u8], pwd: &[u8], data_dir: &Path) {
    if !vault_shared_mode() {
        return;
    }
    if String::from_utf8_lossy(blob).trim().starts_with("V2:") {
        return; // 已是 V2: 文本，无需转换
    }
    let content = format!("V2:{}", bytes_to_hex(pwd));
    match std::fs::write(key_path, content.as_bytes()) {
        Ok(()) => fs_utils::app_log(
            data_dir,
            "vault: AIWORK_VAULT_SHARED 已开启，vault_key.bin 已由 DPAPI blob 转换为 V2: 文本格式（可与 aiwork-server 共享 conf；密钥现为明文 hex，注意目录访问控制）",
        ),
        Err(e) => fs_utils::app_log(
            data_dir,
            &format!("vault: vault_key.bin 转换 V2: 写入失败（下次启动重试）: {e}"),
        ),
    }
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
        // 快照存在性必须在 Stronghold::new 之前判定（new 可能立即创建空快照文件）
        let snapshot_exists = path.exists();
        let password = vault_password_at(data_dir)?;
        let sh = Stronghold::new(&path, password).map_err(|e| format!("打开 vault 失败: {e}"))?;
        // 快照数据不会自动进入 clients map，必须显式 load_client（见单元测试 stronghold_快照往返）；
        // 仅当快照中不存在该 client（首次创建）时才新建，防止空 client 覆盖已有快照导致凭据丢失。
        // 密钥/快照错配防护（issue #139）：快照存在但 load_client 失败，大概率是 vault_key.bin
        // 与快照不匹配（如共享迁移期间替换过密钥）——拒绝打开，禁止空 client 经 save() 覆盖原快照
        if sh.load_client(CLIENT_PATH.to_vec()).is_err() {
            if snapshot_exists {
                return Err(format!(
                    "vault 快照已存在但 client 加载失败（{}）：大概率 vault_key.bin 与快照不匹配（如迁移期间替换过密钥）。已拒绝打开，防止空 client 覆盖快照；请恢复匹配的密钥或快照备份",
                    path.display()
                ));
            }
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

/// 探测 vault 当前是否可用（issue #153 修复 2）：不读写任何记录，仅验证
/// 打开+取 client 成功。用于调用方区分「vault 系统性故障」与「确实无凭证」——
/// ns_get 对两者都返回 None（fail-secure），凭证消费方（如 ensure_fresh）
/// 借此把故障期误判为 no_credential 的情况识别出来，避免触发永久禁用等
/// 不可自愈的下游处置。
pub fn available(data_dir: &Path) -> bool {
    with_open(data_dir, |sh| {
        sh.get_client(CLIENT_PATH.to_vec())
            .map(|_| ())
            .map_err(|e| format!("获取 vault client 失败: {e}"))
    })
    .is_ok()
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
        log_once(data_dir, &format!("[vault] ns_get {ns}:{key} 失败: {e}"));
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

// ---------------- 公共 API ----------------

/// 从磁盘加载账号文件，并从 vault 回填占位账号的明文凭据（仅内存，不落明文盘）。
/// JSON 中已有的明文凭据优先（更新鲜，例如 MITM 新捕获，待下次保存迁移进 vault）。
pub fn load_accounts(state: &AppState) -> AccountsFile {
    // SQLite 化（P3）：checkin_accounts.json → accounts 表（行保序、user_id 可空）
    let mut file: AccountsFile = crate::store::docs::accounts_load(&crate::store::db(&state.data_dir));
    // vault 不可用：降级返回 JSON 原样（占位 jwt 视为空，上层自行报错）
    let _ = with_open(&state.data_dir, |sh| {
        let client = sh
            .get_client(CLIENT_PATH.to_vec())
            .map_err(|e| format!("获取 vault client 失败: {e}"))?;
        for a in file.accounts.iter_mut() {
            let Some(uid) = a.user_id.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            if let Ok(Some(v)) = client.store().get(uid.as_bytes()) {
                if let Ok(entry) = serde_json::from_slice::<SecretEntry>(&v) {
                    // JWT：JSON 明文优先（更新鲜，如 MITM 新捕获），无明文才回填 vault
                    if a.jwt.trim().is_empty() && !entry.jwt.is_empty() {
                        a.jwt = entry.jwt;
                    }
                    // RT：与 JWT 独立判断——JSON 无明文即回填 vault（trim 口径与 JWT 一致）。
                    // 不能因 JWT 有捕获明文就 continue 跳过整个账号，否则代理捕获
                    // 写回明文 JWT 后，OAuth 登录拿到的 vault RT 永远回填不上，
                    // 账号会恒显「无 Refresh」（2026-10-11 实测）
                    if a.refresh_token
                        .as_deref()
                        .map_or(true, |s| s.trim().is_empty())
                        && !entry.refresh_token.is_empty()
                    {
                        a.refresh_token = Some(entry.refresh_token);
                    }
                }
            }
        }
        Ok(())
    });
    file
}

/// 保存账号文件：非空凭据写入 vault（字段级合并）并落盘快照，JSON 占位化。
/// vault 写失败时**禁止明文落盘**（审查 P1）且**数据库保持原状**（issue #153 修复 1）：
/// 原实现「wipe 占位 + 落库」会在 vault 故障（如 os error 448）时当场销毁库中既有的
/// 明文凭据（启动迁移场景）且无处恢复；现改为不落库直接返回 Err——迁移场景明文
/// 保留在库中待 vault 修复后下次启动重试收敛，常规场景本次凭据更新仅存内存。
/// 注意：仅成功路径会就地清空调用方结构体中的 jwt / refresh_token 字段。
pub fn save_accounts(state: &AppState, accounts: &mut AccountsFile) -> Result<(), String> {
    match write_vault_secrets(state, accounts) {
        Ok(()) => {
            // vault 已持久化 → 落库占位（明文唯一驻留地是 vault）
            wipe_placeholders(accounts);
            crate::store::docs::accounts_save(&crate::store::db(&state.data_dir), accounts)?;
            Ok(())
        }
        Err(reason) => {
            fs_utils::app_log(
                &state.data_dir,
                &format!("vault 写入失败，数据库未改动（凭据未持久化，禁止明文落库）: {reason}"),
            );
            Err(format!(
                "加密存储失败，数据库未改动，凭据未持久化（vault 错误: {reason}）"
            ))
        }
    }
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
            &format!("启动迁移: 写入 vault 失败（数据库未改动，明文保留待下次启动重试）: {e}"),
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
                            "启动迁移: WB 账号 {id} 凭证入 vault 失败（数据库未改动，明文保留待重试）: {e}"
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
            &format!("启动迁移: Qoder 凭证入 vault 失败（数据库未改动，明文保留待重试）: {e}"),
        ),
    }
    // ③ 豆包账号池（doubao_accounts 表）
    {
        // 读-改-写互斥：load→save 持池锁，防启动迁移与并发写方互踩
        let _guard = state.doubao_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
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
                    &format!("启动迁移: 豆包凭证入 vault 失败（数据库未改动，明文保留待重试）: {e}"),
                ),
            }
        }
    }
    // ④ 豆包抓包凭证（审查 P1-7：kv doubao_captured_credentials → vault，
    // ns "doubao"/"captured"；vault 成功后删除明文 kv 行，读取侧 vault 优先）
    {
        let plain: serde_json::Value =
            crate::store::db(&state.data_dir).kv_get("doubao_captured_credentials");
        let has_session = plain
            .get("session_id")
            .and_then(|s| s.as_str())
            .map_or(false, |s| !s.is_empty());
        if has_session {
            match crate::vault::ns_set(&state.data_dir, "doubao", "captured", &plain) {
                Ok(()) => {
                    let _ =
                        crate::store::db(&state.data_dir).kv_delete("doubao_captured_credentials");
                    fs_utils::app_log(&state.data_dir, "启动迁移: 豆包抓包凭证已加密写入 vault");
                }
                Err(e) => fs_utils::app_log(
                    &state.data_dir,
                    &format!("启动迁移: 豆包抓包凭证入 vault 失败（下次启动重试）: {e}"),
                ),
            }
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

    #[test]
    fn parse_key_file_v2文本解析与hex往返() {
        let dir = std::env::temp_dir().join(format!("vault_parse_v2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vault_key.bin");
        let key = [0xABu8; 32];
        // 标准格式：V2: + 64 位小写 hex（与服务端 docker_main 写出格式一致）
        std::fs::write(&path, format!("V2:{}", bytes_to_hex(&key))).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(parse_key_file(&raw, &path).expect("parse V2"), key.to_vec());
        // 带首尾空白（复制粘贴常见）也能解析
        std::fs::write(&path, format!("  V2:{}\r\n", bytes_to_hex(&key))).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(parse_key_file(&raw, &path).expect("parse trimmed"), key.to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_key_file_v2长度或字符非法时报错() {
        let path = Path::new("vault_key.bin");
        // 长度不对（63 位 hex）
        let short = format!("V2:{}a", "ab".repeat(31));
        let err = parse_key_file(short.as_bytes(), path).unwrap_err();
        assert!(err.contains("64 位 hex"), "实际报错: {err}");
        // 非 hex 字符（ASCII）→ 被 hexdigit 全量校验拦下
        let bad = format!("V2:{}", "z".repeat(64));
        let err = parse_key_file(bad.as_bytes(), path).unwrap_err();
        assert!(err.contains("非 hex 字符"), "实际报错: {err}");
        // 多字节 UTF-8 且总字节长恰为 64（共享目录手工编辑污染场景）：
        // é 从奇数字节偏移 61 开始，旧代码按字节切片会在 char boundary
        // 中间 panic；须收敛为友好 Err（panic 回归用例）
        let multibyte = format!("V2:{}éa", "a".repeat(61));
        assert_eq!(multibyte.len(), 67); // 3 前缀 + 61 + 2 + 1 = 67 字节
        let err = parse_key_file(multibyte.as_bytes(), path).unwrap_err();
        assert!(err.contains("非 hex 字符"), "实际报错: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn parse_key_file_dpapi回退与非双格式报错() {
        let dir = std::env::temp_dir().join(format!("vault_parse_dpapi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vault_key.bin");
        // DPAPI blob（无 V2: 前缀）→ 走回退分支解出原密钥
        let key = [0x5Au8; 32];
        std::fs::write(&path, dpapi::protect(&key).unwrap()).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(parse_key_file(&raw, &path).expect("dpapi fallback"), key.to_vec());
        // 既非 V2: 也非 DPAPI（随机字节）→ 明确报错且点出两种格式
        let err = parse_key_file(&[0xFF, 0x00, 0x13, 0x37], &path).unwrap_err();
        assert!(err.contains("既非 V2:") && err.contains("DPAPI"), "实际报错: {err}");
        let _ = std::fs::remove_dir_all(&dir);
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
