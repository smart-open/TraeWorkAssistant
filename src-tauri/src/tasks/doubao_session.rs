//! 豆包会话续期巡检（原 src-python/doubao_renew.py 的 Rust 移植，逐函数对齐）。
//!
//! 实测结论（2026-09-08，Doubao Chromium 147）：桌面客户端的 cookie 值在 Chromium os_crypt
//! 之下还有一层客户端级加密——v10/DPAPI + AES-256-GCM 解出的明文仍为二进制密文（非 ASCII），
//! 无法离线得到明文 sessionid。因此：
//! - 续期主路径 = PS 桥 KeepAlive（启动豆包让客户端自己联网滑动续期）；
//! - 巡检对池内**明文 sessionid 凭证**生效（手动录入 / 代理抓包自动回写，两者同为明文）；
//! - cookie 解密能力保留为**诊断**用途（校验 User Data / 快照 Local State+Cookies 是否完整可解）。
//!
//! 两段式探活（修复「死会话被洗白」）：
//!   ① 先用已登录 JSON 端点 api_probe(DEFAULT_PROBE_URL) 权威判定会话有效性
//!     （info/v2/ 对任意 sid 一律 200+SPA HTML，200 恒真不可用于判定，实测 2026-09-09）；
//!   ② 判定 ok 后才调 renew_probe(renew_url) 保活并抓取 Set-Cookie 回写；
//!     判定 expired 置 expired=True；error 不改状态。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::state::AppState;

/// 轻量保活端点（必须登录）：200=有效 / 302→passport=过期；代理日志实测确认
pub const DEFAULT_RENEW_URL: &str = "https://www.doubao.com/info/v2/";
/// POST 已登录 JSON 端点：code=0 有效 / code=710012001 登录态失效（doubao_quota 同款语义）
pub const DEFAULT_PROBE_URL: &str =
    "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/";
const SESSION_EXPIRED_CODE: i64 = 710012001;
const TARGET_COOKIES: [&str; 5] = ["sessionid", "sessionid_ss", "sid_tt", "uid_tt", "sid_guard"];
const TIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

fn now_str() -> String {
    chrono::Local::now().format(TIME_FMT).to_string()
}

// ── DPAPI / AES-GCM 解密（诊断用途）────────────────────────────────────────

#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    crate::vault::dpapi::unprotect(data)
}
#[cfg(not(windows))]
fn dpapi_unprotect(_data: &[u8]) -> Result<Vec<u8>, String> {
    Err("macOS 暂不支持 cookie 直读（客户端走 Keychain Safe Storage，属重写范畴）；请使用 MITM 捕获".into())
}

/// Local State → os_crypt.encrypted_key（base64，DPAPI 包裹）→ AES-256 密钥
#[cfg(windows)]
fn load_aes_key(user_data: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let ls_path = user_data.join("Local State");
    let raw = std::fs::read_to_string(&ls_path)
        .map_err(|e| format!("Local State 读取失败: {e}"))?;
    let ls: Value = serde_json::from_str(&raw).map_err(|e| format!("Local State 解析失败: {e}"))?;
    let enc_key = ls
        .get("os_crypt")
        .and_then(|v| v.get("encrypted_key"))
        .and_then(Value::as_str)
        .ok_or("Local State 无 os_crypt.encrypted_key")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(enc_key)
        .map_err(|e| format!("encrypted_key base64 解码失败: {e}"))?;
    if decoded.len() < 5 || &decoded[..5] != b"DPAPI" {
        return Err("encrypted_key 前缀非 DPAPI，布局可能已变化".into());
    }
    dpapi_unprotect(&decoded[5..])
}

/// v10 布局：'v10' + nonce(12) + ciphertext+tag(16)。v20/app-bound 直接放弃（不攻击）
#[cfg(windows)]
fn decrypt_cookie_blob(blob: &[u8], key: &[u8]) -> Option<String> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    if blob.len() < 19 || &blob[..3] != b"v10" {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let nonce = Nonce::from_slice(&blob[3..15]);
    let plain = cipher.decrypt(nonce, &blob[15..]).ok()?;
    Some(String::from_utf8_lossy(&plain).to_string())
}

pub(crate) fn chromium_profiles(user_data: &Path) -> Vec<PathBuf> {
    /// Default + Profile N 目录（豆包自带账号隔离，登录会话可能位于任意 Profile）
    fn is_profile_dir(p: &Path) -> bool {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        (name == "Default" || name.starts_with("Profile ")) && p.is_dir()
    }
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| is_profile_dir(p)).collect();
    out.sort();
    out
}

/// 读 Local State → profile.last_used（客户端当前活跃 Profile 目录名）
pub(crate) fn read_active_profile_name(user_data: &Path) -> Option<String> {
    let ls = user_data.join("Local State");
    let raw = std::fs::read_to_string(ls).ok()?;
    let data: Value = serde_json::from_str(&raw).ok()?;
    let name = data
        .get("profile")
        .and_then(|p| p.get("last_used"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// 复制 Cookies 库到临时目录后读取（规避客户端运行时文件锁）；返回临时目录路径
fn copy_cookies_db(prof: &Path) -> Result<(PathBuf, PathBuf), String> {
    let mut db = prof.join("Network").join("Cookies");
    if !db.exists() {
        db = prof.join("Cookies"); // 旧布局兜底
        if !db.exists() {
            return Err("no cookies file".into());
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let td = std::env::temp_dir().join(format!("aw_ck_{}_{nanos}", std::process::id()));
    std::fs::create_dir_all(&td).map_err(|e| format!("临时目录创建失败: {e}"))?;
    let tmp_db = td.join("Cookies");
    // 客户端运行中会独占 Cookies 锁（os error 32）：短重试两次，仍失败返回明确的
    // 「运行中占用」语义（上层按此静默跳过，不再当作错误刷屏）
    let mut copied = Err("unreached".into());
    for _ in 0..3 {
        copied = std::fs::copy(&db, &tmp_db).map(|_| ()).map_err(|e| {
            if e.to_string().contains("32") {
                "Cookies 被豆包客户端运行占用，跳过本轮探测".to_string()
            } else {
                format!("Cookies 复制失败: {e}")
            }
        });
        if copied.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    copied?;
    // -wal/-shm 一并复制，尽量避免读到未 checkpoint 的空库
    for suffix in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("Cookies{suffix}"));
        if side.exists() {
            let _ = std::fs::copy(&side, td.join(format!("Cookies{suffix}")));
        }
    }
    Ok((tmp_db, td))
}

/// 读取并解密单个 Profile 的 doubao.com 域目标 cookie，返回 {name: value}
#[cfg(windows)]
fn read_profile_target_cookies(prof: &Path, key: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok((tmp_db, td)) = copy_cookies_db(prof) else {
        return out;
    };
    let opened = rusqlite::Connection::open_with_flags(
        &tmp_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .and_then(|conn| {
        let mut stmt =
            conn.prepare("SELECT name, encrypted_value FROM cookies WHERE host_key LIKE '%doubao.com'")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
            ))
        })?;
        let mut out = HashMap::new();
        for r in rows.flatten() {
            let (name, enc) = r;
            if !TARGET_COOKIES.contains(&name.as_str()) {
                continue;
            }
            if let Some(val) = decrypt_cookie_blob(&enc, key) {
                if !val.is_empty() {
                    out.insert(name, val);
                }
            }
        }
        Ok(out)
    });
    let _ = std::fs::remove_dir_all(&td);
    out.extend(opened.unwrap_or_default());
    out
}

/// 读取并解密一个 User Data 根下的 doubao.com 域目标 cookie。多 Profile：活跃 Profile
/// 优先取值，其余按目录序兜底补缺（不覆盖高优先级已取到的键）
#[cfg(windows)]
fn read_doubao_cookies(user_data: &Path) -> Result<HashMap<String, String>, String> {
    let mut profiles = chromium_profiles(user_data);
    if profiles.is_empty() {
        return Ok(HashMap::new());
    }
    if let Some(active) = read_active_profile_name(user_data) {
        // 活跃排最前（稳定排序：仅调整相对顺序，其余保持目录序）
        profiles.sort_by_key(|p| p.file_name().and_then(|n| n.to_str()) != Some(active.as_str()));
    }
    let key = load_aes_key(user_data)?;
    let mut out = HashMap::new();
    for prof in &profiles {
        for (name, val) in read_profile_target_cookies(prof, &key) {
            out.entry(name).or_insert(val);
        }
    }
    Ok(out)
}

/// 读 Local State → saman.local_storage_app_for_web.enterprise 内嵌的 x-tt-multi-sids
/// （uid → 明文 sessionid 映射）。客户端把当前全部账号的会话 sid 明文缓存在这里——
/// Cookies 解出的是客户端级二次加密密文（不可用），此处是唯一可离线验证的会话来源。
fn read_multi_sids(user_data: &Path) -> HashMap<String, String> {
    let ls = user_data.join("Local State");
    let Ok(raw) = std::fs::read_to_string(ls) else {
        return HashMap::new();
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let ent = data
        .get("saman")
        .and_then(|s| s.get("local_storage_app_for_web"))
        .and_then(|s| s.get("enterprise"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut out = HashMap::new();
    if let Some(i) = ent.find("x-tt-multi-sids") {
        // 对齐 python：跳过 18 字节（15 字符键 + `":"`），取至下一引号
        let after = &ent[(i + "x-tt-multi-sids\":\"".len()).min(ent.len())..];
        let val = after.split('"').next().unwrap_or("");
        let decoded = urlencoding::decode(val).unwrap_or_default();
        for pair in decoded.split('|') {
            if let Some((k, v)) = pair.split_once(':') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() {
                    out.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    out
}

// ── 会话有效性探测（切换前预检）────────────────────────────────────────────

/// 探活专用 agent：15s 超时 + 不跟随重定向（30x 交由调用方判定是否跳 passport）+ 直连
fn probe_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .redirects(0)
        .build()
}

fn auth_headers(sid: &str) -> String {
    format!("sessionid={sid}; sessionid_ss={sid}")
}

/// 携带会话 POST 已登录端点，返回 (status, detail)，status ∈ ok|expired|error|unknown
fn api_probe(agent: &ureq::Agent, sid: &str, url: &str) -> (String, String) {
    let body = json!({"product_line": "membership"}).to_string();
    let resp = agent
        .post(url)
        .set("Cookie", &auth_headers(sid))
        .set(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AIWorkAssistant/1.0",
        )
        .set("Referer", "https://www.doubao.com/")
        .set("Accept", "application/json, text/plain, */*")
        .set("Content-Type", "application/json")
        .send_string(&body);
    match resp {
        Ok(r) => {
            let text = r.into_string().unwrap_or_default();
            let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if !data.is_object() {
                return ("error".into(), "响应非 JSON 对象".into());
            }
            match data.get("code") {
                None | Some(Value::Null) => ("ok".into(), "code=0".into()),
                Some(c) if c.as_i64() == Some(0) => ("ok".into(), "code=0".into()),
                Some(c) if c.as_i64() == Some(SESSION_EXPIRED_CODE) => (
                    "expired".into(),
                    "code=710012001（登录态失效，会话已被服务端吊销或过期）".into(),
                ),
                Some(c) => ("unknown".into(), format!("code={c}（业务拒绝，无法判定会话状态）")),
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let loc = r.header("Location").unwrap_or("").to_string();
            if (301..=308).contains(&code) && loc.to_lowercase().contains("passport") {
                ("expired".into(), format!("302 → {}", truncate(&loc, 120)))
            } else if code == 401 {
                ("expired".into(), "HTTP 401".into())
            } else {
                ("error".into(), format!("HTTP {code} {}", truncate(&loc, 80)))
            }
        }
        Err(e) => ("error".into(), truncate(&e.to_string(), 160)),
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 携带 cookie GET 保活端点。返回 (status, new_cookies, detail)，status ∈ ok|expired|error
fn renew_probe(agent: &ureq::Agent, sid: &str, url: &str) -> (String, HashMap<String, String>, String) {
    let resp = agent
        .get(url)
        .set("Cookie", &auth_headers(sid))
        .set(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AIWorkAssistant/1.0",
        )
        .call();
    match resp {
        Ok(r) => {
            // 滑动续期：响应可能 Set-Cookie 下发新 sessionid/sid_guard
            let mut new_cookies = HashMap::new();
            for raw in r.all("set-cookie") {
                for pair in raw.split(';') {
                    if let Some((k, v)) = pair.trim().split_once('=') {
                        if TARGET_COOKIES.contains(&k) && !v.is_empty() {
                            new_cookies.insert(k.to_string(), v.to_string());
                        }
                    }
                }
            }
            // 某些网关对未登录也返回 200 + 登录页，用 Location/页面特征兜底不判定，视为有效
            ("ok".into(), new_cookies, format!("HTTP {}", r.status()))
        }
        Err(ureq::Error::Status(code, r)) => {
            let loc = r.header("Location").unwrap_or("").to_string();
            if (301..=308).contains(&code) && loc.to_lowercase().contains("passport") {
                ("expired".into(), HashMap::new(), format!("302 → {}", truncate(&loc, 120)))
            } else if code == 401 {
                ("expired".into(), HashMap::new(), "HTTP 401".into())
            } else {
                ("error".into(), HashMap::new(), format!("HTTP {code} {}", truncate(&loc, 80)))
            }
        }
        Err(e) => ("error".into(), HashMap::new(), truncate(&e.to_string(), 160)),
    }
}

/// sid_guard 格式：'<sid>|<create_ts 秒>|<duration 秒>|...' → 到期时间字符串。
/// 池内存储保持 Set-Cookie 下发的 URL 编码原样，解析前先解码
fn parse_sid_guard(value: &str) -> Option<String> {
    let value = if value.contains('%') {
        urlencoding::decode(value).unwrap_or_default().to_string()
    } else {
        value.to_string()
    };
    let parts: Vec<&str> = value.split('|').collect();
    if parts.len() < 3 {
        return None;
    }
    let create_ts: i64 = parts[1].trim().parse().ok()?;
    let duration: i64 = parts[2].trim().parse().ok()?;
    if create_ts <= 0 || duration <= 0 {
        return None;
    }
    use chrono::TimeZone;
    let dt = chrono::Local
        .timestamp_opt(create_ts + duration, 0)
        .single()?;
    Some(dt.format(TIME_FMT).to_string())
}

/// 切换/一键打开前预检：目标槽位存储的会话在服务端是否仍有效。
/// 返回 {status: ok|expired|unknown, detail, source}（JSON 契约对齐 python stdout 单行）
pub fn probe_slot_session(user_data: &Path, uid: &str, url: &str) -> Value {
    let agent = probe_agent();
    let sid = if uid.is_empty() { String::new() } else { read_multi_sids(user_data).get(uid).cloned().unwrap_or_default() };
    let mut source = "local_state_multi_sids";
    let mut sid = sid;
    if sid.is_empty() {
        // 兜底：cookie 解密（仅 ASCII 明文可用；密文为客户端级二次加密，不可验证）
        #[cfg(windows)]
        if let Ok(cookies) = read_doubao_cookies(user_data) {
            let cand = cookies
                .get("sessionid")
                .or_else(|| cookies.get("sessionid_ss"))
                .cloned()
                .unwrap_or_default();
            if cand.is_ascii() {
                sid = cand;
                source = "cookie_decrypt";
            }
        }
    }
    if sid.is_empty() {
        return json!({
            "status": "unknown",
            "detail": "槽位无可验证会话凭证（multi-sids 无该 uid 且无明文 cookie）",
            "source": source,
        });
    }
    let (status, detail) = api_probe(&agent, &sid, url);
    json!({"status": status, "detail": detail, "source": source})
}

// ── 续期巡检（网络）────────────────────────────────────────────────────────

/// 诊断模式：解密当前 User Data + 各快照槽，报告可解性与明文特征（不写池）。
/// 实测：桌面客户端 cookie 值为客户端级二次加密的密文（解出非 ASCII），不能当 sessionid 用
fn sync_cookie_state(state: &AppState, logs: &mut Vec<String>) -> Value {
    let mut sources: Vec<Value> = Vec::new();
    let diagnose = |label: &str, ud: &Path, sources: &mut Vec<Value>, logs: &mut Vec<String>| {
        #[cfg(windows)]
        let res = read_doubao_cookies(ud).map(|c| {
            let ascii_n = c.values().filter(|v| v.is_ascii()).count();
            (c.len(), Some(ascii_n), c.keys().cloned().collect::<Vec<_>>())
        });
        #[cfg(not(windows))]
        let res: Result<(usize, Option<usize>, Vec<String>), String> =
            Err("macOS 暂不支持 cookie 直读，请使用 MITM 捕获".into());
        match res {
            Err(e) => {
                logs.push(format!("{label} 解密失败：{e}"));
                sources.push(json!({"source": label, "decryptable": false, "detail": truncate(&e, 120)}));
            }
            Ok((0, _, _)) => {
                logs.push(format!("{label} 未解出目标 cookie（豆包可能未登录或布局变化）"));
                sources.push(json!({"source": label, "decryptable": false, "detail": "no cookies"}));
            }
            Ok((n, ascii_n, keys)) => {
                let ascii_n = ascii_n.unwrap_or(0);
                sources.push(json!({
                    "source": label, "decryptable": true, "cookies": keys, "ascii_values": ascii_n,
                    "note": if ascii_n > 0 { "ascii_values>0 的值可作为明文凭证" } else { "密文（客户端级二次加密），不可作为凭证" },
                }));
                logs.push(format!("{label} 解出 {n} 项（ASCII 明文 {ascii_n} 项）"));
            }
        }
    };

    // 基根：Windows=%LOCALAPPDATA%，mac=Application Support（豆包桌面端 Chromium 布局）
    let ud = crate::platform::local_data_root_lossy()
        .join("Doubao")
        .join("User Data");
    if ud.exists() {
        diagnose("live", &ud, &mut sources, logs);
    }
    let profiles_root = state.data_dir.join("data").join("profiles_doubao");
    if profiles_root.exists() {
        let mut slots: Vec<PathBuf> = std::fs::read_dir(&profiles_root)
            .map(|it| {
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()) != Some("last"))
                    .collect()
            })
            .unwrap_or_default();
        slots.sort();
        for slot in slots {
            let name = slot
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            diagnose(&format!("snapshot:{name}"), &slot, &mut sources, logs);
        }
    }
    json!({"synced": 0, "sources": sources, "cryptography": true})
}

/// 对池内有明文 sessionid 的账号做两段式探活续期（两段式语义见模块头注释）。
/// 返回 {ok, expired, error, skipped, accounts:[{user_id,status,detail,renewed?}]}
/// probe_url 参数化供测试注入不可达端点（离线确定性，不依赖真实网络环境）。
fn run_renewal(state: &AppState, renew_url: &str, probe_url: &str) -> Value {
    // SQLite 化（P3）：doubao_accounts 表（Value 形态沿用原 JSON 处理逻辑）
    let mut pool: Value =
        serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
    if !pool.is_object() {
        pool = json!({"accounts": []});
    }
    let accounts = pool
        .get_mut("accounts")
        .and_then(Value::as_array_mut)
        .cloned()
        .unwrap_or_default();
    let agent = probe_agent();
    let mut results: Vec<Value> = Vec::new();
    // 回填用的完整账号对象（含 skipped 与未变更字段）：禁止用 results 摘要替换账号池
    //（审查 #1：曾用摘要整体回写导致 session_id/sid_guard 等全字段丢失、skipped 账号被删）
    let mut updated: Vec<Value> = Vec::new();
    let (mut ok_n, mut expired_n, mut error_n, mut skipped_n) = (0usize, 0usize, 0usize, 0usize);

    for mut acc in accounts {
        let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("").to_string();
        let sid = acc
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if sid.is_empty() {
            skipped_n += 1;
            updated.push(acc); // 无凭据账号原样保留在池中
            continue;
        }
        // ① 权威判定：已登录 JSON 端点（code=0 有效 / 710012001 失效）
        let (first_status, first_detail) = api_probe(&agent, &sid, probe_url);
        let (status, detail, new_cookies) = match first_status.as_str() {
            "ok" => {
                // ② 会话有效 → 保活请求抓续期 Cookie（renew_url/info/v2/ 仅作保活用途）
                let (st, nc, dt) = renew_probe(&agent, &sid, renew_url);
                match st.as_str() {
                    "ok" | "expired" => (st, dt, nc),
                    // 保活请求网络失败：会话刚被权威端点判定有效，不因保活失败改状态，
                    // 仅跳过 Cookie 回写（status=ok、无 new_cookies）
                    _ => (
                        "ok".into(),
                        format!("probe ok; renew: {dt}"),
                        HashMap::new(),
                    ),
                }
            }
            "expired" => ("expired".into(), first_detail, HashMap::new()),
            // error / unknown：无法判定会话状态，不改状态（不洗白也不误杀）
            _ => ("error".into(), first_detail, HashMap::new()),
        };

        let mut entry = json!({"user_id": uid, "status": status, "detail": detail});
        if let Some(acc_obj) = acc.as_object_mut() {
            match status.as_str() {
                "ok" => {
                    ok_n += 1;
                    acc_obj.insert("expired".into(), json!(false));
                    acc_obj.insert("last_renew_at".into(), json!(now_str()));
                    if let Some(new_sid) = new_cookies.get("sessionid") {
                        acc_obj.insert("session_id".into(), json!(new_sid));
                        entry["renewed"] = json!(true);
                    }
                    if let Some(guard) = new_cookies.get("sid_guard") {
                        acc_obj.insert("sid_guard".into(), json!(guard));
                        acc_obj.insert("session_expire_at".into(), json!(parse_sid_guard(guard)));
                    }
                }
                "expired" => {
                    expired_n += 1;
                    acc_obj.insert("expired".into(), json!(true));
                    acc_obj.insert("last_renew_at".into(), json!(now_str()));
                }
                _ => {
                    error_n += 1; // 网络错误不改 expired 状态
                }
            }
        }
        results.push(entry);
        updated.push(acc); // 完整账号对象（含本轮新 session_id/sid_guard 等）回填
    }
    // 仅在拿到结果时回写（python 同款；空池不触发写盘）。
    // 回填 updated（完整账号对象）而非 results（摘要），字段零丢失（对照 doubao_quota::run_batch）
    if !results.is_empty() {
        pool["accounts"] = Value::Array(updated);
        if let Ok(file) = serde_json::from_value::<crate::commands::doubao::DoubaoAccountPool>(pool) {
            let _ = crate::commands::doubao::save_pool(state, &file);
        }
    }
    json!({
        "ok": ok_n, "expired": expired_n, "error": error_n, "skipped": skipped_n,
        "accounts": results,
    })
}

// ── 主入口 ─────────────────────────────────────────────────────────────────

/// 续期巡检主入口（对齐 python main）：诊断 + （可选）网络续期巡检，
/// 结果写 data/doubao_renew_result.json，返回 summary JSON（前端/CLI 消费契约同款）。
pub fn run(state: &AppState, sync_only: bool, url_override: Option<&str>) -> Result<Value, String> {
    let mut logs: Vec<String> = Vec::new();
    // 保活端点：命令行 > settings.doubao_renew_url > 默认首页
    let url = url_override
        .map(str::to_string)
        .or_else(|| {
            let s = state.settings().doubao_renew_url;
            s.filter(|u| !u.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_RENEW_URL.to_string());

    let sync_info = sync_cookie_state(state, &mut logs);
    let summary = if sync_only {
        json!({
            "mode": "diagnose", "finished_at": now_str(), "sync": sync_info, "logs": logs,
        })
    } else {
        let renew_info = run_renewal(state, &url, DEFAULT_PROBE_URL);
        json!({
            "mode": "full", "finished_at": now_str(), "renew_url": url,
            "sync": sync_info,
            "renew": {
                "ok": renew_info["ok"], "expired": renew_info["expired"],
                "error": renew_info["error"], "skipped": renew_info["skipped"],
            },
            "accounts": renew_info["accounts"],
            "logs": logs,
        })
    };
    // SQLite 化（P2）：doubao_renew_result.json → kv `doubao_renew_result`
    crate::store::db(&state.data_dir).kv_set("doubao_renew_result", &summary)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("aiwork_doubao_renew_test_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }

    /// 回归（审查 #1）：续期回写必须保留账号全字段与 skipped 账号，
    /// 即使全部走 error 路径（不可达端点）也不得把账号池替换成摘要行。
    #[test]
    fn renewal_writeback_preserves_account_fields() {
        let state = temp_state("fields");
        // SQLite 化（P3）：种子走 doubao_accounts 表
        let seed: crate::commands::doubao::DoubaoAccountPool = serde_json::from_value(json!({
            "accounts":[
                {"user_id":"1001","name":"甲","session_id":"sid-AAA","ttwid":"tw1","sid_guard":"g1"},
                {"user_id":"1002","name":"乙","session_id":"sid-BBB"},
                {"user_id":"1003","name":"丙（无凭据）"}
            ]}))
        .unwrap();
        crate::commands::doubao::save_pool(&state, &seed).unwrap();
        // 双端点均不可达 → 带凭据账号确定性走 error（不依赖真实网络，离线可复现）
        let summary = run_renewal(&state, "http://127.0.0.1:1/renew", "http://127.0.0.1:1/probe");
        assert_eq!(summary["error"].as_u64(), Some(2), "两个带 sid 账号应走 error");
        assert_eq!(summary["skipped"].as_u64(), Some(1));

        let pool: Value = serde_json::to_value(crate::commands::doubao::load_pool(&state)).unwrap();
        let accounts = pool["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 3, "skipped 账号不得被删除");
        let by_uid = |u: &str| accounts.iter().find(|a| a["user_id"] == u).unwrap().clone();
        // 完整字段保留（修复前：此处只剩 {user_id,status,detail}）
        let a1 = by_uid("1001");
        assert_eq!(a1["session_id"], "sid-AAA");
        assert_eq!(a1["name"], "甲");
        assert_eq!(a1["ttwid"], "tw1");
        assert_eq!(a1["sid_guard"], "g1");
        let a3 = by_uid("1003");
        assert_eq!(a3["name"], "丙（无凭据）", "无凭据账号原样保留");
        // summary 侧仍输出摘要（前端契约不变）
        assert_eq!(summary["accounts"].as_array().unwrap().len(), 2);
    }
}
