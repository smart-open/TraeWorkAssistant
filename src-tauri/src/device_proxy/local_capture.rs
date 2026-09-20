//! TRAE 本地登录态捕获（原 device_proxy.py `--capture-local` / `capture_from_local` 迁移）。
//!
//! 背景：当前版本 TRAE 的鉴权请求(api.trae.cn)不走系统代理，MITM 代理抓不到
//! Cloud-IDE-JWT。但 TRAE 把登录态存在本地 Cookies(Chromium 格式)与 Local Storage
//! leveldb，此处直接解密提取并经 [`handler::update_account_jwt`] 写回账号表，
//! 作为代理方案的兜底（Windows + macOS 双平台）。
//!
//! 平台差异（均为 Chromium os_crypt 惯例）：
//! - Windows：Local State `os_crypt.encrypted_key`（DPAPI）→ AES-256-GCM（'v10'）；
//!   旧格式直接 DPAPI。目录 `%APPDATA%/{TRAE SOLO CN, Trae CN}`。
//! - macOS：Keychain `<App> Safe Storage`（account="Chrome"，跨应用读取会触发
//!   钥匙串授权弹窗）→ PBKDF2-HMAC-SHA1(salt="saltysalt", 1003 轮) → AES-128-CBC
//!  （'v10'，IV=16×空格，PKCS7）。目录 `~/Library/Application Support/{TRAE SOLO CN, Trae CN}`。
//! - leveldb 明文扫描双平台同款。
//!
//! 入口：CLI 任务模式 `--task-run trae-capture-local`（对齐原 `python device_proxy.py
//! --capture-local` 手动兜底用法）；另被 refresh_jwt 失败后的本地恢复自动调用。

use std::path::{Path, PathBuf};

use crate::state::AppState;

use super::handler::{extract_user_id, update_account_jwt, valid_cloud_ide_jwt, ProxyCtx};

/// 候选应用数据目录（对齐 Python `_trae_app_dirs`）：TRAE SOLO CN（Trae Work）+ Trae CN；
/// 设置 TRAE_APP_DIR 时只扫指定目录（兼容旧环境变量）。
#[cfg(windows)]
fn trae_app_dirs() -> Vec<PathBuf> {
    if let Ok(env) = std::env::var("TRAE_APP_DIR") {
        if !env.is_empty() {
            return vec![PathBuf::from(env)];
        }
    }
    let base = std::env::var("APPDATA").unwrap_or_default();
    if base.is_empty() {
        return vec![];
    }
    vec![
        Path::new(&base).join("TRAE SOLO CN"),
        Path::new(&base).join("Trae CN"),
    ]
}

/// mac 版候选目录：`~/Library/Application Support/{TRAE SOLO CN, Trae CN}`
///（实测两目录结构与 Windows Electron 布局一致：根目录 Cookies + Local Storage + Partitions）
#[cfg(target_os = "macos")]
fn trae_app_dirs() -> Vec<PathBuf> {
    if let Ok(env) = std::env::var("TRAE_APP_DIR") {
        if !env.is_empty() {
            return vec![PathBuf::from(env)];
        }
    }
    let base = std::env::var("HOME").unwrap_or_default();
    if base.is_empty() {
        return vec![];
    }
    let base = Path::new(&base).join("Library").join("Application Support");
    vec![
        base.join("TRAE SOLO CN"),
        base.join("Trae CN"),
    ]
}

/// 离线场景构造 ProxyCtx（复用 handler 的账号写回通路；不启动代理、不 emit 前端事件）
fn offline_ctx(state: &AppState) -> ProxyCtx {
    let captured = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    ProxyCtx {
        log: super::logger::ProxyLog::new(state.logs_dir().join("proxy.log"), None, captured),
        req_logger: std::sync::Arc::new(super::logger::RequestLogger::new(state.logs_dir())),
        targets: Vec::new(),
        auto_capture_jwt: true,
        data_dir: state.data_dir.clone(),
        upstream: None,
        pin_state: std::sync::Mutex::new(std::collections::HashMap::new()),
    }
}

/// 解密 TRAE 本地 Cookies + 扫描 Local Storage leveldb，提取 Cloud-IDE-JWT 写回
/// 账号表（对齐 Python `capture_from_local`）。返回新增/更新账号数。
pub fn capture_from_local(state: &AppState) -> Result<serde_json::Value, String> {
    let ctx = offline_ctx(state);
    let mut total = 0usize;
    for app_dir in trae_app_dirs() {
        match capture_from_app_dir(&ctx, &app_dir) {
            Ok(n) => total += n,
            Err(e) => ctx.log.log(&format!("[local] 目录 {} 捕获失败: {e}", app_dir.display())),
        }
    }
    ctx.log.log(&format!("[local] 本地捕获完成，新增/更新 {total} 个账号"));
    Ok(serde_json::json!({ "ok": true, "captured": total }))
}

// ---------------- Windows 解密细节（对齐 Python _chrome_aes_key/_decrypt_cookie） ----------------

/// 读 Local State → os_crypt.encrypted_key（DPAPI 包裹的 AES-256 密钥）
#[cfg(windows)]
fn chrome_aes_key(app_dir: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let lp = app_dir.join("Local State");
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
    crate::vault::dpapi::unprotect(&raw[5..])
}

/// mac 版密钥：Keychain `<App> Safe Storage`（Chromium os_crypt_mac，account 固定
/// "Chrome"）→ PBKDF2-HMAC-SHA1(salt="saltysalt", 1003 轮, 16 字节)。
/// 首次跨应用读取会弹钥匙串授权弹窗，用户须点「始终允许」。
#[cfg(target_os = "macos")]
fn chrome_aes_key(app_dir: &Path) -> Result<Vec<u8>, String> {
    let app_name = app_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let entry = keyring::Entry::new(&format!("{app_name} Safe Storage"), "Chrome")
        .map_err(|e| format!("打开 Keychain 条目失败: {e}"))?;
    let password = entry
        .get_password()
        .map_err(|e| format!("读取 Safe Storage 失败: {e}（若弹了钥匙串授权框请选「始终允许」后重试）"))?;
    let mut key = vec![0u8; 16];
    pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(password.as_bytes(), b"saltysalt", 1003, &mut key)
        .map_err(|e| format!("PBKDF2 密钥派生失败: {e}"))?;
    Ok(key)
}

/// 单个 cookie 密文解密（Windows）：v10 为 AES-256-GCM（'v10'+nonce12+ct+tag16），
/// 旧格式直接 DPAPI。key 缺失时 v10 放弃（返回 None，调用方回退明文 value）。
#[cfg(windows)]
fn decrypt_cookie(enc: &[u8], key: Option<&[u8]>) -> Option<String> {
    if enc.len() >= 3 && &enc[..3] == b"v10" {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        if enc.len() < 19 {
            return None;
        }
        let key = key?;
        let cipher = Aes256Gcm::new_from_slice(key).ok()?;
        let plain = cipher.decrypt(Nonce::from_slice(&enc[3..15]), &enc[15..]).ok()?;
        return Some(String::from_utf8_lossy(&plain).to_string());
    }
    crate::vault::dpapi::unprotect(enc).ok().map(|v| String::from_utf8_lossy(&v).to_string())
}

/// 单个 cookie 密文解密（macOS）：'v10' + AES-128-CBC（IV=16×空格）+ PKCS7 去填充。
/// mac 无 DPAPI 旧格式，非 v10 前缀一律 None。
#[cfg(target_os = "macos")]
fn decrypt_cookie(enc: &[u8], key: Option<&[u8]>) -> Option<String> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    if enc.len() < 19 || &enc[..3] != b"v10" {
        return None;
    }
    let key = key?;
    let key: &[u8; 16] = key.try_into().ok()?;
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
    let dec = Aes128CbcDec::new_from_slices(key, &[0x20u8; 16]).ok()?;
    let mut buf = enc[3..].to_vec();
    let plain = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    Some(String::from_utf8_lossy(plain).to_string())
}

/// 从一段文本里找 Cloud-IDE-JWT（对齐 Python `_find_cloud_ide_jwt`）：
/// 先匹配显式前缀，命中即返回（无效也直接 None，不落入通用扫描）；
/// 否则通用三段 JWT 逐个校验，返回首个通过者（格式化为 `Cloud-IDE-JWT <jwt>`）。
fn find_cloud_ide_jwt(blob: &str) -> Option<String> {
    use std::sync::OnceLock;
    static PREFIX_RE: OnceLock<regex::Regex> = OnceLock::new();
    static JWT_RE: OnceLock<regex::Regex> = OnceLock::new();
    if blob.is_empty() {
        return None;
    }
    let prefix = PREFIX_RE.get_or_init(|| {
        regex::Regex::new(r"Cloud-IDE-JWT\s+([A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+)")
            .expect("jwt prefix regex")
    });
    if let Some(caps) = prefix.captures(blob) {
        return valid_cloud_ide_jwt(&caps[1]);
    }
    let jwt = JWT_RE
        .get_or_init(|| regex::Regex::new(r"[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+").expect("jwt regex"));
    for m in jwt.find_iter(blob) {
        if let Some(v) = valid_cloud_ide_jwt(m.as_str()) {
            return Some(v);
        }
    }
    None
}

/// 对单个应用数据目录执行 Cookies 解密 + leveldb 明文扫描（对齐 Python
/// `_capture_from_app_dir`）。返回新增/更新账号数。
/// Cookies 库位置双平台兼容：旧版 Electron 在根目录，新版在 Network/ 子目录，
/// 另有 Partitions/trae-webview 分区——三处都扫，不存在即跳过。
fn capture_from_app_dir(ctx: &ProxyCtx, app_dir: &Path) -> Result<usize, String> {
    let mut found = 0usize;
    ctx.log.log(&format!("[local] TRAE 数据目录: {}", app_dir.display()));
    if !app_dir.is_dir() {
        ctx.log.log("[local] 目录不存在，跳过");
        return Ok(0);
    }
    let key = match chrome_aes_key(app_dir) {
        Ok(k) => Some(k),
        Err(e) => {
            ctx.log.log(&format!("[local] 未取得 AES 密钥({e})；将仅扫描明文值"));
            None
        }
    };

    // 1) Cookies 数据库：根目录（mac 实测位置）+ Network/（新版 Chromium 布局）+ trae-webview 分区
    let mut dbs = vec![app_dir.join("Cookies"), app_dir.join("Network").join("Cookies")];
    let tw = app_dir.join("Partitions").join("trae-webview").join("Cookies");
    if tw.exists() {
        dbs.push(tw);
    }
    for db in dbs {
        if !db.exists() {
            continue;
        }
        match scan_cookies_db(ctx, &db, key.as_deref()) {
            Ok(n) => found += n,
            Err(e) => ctx.log.log(&format!("[local] 读取 Cookies 失败 {}: {e}", db.display())),
        }
    }

    // 2) Local Storage leveldb 明文兜底扫描
    for ls in [
        app_dir.join("Local Storage").join("leveldb"),
        app_dir.join("Partitions").join("trae-webview").join("Local Storage").join("leveldb"),
    ] {
        if !ls.is_dir() {
            continue;
        }
        found += scan_leveldb_dir(ctx, &ls);
    }
    Ok(found)
}

/// 复制 Cookies 库到临时目录后只读打开（规避客户端运行时文件锁；
/// -wal/-shm 一并复制避免读到未 checkpoint 的空库），返回命中的账号数
fn scan_cookies_db(ctx: &ProxyCtx, db: &Path, key: Option<&[u8]>) -> Result<usize, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let td = std::env::temp_dir().join(format!("aw_trae_ck_{}_{nanos}", std::process::id()));
    std::fs::create_dir_all(&td).map_err(|e| format!("临时目录创建失败: {e}"))?;
    let tmp_db = td.join("Cookies");
    let copy_res = std::fs::copy(db, &tmp_db).map_err(|e| format!("Cookies 复制失败: {e}"));
    if copy_res.is_err() {
        let _ = std::fs::remove_dir_all(&td);
        return Err(copy_res.unwrap_err());
    }
    for suffix in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("Cookies{suffix}"));
        if side.exists() {
            let _ = std::fs::copy(&side, td.join(format!("Cookies{suffix}")));
        }
    }

    let result = (|| -> Result<usize, String> {
        let conn = rusqlite::Connection::open_with_flags(
            &tmp_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("打开 Cookies 库失败: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT host_key, name, value, encrypted_value FROM cookies")
            .map_err(|e| format!("查询 cookies 表失败: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|e| format!("遍历 cookies 失败: {e}"))?;
        let mut found = 0usize;
        for r in rows.flatten() {
            let (host, name, value, enc) = r;
            // 明文 value 优先作 blob；密文解密成功则覆盖（解密失败回退明文，对齐 Python）
            let mut blob = value;
            if !enc.is_empty() {
                if let Some(d) = decrypt_cookie(&enc, key) {
                    blob = d;
                }
            }
            let Some(hit) = find_cloud_ide_jwt(&blob) else { continue };
            let Some(uid) = extract_user_id(&hit) else { continue };
            let status = update_account_jwt(ctx, &uid, &hit);
            ctx.log.log(&format!(
                "[local] 命中 Cookies host={host} name={name} -> {status}"
            ));
            found += 1;
        }
        Ok(found)
    })();
    let _ = std::fs::remove_dir_all(&td);
    result
}

/// leveldb 目录明文扫描：.ldb/.log 文件按字节正则找 `Cloud-IDE-JWT <jwt>`
///（对齐 Python：不做结构化解析，命中即写回）
fn scan_leveldb_dir(ctx: &ProxyCtx, ls: &Path) -> usize {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r"Cloud-IDE-JWT [A-Za-z0-9_\-\.=]+").expect("leveldb jwt regex")
    });
    let mut found = 0usize;
    let Ok(entries) = std::fs::read_dir(ls) else { return 0 };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".ldb") || name.ends_with(".log")) {
            continue;
        }
        let Ok(data) = std::fs::read(entry.path()) else { continue };
        for m in re.find_iter(&data) {
            let hit = String::from_utf8_lossy(m.as_bytes()).to_string();
            let Some(uid) = extract_user_id(&hit) else { continue };
            let status = update_account_jwt(ctx, &uid, &hit);
            ctx.log.log(&format!("[local] 命中 leveldb {name} -> {status}"));
            found += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// find_cloud_ide_jwt：显式前缀优先（无效即止）+ 通用 JWT 校验回退
    #[test]
    fn find_jwt_prefix_and_generic() {
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256"}));
        let payload = b64(&serde_json::json!({"data": {"id": "u1"}}));
        let jwt = format!("{header}.{payload}.sig");
        // 显式前缀
        let hit = find_cloud_ide_jwt(&format!("blob Cloud-IDE-JWT {jwt} tail")).unwrap();
        assert_eq!(hit, format!("Cloud-IDE-JWT {jwt}"));
        // 通用扫描（无前缀）
        let hit2 = find_cloud_ide_jwt(&format!("x {jwt} y")).unwrap();
        assert_eq!(hit2, format!("Cloud-IDE-JWT {jwt}"));
        // 前缀命中但无效（HS256）→ 直接 None，不落入通用扫描
        let h2 = b64(&serde_json::json!({"alg": "HS256"}));
        let bad = format!("Cloud-IDE-JWT {h2}.{payload}.sig");
        assert!(find_cloud_ide_jwt(&bad).is_none());
        // 空串
        assert!(find_cloud_ide_jwt("").is_none());
    }
}
