//! 豆包对话/会话辅助（原 src-python/doubao_chats.py 的 Rust 移植）。
//!
//! 应用侧消费的三个能力（其余 --scan/--dump-* 为 IndexedDB 侦察 CLI，应用未调用，不移植）：
//! - `check_login_cookie`：读 Chromium Cookies 库（复制到临时目录规避客户端运行锁），
//!   判定 sessionid/sid_guard 存在性与剩余有效期——cookie **名**为明文（值才加密），
//!   无需解密即可判定登录态；活跃 Profile（Local State → profile.last_used）优先，
//!   无 Local State 时回退聚合语义（任一 Profile 有会话 / 最小剩余）；
//! - `detect_uid`：读客户端 Local Storage leveldb 的 client_device_info.userId（客户端
//!   每次启动自写，不依赖代理）——自包含 leveldb 读取器（WAL 32KB 块逐记录重组 +
//!   SST snappy 解压，跳过损坏记录，容错语义对齐 python），并与代理抓包文件按
//!   时间戳比新鲜度取新者；最后兜底 Local State info_cache 的 saman.user_id；
//! - `export_account`：走官方 IM API（recent_conv 会话列表 + chain/single 消息），
//!   导出 markdown + json 到 data/exports/。需要账号池内已有明文 sessionid 凭证。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::state::AppState;

/// IM API 网关 UA（缺客户端标识会被拒）
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.27.12";
/// 会话列表查询串（设备指纹参数缺 web_id/tea_uuid/fp 会被网关判 712010702，实测对照确认）
const RECENT_QS: &str = "version_code=20800&language=zh&device_platform=web&doubao_device_platform=desktop\
&aid=582478&real_aid=582478&pkg_type=release_version&device_id=199439841787403\
&pc_version=2.27.12&doubao_pc_version=2.27.12&region=CN&sys_region=CN&samantha_web=1\
&web_platform=desktop&use-olympus-account=1&runtime=web&runtime_version=3.35.4\
&client_platform=pc_client&chromium_version=147.0.7727.149&channel=win\
&web_id=7681679574816589346&tea_uuid=199439841787403&fp=verify_199439841787403\
&web_tab_id=4db30b53-185f-4e73-99aa-6d29921381fc";
/// 单会话消息查询串（RECENT_QS 替换 aid/web_platform，与 python 同款）
const SINGLE_QS: &str = "version_code=20800&language=zh&device_platform=web&doubao_device_platform=web\
&aid=497858&real_aid=497858&pkg_type=release_version&device_id=199439841787403\
&pc_version=2.27.12&doubao_pc_version=2.27.12&region=CN&sys_region=CN&samantha_web=1\
&web_platform=web&use-olympus-account=1&runtime=web&runtime_version=3.35.4\
&client_platform=pc_client&chromium_version=147.0.7727.149&channel=win\
&web_id=7681679574816589346&tea_uuid=199439841787403&fp=verify_199439841787403\
&web_tab_id=4db30b53-185f-4e73-99aa-6d29921381fc";

const TIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

fn now_str() -> String {
    chrono::Local::now().format(TIME_FMT).to_string()
}

// ── leveldb 读取器（自包含，零第三方依赖；容错语义对齐 python）──────────────

fn read_varint(b: &[u8], mut i: usize) -> Result<(u64, usize), String> {
    let mut r: u64 = 0;
    let mut s: u32 = 0;
    loop {
        let c = *b.get(i).ok_or("varint out of range")?;
        i += 1;
        r |= ((c & 0x7F) as u64) << s;
        if c & 0x80 == 0 {
            return Ok((r, i));
        }
        s += 7;
    }
}

/// 纯 python 移植的 snappy 块解压（leveldb 数据块专用，覆盖 literal/copy 三类 tag）
fn snappy_decompress(src: &[u8]) -> Result<Vec<u8>, String> {
    let mut i = 0usize;
    // 首部 varint = 解压后长度（仅用于容量预估，不解码校验）
    let mut ulen: u64 = 0;
    let mut shift = 0u32;
    loop {
        let c = *src.get(i).ok_or("snappy truncated")?;
        i += 1;
        ulen |= ((c & 0x7F) as u64) << shift;
        shift += 7;
        if c & 0x80 == 0 {
            break;
        }
    }
    let mut out: Vec<u8> = Vec::with_capacity(ulen as usize);
    let n = src.len();
    while i < n {
        let t = src[i];
        i += 1;
        let tag = t & 3;
        if tag == 0 {
            // literal
            let mut ln = (t >> 2) as usize + 1;
            if ln > 60 {
                let nb = ln - 60;
                if i + nb > n {
                    return Err("snappy truncated".into());
                }
                let mut v: usize = 0;
                for k in 0..nb {
                    v |= (src[i + k] as usize) << (8 * k);
                }
                i += nb;
                ln = v + 1;
            }
            if i + ln > n {
                return Err("snappy truncated".into());
            }
            out.extend_from_slice(&src[i..i + ln]);
            i += ln;
        } else {
            // copy（1 字节/2 字节/4 字节偏移）
            let ln: usize;
            let off: usize;
            if tag == 1 {
                if i >= n {
                    return Err("snappy truncated".into());
                }
                ln = (((t >> 2) & 7) as usize) + 4;
                off = (((t >> 5) as usize) << 8) | (src[i] as usize);
                i += 1;
            } else if tag == 2 {
                if i + 2 > n {
                    return Err("snappy truncated".into());
                }
                ln = (t >> 2) as usize + 1;
                off = u16::from_le_bytes([src[i], src[i + 1]]) as usize;
                i += 2;
            } else {
                if i + 4 > n {
                    return Err("snappy truncated".into());
                }
                ln = (t >> 2) as usize + 1;
                off = u32::from_le_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]) as usize;
                i += 4;
            }
            if off == 0 || off > out.len() {
                return Err("snappy bad offset".into());
            }
            for _ in 0..ln {
                let b = out[out.len() - off];
                out.push(b);
            }
        }
    }
    Ok(out)
}

/// WAL（.log）：32KB 块 + [len2][type1] 记录（python 未校验 CRC，移植保持一致）；
/// type 1=full 2=first 3=middle 4=last → 重组为 WriteBatch 序列
fn read_wal_batches(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut pos = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut batches = Vec::new();
    while pos + 7 <= raw.len() {
        let off = pos % 32768;
        if off + 7 > 32768 {
            // 记录头不跨块
            pos += 32768 - off;
            continue;
        }
        let length = u16::from_le_bytes([raw[pos + 4], raw[pos + 5]]) as usize;
        let rtype = raw[pos + 6];
        let end = (pos + 7 + length).min(raw.len());
        let payload = &raw[pos + 7..end];
        pos += 7 + length;
        match rtype {
            1 | 4 => {
                buf.extend_from_slice(payload);
                batches.push(std::mem::take(&mut buf));
            }
            2 | 3 => buf.extend_from_slice(payload),
            _ => {}
        }
    }
    batches
}

/// SST（.ldb）→ [(seq, type, user_key, value)]；value 为空 = 删除
fn read_sst_entries(raw: &[u8]) -> Result<Vec<(u64, u8, Vec<u8>, Vec<u8>)>, String> {
    let n = raw.len();
    if n < 48 {
        return Err("sst too small".into());
    }
    // footer（48 字节内 4 个 varint）：meta_off/meta_size/index_off/index_size
    let i = n - 48;
    let (_off, i2) = read_varint(raw, i)?;
    let (_size, i3) = read_varint(raw, i2)?;
    let (off2, i4) = read_varint(raw, i3)?;
    let (size2, _) = read_varint(raw, i4)?;
    let idx = raw
        .get(off2 as usize..(off2 as usize).saturating_add(size2 as usize))
        .ok_or("index block out of range")?;
    if idx.len() < 4 {
        return Err("index block too small".into());
    }
    let idx_tail = &idx[idx.len() - 4..];
    let n_rs = u32::from_le_bytes(idx_tail.try_into().unwrap()) as usize;
    let end = idx.len().saturating_sub(4 + n_rs * 4);

    // index block → 各数据块 (offset, size)
    let mut j = 0usize;
    let mut last: Vec<u8> = Vec::new();
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    while j < end {
        let (sh, j2) = read_varint(idx, j)?;
        let (ns, j3) = read_varint(idx, j2)?;
        let (vl, j4) = read_varint(idx, j3)?;
        let (sh, ns, vl) = (sh as usize, ns as usize, vl as usize);
        if j4 + ns + vl > idx.len() {
            return Err("index entry out of range".into());
        }
        let mut key = last[..sh.min(last.len())].to_vec();
        key.extend_from_slice(&idx[j4..j4 + ns]);
        let val = &idx[j4 + ns..j4 + ns + vl];
        let (ob, sb) = {
            let (ov, o1) = read_varint(val, 0)?;
            let (sv, _) = read_varint(val, o1)?;
            (ov as usize, sv as usize)
        };
        blocks.push((ob, sb));
        j = j4 + ns + vl;
        last = key;
    }

    // 逐数据块解出 entries
    let mut out = Vec::new();
    for (ob, sb) in blocks {
        if ob + sb >= n {
            return Err("data block out of range".into());
        }
        let comp = raw[ob + sb]; // 块尾 1 字节压缩标记
        let blk: Vec<u8> = if comp == 1 {
            snappy_decompress(&raw[ob..ob + sb])?
        } else {
            raw[ob..ob + sb].to_vec()
        };
        if blk.len() < 4 {
            return Err("data block too small".into());
        }
        let tail = &blk[blk.len() - 4..];
        let n_rs2 = u32::from_le_bytes(tail.try_into().unwrap()) as usize;
        let end2 = blk.len().saturating_sub(4 + n_rs2 * 4);
        let mut j = 0usize;
        let mut last: Vec<u8> = Vec::new();
        while j < end2 {
            let (sh, j2) = read_varint(&blk, j)?;
            let (ns, j3) = read_varint(&blk, j2)?;
            let (vl, j4) = read_varint(&blk, j3)?;
            let (sh, ns, vl) = (sh as usize, ns as usize, vl as usize);
            if j4 + ns + vl > blk.len() {
                return Err("entry out of range".into());
            }
            let mut key = last[..sh.min(last.len())].to_vec();
            key.extend_from_slice(&blk[j4..j4 + ns]);
            let val = blk[j4 + ns..j4 + ns + vl].to_vec();
            j = j4 + ns + vl;
            if key.len() < 8 {
                return Err("key too small".into());
            }
            let klen = key.len();
            let tag = u64::from_le_bytes(key[klen - 8..].try_into().unwrap());
            out.push((tag >> 8, (tag & 0xFF) as u8, key[..klen - 8].to_vec(), val));
            last = key;
        }
    }
    Ok(out)
}

fn put_entry(best: &mut HashMap<Vec<u8>, (u64, Vec<u8>)>, seq: u64, key: &[u8], val: Option<&[u8]>) {
    let newer = best.get(key).map_or(true, |(s, _)| seq > *s);
    if newer {
        best.insert(key.to_vec(), (seq, val.unwrap_or_default().to_vec()));
    }
}

fn sorted_files(db_dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(db_dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 读一个 leveldb 目录，返回 {key: (seq, value)}（按 seq 合并、已应用删除：value 空 = 墓碑）。
/// 单文件损坏/客户端运行锁读取失败 → 跳过该文件（对齐 python 容错语义）
pub(crate) fn read_leveldb_dir(db_dir: &Path) -> HashMap<Vec<u8>, (u64, Vec<u8>)> {
    let mut best: HashMap<Vec<u8>, (u64, Vec<u8>)> = HashMap::new();
    for fp in sorted_files(db_dir, "ldb") {
        if let Ok(raw) = std::fs::read(&fp) {
            if let Ok(entries) = read_sst_entries(&raw) {
                for (seq, t, key, val) in entries {
                    put_entry(&mut best, seq, &key, (t == 1).then_some(val.as_slice()));
                }
            }
        }
    }
    for fp in sorted_files(db_dir, "log") {
        let Ok(raw) = std::fs::read(&fp) else { continue };
        for batch in read_wal_batches(&raw) {
            if batch.len() < 12 {
                continue;
            }
            let seq0 = u64::from_le_bytes(batch[0..8].try_into().unwrap());
            let count = u32::from_le_bytes(batch[8..12].try_into().unwrap()) as u64;
            let mut i = 12usize;
            for n in 0..count {
                let Some(&t) = batch.get(i) else { break };
                i += 1;
                let Ok((klen, i2)) = read_varint(&batch, i) else { break };
                let klen = klen as usize;
                if i2 + klen > batch.len() {
                    break;
                }
                let key = &batch[i2..i2 + klen];
                i = i2 + klen;
                if t == 1 {
                    let Ok((vlen, i3)) = read_varint(&batch, i) else { break };
                    let vlen = vlen as usize;
                    if i3 + vlen > batch.len() {
                        break;
                    }
                    put_entry(&mut best, seq0 + n, key, Some(&batch[i3..i3 + vlen]));
                    i = i3 + vlen;
                } else {
                    put_entry(&mut best, seq0 + n, key, None);
                }
            }
        }
    }
    best
}

// ── detect_uid：Local Storage leveldb → client_device_info.userId ──────────

/// 客户端 Local Storage 当前 uid（优先活跃 Profile），与代理抓包文件比新鲜度。
/// 返回 {"user_id", "source": local_storage|captured|local_state, "launch_ts_ms"}
/// （JSON 契约对齐 python stdout 单行）
pub fn detect_uid(state: &AppState) -> Value {
    let mut result = json!({"user_id": null, "source": null, "launch_ts_ms": 0});
    // 基根：Windows=%LOCALAPPDATA%，mac=Application Support（豆包桌面端 Chromium 布局）
    let ud = crate::platform::local_data_root_lossy()
        .join("Doubao")
        .join("User Data");

    // 多 Profile：登录会话/client_device_info 写在活跃 Profile 的 leveldb 里
    //（活跃 = Local State → profile.last_used；无 last_used 时取 launch 最新）
    let active_name = super::doubao_session::read_active_profile_name(&ud);
    let mut names: Vec<String> = super::doubao_session::chromium_profiles(&ud)
        .into_iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    if let Some(a) = &active_name {
        // 活跃排最前；last_used 指向的目录可能尚不存在，读取时自然跳过
        if let Some(pos) = names.iter().position(|n| n == a) {
            let x = names.remove(pos);
            names.insert(0, x);
        } else {
            names.insert(0, a.clone());
        }
    }

    let mut best: Option<(i64, u64, String)> = None; // (launch_ms, seq, uid)
    for (idx, name) in names.iter().enumerate() {
        let ldb = ud.join(name).join("Local Storage").join("leveldb");
        if !ldb.is_dir() {
            continue;
        }
        let data = read_leveldb_dir(&ldb);
        for (k, (seq, v)) in &data {
            if !k.windows("client_device_info".len()).any(|w| w == b"client_device_info") {
                continue;
            }
            let s = if v.first() == Some(&1) {
                String::from_utf8_lossy(&v[1..]).into_owned()
            } else {
                String::from_utf8_lossy(v).into_owned()
            };
            let Some(pos) = s.find('{') else { continue };
            let Ok(j) = serde_json::from_str::<Value>(&s[pos..]) else { continue };
            let uid = j.get("userId").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if uid.len() < 10 || !uid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let launch = j
                .get("currentLaunchTime")
                .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
                .unwrap_or(0);
            let cand = (launch, *seq, uid.clone());
            let replace = match &best {
                None => true,
                // idx=0 即活跃 Profile：命中即用；否则比 launch_ms 取新
                Some(b) => (idx == 0 && b.2 != uid && active_name.is_some()) || cand.0 > b.0,
            };
            if replace {
                best = Some(cand);
            }
        }
        if best.is_some() && idx == 0 && active_name.is_some() {
            break; // 活跃 Profile 已给出 uid，无需再扫
        }
    }
    let ls_uid: Option<String> = best.as_ref().map(|b| b.2.clone());
    let ls_ts: f64 = best.as_ref().map(|b| b.0 as f64 / 1000.0).unwrap_or(0.0);

    // 抓包凭证新鲜度（代理开着时随流量秒级更新，通常更新；SQLite 化 P3 走 kv）
    let mut cap_uid: Option<String> = None;
    let mut cap_ts: f64 = 0.0;
    {
        let c: Value = crate::store::db(&state.data_dir).kv_get("doubao_captured_credentials");
        if !c.is_null() {
        let u = c.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let ts = c.get("captured_at").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if !u.is_empty() && u.bytes().all(|b| b.is_ascii_digit()) && !ts.is_empty() {
            match chrono::NaiveDateTime::parse_from_str(&ts, TIME_FMT) {
                Ok(ndt) => match ndt.and_local_timezone(chrono::Local).single() {
                    Some(dt) => {
                        cap_uid = Some(u);
                        cap_ts = dt.timestamp() as f64;
                    }
                    None => eprintln!("[detect-uid] 抓包时间解析歧义（忽略）: {ts}"),
                },
                Err(e) => eprintln!("[detect-uid] 抓包文件时间解析失败（忽略）: {e}"),
            }
        }
        }
    }

    if cap_uid.is_some() && cap_ts > ls_ts {
        result["user_id"] = json!(cap_uid);
        result["source"] = json!("captured");
        result["launch_ts_ms"] = json!(best.as_ref().map(|b| b.0).unwrap_or(0));
    } else if let Some(u) = &ls_uid {
        result["user_id"] = json!(u);
        result["source"] = json!("local_storage");
        result["launch_ts_ms"] = json!(best.as_ref().map(|b| b.0).unwrap_or(0));
    } else if let Some(u) = &cap_uid {
        result["user_id"] = json!(u);
        result["source"] = json!("captured");
    } else {
        // Local State info_cache 兜底（最低优先级）：部分客户端版本不再把 client_device_info
        // 写进 Local Storage leveldb，此时取活跃 Profile 的 saman.user_id（同 profile 重登不更新）
        let ls_state = ud.join("Local State");
        let v: Value = crate::fs_utils::read_json(&ls_state);
        if let Some(info) = v
            .get("profile")
            .and_then(|p| p.get("info_cache"))
            .and_then(Value::as_object)
        {
            let valid = |u: &str| u.len() >= 10 && u.bytes().all(|b| b.is_ascii_digit());
            let mut cand: Option<String> = None;
            if let Some(a) = &active_name {
                if let Some(e) = info.get(a) {
                    let u = e
                        .get("saman")
                        .and_then(|s| s.get("user_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if valid(&u) {
                        cand = Some(u);
                    }
                }
            }
            if cand.is_none() {
                for e in info.values() {
                    let u = e
                        .get("saman")
                        .and_then(|s| s.get("user_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if valid(&u) {
                        cand = Some(u);
                        break;
                    }
                }
            }
            if let Some(u) = cand {
                result["user_id"] = json!(u);
                result["source"] = json!("local_state");
            }
        }
    }
    result
}

// ── check_login_cookie：Cookies 库登录会话存在性（无需解密）─────────────────

/// 读取单个 Profile 的 doubao.com 会话 cookie 概况。
/// 客户端运行中 Cookies 被独占锁：复制 Cookies* 到临时目录后只读打开（python 同款）。
fn read_profile_session(prof: &Path) -> Value {
    let net = prof.join("Network");
    if !net.join("Cookies").is_file() && !prof.join("Cookies").is_file() {
        // 新旧布局均无 Cookies 库
        return json!({"doubao_cookies": 0, "has_session": false, "sessionid_remaining_sec": null, "error": "no cookies file"});
    }
    let src_dir = if net.join("Cookies").is_file() { net } else { prof.to_path_buf() };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let td = std::env::temp_dir().join(format!("aw_ckchk_{}_{nanos}", std::process::id()));

    let work = || -> Result<Value, String> {
        std::fs::create_dir_all(&td).map_err(|e| format!("临时目录创建失败: {e}"))?;
        // Cookies 及其 -journal/-wal 一并复制，避免读到未恢复的事务状态。
        // 客户端运行中会独占 Cookies 锁（os error 32）：短重试两次，仍失败返回明确的
        // 「运行中占用」语义（上层按此静默跳过，不再当作错误刷屏）
        let entries = std::fs::read_dir(&src_dir).map_err(|e| format!("读取目录失败: {e}"))?;
        for e in entries.flatten() {
            let name = e.file_name();
            if name.to_string_lossy().starts_with("Cookies") {
                let src = e.path();
                let dst = td.join(&name);
                let mut copied = Err("unreached".into());
                for _ in 0..3 {
                    copied = std::fs::copy(&src, &dst).map(|_| ()).map_err(|err| {
                        if err.to_string().contains("32") {
                            "Cookies 被豆包客户端运行占用，跳过本轮探测".to_string()
                        } else {
                            format!("Cookies 复制失败: {err}")
                        }
                    });
                    if copied.is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                copied?;
            }
        }
        let conn = rusqlite::Connection::open_with_flags(
            td.join("Cookies"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("Cookies 打开失败: {e}"))?;
        let n: i64 = conn
            .query_row("SELECT count(*) FROM cookies WHERE host_key LIKE '%doubao.com'", [], |r| r.get(0))
            .map_err(|e| format!("查询失败: {e}"))?;
        let sess: i64 = conn
            .query_row(
                "SELECT count(*) FROM cookies WHERE host_key LIKE '%doubao.com' \
                 AND name IN ('sessionid','sid_guard')",
                [],
                |r| r.get(0),
            )
            .map_err(|e| format!("查询失败: {e}"))?;
        // sessionid/sid_guard 最小剩余有效期（秒）。expires_utc = 1601-01-01 起的微秒数；
        // 0 = 会话级 cookie（关浏览器即失效），视为 0 剩余
        let mut remaining: Option<f64> = None;
        {
            let mut stmt = conn
                .prepare(
                    "SELECT expires_utc FROM cookies WHERE host_key LIKE '%doubao.com' \
                     AND name IN ('sessionid','sid_guard')",
                )
                .map_err(|e| format!("查询失败: {e}"))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, i64>(0))
                .map_err(|e| format!("查询失败: {e}"))?;
            for r in rows {
                let exp: i64 = r.map_err(|e| format!("读取失败: {e}"))?;
                if exp == 0 {
                    remaining = Some(0.0);
                    break;
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let remain = exp as f64 / 1_000_000.0 - now - 11_644_473_600.0;
                remaining = Some(match remaining {
                    Some(cur) => cur.min(remain),
                    None => remain,
                });
            }
        }
        Ok(json!({
            "doubao_cookies": n,
            "has_session": sess > 0,
            "sessionid_remaining_sec": remaining.map(|r| r as i64),
            "error": null,
        }))
    };

    let out = match work() {
        Ok(v) => v,
        Err(e) => {
            let pname = prof.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            // 客户端运行中占用 Cookies 锁 = 预期状态（探测不可用非故障），静默跳过不刷屏
            if !e.contains("跳过本轮探测") {
                eprintln!("[check-login] {pname} Cookies 读取失败: {e}");
            }
            json!({
                "doubao_cookies": 0, "has_session": false,
                "sessionid_remaining_sec": null,
                "error": e.chars().take(120).collect::<String>(),
            })
        }
    };
    let _ = std::fs::remove_dir_all(&td);
    out
}

/// 检测某目录（User Data 根 / 快照槽，多 Profile 布局）是否持有登录会话。
/// 活跃 Profile（profile.last_used）语义优先；无 Local State 时回退聚合语义。
/// 返回 {ok, doubao_cookies, has_session, sessionid_remaining_sec, active_profile, profiles}
/// （JSON 契约对齐 python stdout 单行）
pub fn check_login_cookie(profile_dir: &Path) -> Value {
    let mut result = json!({
        "ok": false, "doubao_cookies": 0, "has_session": false,
        "sessionid_remaining_sec": null, "active_profile": null, "profiles": [],
    });
    let profiles = super::doubao_session::chromium_profiles(profile_dir);
    if profiles.is_empty() {
        eprintln!("[check-login] 未找到任何 Profile 目录（Default / Profile N）");
        return result;
    }

    let mut per: Vec<(String, Value)> = Vec::new();
    for prof in &profiles {
        let name = prof.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        per.push((name, read_profile_session(prof)));
    }

    let mut active = super::doubao_session::read_active_profile_name(profile_dir);
    if !active
        .as_ref()
        .map(|a| per.iter().any(|(n, _)| n == a))
        .unwrap_or(false)
    {
        active = None;
    }

    result["ok"] = json!(per
        .iter()
        .any(|(_, e)| e["doubao_cookies"].as_i64().unwrap_or(0) > 0 || e["error"].is_null()));
    result["doubao_cookies"] =
        json!(per.iter().map(|(_, e)| e["doubao_cookies"].as_i64().unwrap_or(0)).sum::<i64>());
    result["active_profile"] = json!(active);
    result["profiles"] = Value::Array(
        per.iter()
            .map(|(n, e)| {
                let mut o = json!({"name": n});
                if let (Some(m), Some(em)) = (o.as_object_mut(), e.as_object()) {
                    for (k, v) in em {
                        m.insert(k.clone(), v.clone());
                    }
                }
                o
            })
            .collect(),
    );

    if let Some(a) = &active {
        // 活跃 Profile 语义：客户端启动打开的就是它，它未登录 = 用户看到未登录
        let e = per
            .iter()
            .find(|(n, _)| n == a)
            .map(|(_, e)| e.clone())
            .unwrap_or(Value::Null);
        result["has_session"] = e["has_session"].clone();
        result["sessionid_remaining_sec"] = e["sessionid_remaining_sec"].clone();
        if !e["error"].is_null() && !e["error"].as_str().unwrap_or("").contains("跳过本轮探测") {
            eprintln!("[check-login] 活跃 Profile {a} 读取失败: {}", e["error"]);
        }
    } else {
        // 旧快照无 Local State：聚合语义（任一有会话；剩余取最小，含未知则未知）
        let cands: Vec<&Value> = per
            .iter()
            .map(|(_, e)| e)
            .filter(|e| e["has_session"].as_bool().unwrap_or(false))
            .collect();
        result["has_session"] = json!(!cands.is_empty());
        if !cands.is_empty() {
            let rems: Vec<Option<i64>> =
                cands.iter().map(|e| e["sessionid_remaining_sec"].as_i64()).collect();
            result["sessionid_remaining_sec"] = if rems.iter().any(Option::is_none) {
                Value::Null
            } else {
                json!(rems.into_iter().flatten().min())
            };
        }
    }
    result
}

// ── 官方 IM API 客户端（对话导出）──────────────────────────────────────────
// 端点（代理抓包实锤，需 cookie: sessionid/sid_tt/sid_guard + ttwid，头 agw-js-conv: str）：
//   POST https://www.doubao.com/im/chain/recent_conv?…  cmd=3200  会话列表
//   POST https://www.doubao.com/im/chain/single?…       cmd=3100  单会话消息
// 正文：content_block[].content.text_block.text；兜底 brief / tts_content

fn uuid_v4() -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(n.to_le_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

fn api_post(agent: &ureq::Agent, url: &str, cookie: &str, body: &Value) -> Result<Value, String> {
    let resp = agent
        .post(url)
        .set("content-type", "application/json; encoding=utf-8")
        .set("cookie", cookie)
        .set("agw-js-conv", "str")
        .set("user-agent", UA)
        .set("accept", "application/json, text/plain, */*")
        .set("referer", "https://www.doubao.com/")
        .send_string(&body.to_string())
        .map_err(|e| format!("API 请求失败: {e}"))?;
    let text = resp.into_string().map_err(|e| format!("响应读取失败: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))
}

fn status_of(j: &Value) -> (i64, String) {
    (
        j.get("status_code").and_then(Value::as_i64).unwrap_or(-1),
        j.get("status_desc").and_then(Value::as_str).unwrap_or("").to_string(),
    )
}

/// 拉取会话列表（按 conv_version 游标翻页）。
/// conv_version 首次请求必须为 int 0（字符串会触发 712010702，实测确认）；
/// 翻页跳失败：豆包客户端自身从不翻页，非首跳可能不被网关支持——降级为警告。
fn fetch_recent_convs(agent: &ureq::Agent, cookie: &str, limit: usize) -> Result<Vec<Value>, String> {
    let mut convs: Vec<Value> = Vec::new();
    let mut cursor = json!(0i64); // int；翻页后为服务端 next_conv_version（str）
    for _ in 0..10 {
        let body = json!({
            "cmd": 3200,
            "uplink_body": {"pull_recent_conv_chain_uplink_body": {
                "limit": limit.min(50), "message_count_per_conv": 0, "api_version": 1,
                "conv_version": cursor, "direction": 3,
                "option": {"not_need_message": true, "need_complete_conversation": true,
                            "need_coco_bot": true, "need_pc_pin_chain": true, "pc_pin_query_type": 0,
                            "exclude_archive": true, "only_archive": false}}},
            "sequence_id": uuid_v4(), "channel": 2, "version": "1",
        });
        let j = api_post(agent, &format!("https://www.doubao.com/im/chain/recent_conv?{RECENT_QS}"), cookie, &body)?;
        let (code, desc) = status_of(&j);
        if code != 0 {
            if cursor.as_i64() == Some(0) {
                return Err(format!("recent_conv 失败: {code} {desc}"));
            }
            eprintln!("[warn] 会话列表翻页失败（已拉 {} 个）: {code} {desc}", convs.len());
            break;
        }
        let chain = &j["downlink_body"]["pull_recent_conv_chain_downlink_body"];
        for cell in chain.get("cells").and_then(Value::as_array).into_iter().flatten() {
            let c = &cell["conversation"];
            if c.get("conversation_id").and_then(Value::as_str).is_some() {
                convs.push(c.clone());
            }
        }
        if !chain.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
            break;
        }
        let nxt = chain.get("next_conv_version").and_then(Value::as_str).unwrap_or("").to_string();
        if nxt.is_empty() || Value::String(nxt.clone()) == cursor {
            break;
        }
        cursor = json!(nxt);
    }
    Ok(convs)
}

/// 拉取单会话消息（从最新向前翻页，最多 max_pages × 20 条）
fn fetch_messages(agent: &ureq::Agent, cookie: &str, conv_id: &str, max_pages: usize) -> Result<Vec<Value>, String> {
    let mut msgs: Vec<Value> = Vec::new();
    let mut anchor: i64 = 9_007_199_254_740_991; // 2^53-1 = 从最新开始
    for _ in 0..max_pages {
        let body = json!({
            "cmd": 3100,
            "uplink_body": {"pull_singe_chain_uplink_body": {
                "conversation_id": conv_id, "anchor_index": anchor, "conversation_type": 3,
                "direction": 1, "limit": 20, "ext": {}, "filter": {"index_list": []},
                "evaluate_ab_params": "", "evaluate_common_params": ""}},
            "sequence_id": uuid_v4(), "channel": 2, "version": "1",
        });
        let j = api_post(agent, &format!("https://www.doubao.com/im/chain/single?{SINGLE_QS}"), cookie, &body)?;
        let (code, desc) = status_of(&j);
        if code != 0 {
            return Err(format!("chain/single 失败: {code} {desc}"));
        }
        let page = j["downlink_body"]["pull_singe_chain_downlink_body"]["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let mut combined = page; // 页内新→旧，向前拼接
        combined.append(&mut msgs);
        msgs = combined;
        // index_in_conv 为字符串形式的大整数序列号；任一解析失败 → 终止翻页（python 同款）
        let mut oldest: Option<i64> = Some(i64::MAX);
        for m in &msgs[..page_len] {
            let v = match m.get("index_in_conv") {
                None | Some(Value::Null) => 0,
                Some(Value::String(s)) if s.is_empty() => 0,
                Some(Value::Number(num)) => num.as_i64().unwrap_or(0),
                Some(Value::String(s)) => match s.parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => {
                        oldest = None;
                        break;
                    }
                },
                _ => 0,
            };
            oldest = Some(oldest.unwrap_or(i64::MAX).min(v));
        }
        let oldest = match oldest {
            None => break,
            Some(o) => o,
        };
        if oldest <= 0 {
            break; // 已到会话开头
        }
        anchor = oldest;
    }
    Ok(msgs)
}

/// 从消息记录提取正文：content_block text 优先，brief / tts_content 兜底
fn message_text(m: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for blk in m.get("content_block").and_then(Value::as_array).into_iter().flatten() {
        if let Some(t) = blk
            .get("content")
            .and_then(|c| c.get("text_block"))
            .and_then(|t| t.get("text"))
            .and_then(Value::as_str)
        {
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    if !parts.is_empty() {
        return parts.join("\n\n");
    }
    ["content", "tts_content", "brief"]
        .iter()
        .find_map(|k| m.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn load_account(state: &AppState, uid: &str) -> Result<Value, String> {
    // SQLite 化（P3）：doubao_accounts 表
    let pool: Value =
        serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
    for acc in pool.get("accounts").and_then(Value::as_array).into_iter().flatten() {
        if acc.get("user_id").and_then(Value::as_str) == Some(uid) {
            return Ok(acc.clone());
        }
    }
    Err(format!("账号 {uid} 不在账号池中（需先保存登录态/录入凭证）"))
}

/// 豆包 IM 网关要求 ttwid / sid_guard 以 URL 编码形式出现（| → %7C 等）；
/// 传原始 | / , / : 会报 712010702。账号池统一存原始（可读）形式，发送前编码；
/// 已含 % 的值视为已编码原样透传，避免双重编码
fn cookie_enc(v: &str) -> String {
    if v.contains('%') {
        v.to_string()
    } else {
        urlencoding::encode(v).to_string()
    }
}

fn build_cookie(acc: &Value) -> Result<String, String> {
    let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
    let sid = acc.get("session_id").and_then(Value::as_str).unwrap_or("");
    if sid.is_empty() {
        return Err(format!(
            "账号 {uid} 未录入 sessionid（编辑账号 → 会话凭证，或开启代理自动抓取）"
        ));
    }
    let mut parts = vec![
        format!("sessionid={sid}"),
        format!("sessionid_ss={sid}"),
        format!("sid_tt={sid}"),
    ];
    if let Some(g) = acc.get("sid_guard").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        parts.push(format!("sid_guard={}", cookie_enc(g)));
    }
    match acc.get("ttwid").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(t) => parts.push(format!("ttwid={}", cookie_enc(t))),
        None => eprintln!("[warn] 账号无 ttwid，对话 API 可能拒绝（登录校验不合法）；开启代理后访问豆包可自动抓取"),
    }
    Ok(parts.join("; "))
}

fn fmt_local_ts(v: &Value, fmt: &str) -> String {
    let secs = v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    match secs.filter(|s| *s != 0) {
        Some(s) => {
            use chrono::TimeZone;
            chrono::Local
                .timestamp_opt(s, 0)
                .single()
                .map(|dt| dt.format(fmt).to_string())
                .unwrap_or_default()
        }
        None => String::new(),
    }
}

fn to_markdown(payload: &Value) -> String {
    let mut lines = vec![
        format!("# 豆包对话记录导出 — 账号 {}", payload["account"].as_str().unwrap_or("")),
        String::new(),
        format!(
            "> 导出时间：{} · 共 {} 个会话",
            payload["exported_at"].as_str().unwrap_or(""),
            payload["conversation_count"].as_i64().unwrap_or(0)
        ),
        String::new(),
    ];
    for c in payload["conversations"].as_array().into_iter().flatten() {
        let t = fmt_local_ts(&c["update_time"], "%Y-%m-%d %H:%M");
        let trunc = if c["truncated"].as_bool().unwrap_or(false) {
            "（消息较多，仅导出最近部分）"
        } else {
            ""
        };
        lines.push(format!("## {}", c["name"].as_str().unwrap_or("")));
        lines.push(String::new());
        lines.push(format!("*更新：{t} · {} 条消息 {trunc}*", c["message_count"].as_i64().unwrap_or(0)));
        lines.push(String::new());
        let mut cur_section: Option<String> = None;
        for m in c["messages"].as_array().into_iter().flatten() {
            let sec = m["section_name"].as_str().unwrap_or("");
            if !sec.is_empty() && cur_section.as_deref() != Some(sec) {
                cur_section = Some(sec.to_string());
                lines.push(format!("### {sec}"));
                lines.push(String::new());
            }
            let who = if m["role"].as_str() == Some("user") { "🧑 用户" } else { "🤖 豆包" };
            let ts = fmt_local_ts(&m["create_time"], "%H:%M");
            lines.push(format!("**{who}** `{ts}`"));
            lines.push(String::new());
            lines.push(
                m["text"].as_str().filter(|s| !s.is_empty()).unwrap_or("（无文本内容）").to_string(),
            );
            lines.push(String::new());
        }
        lines.push("---".to_string());
        lines.push(String::new());
    }
    lines.join("\n")
}

/// 导出指定账号的对话记录 → data/exports/doubao_chats_<uid>_<ts>.md / .json
/// （JSON/Markdown 契约对齐 python export_account，前端/历史记录消费同款）
pub fn export_account(
    state: &AppState,
    uid: &str,
    limit_convs: usize,
    max_pages: usize,
) -> Result<Value, String> {
    let acc = load_account(state, uid)?;
    let cookie = build_cookie(&acc)?;
    let agent = super::http_agent(25); // 出站直连（ureq 默认不走系统代理）
    let convs: Vec<Value> =
        fetch_recent_convs(&agent, &cookie, limit_convs)?.into_iter().take(limit_convs).collect();

    let mut out_convs: Vec<Value> = Vec::new();
    for (i, c) in convs.iter().enumerate() {
        let cid = c.get("conversation_id").and_then(Value::as_str).unwrap_or("").to_string();
        let msgs = match fetch_messages(&agent, &cookie, &cid, max_pages) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[warn] 会话 {cid} 拉取失败: {e}");
                Vec::new()
            }
        };
        let slim: Vec<Value> = msgs
            .iter()
            .map(|m| {
                json!({
                    "user_type": m.get("user_type").cloned().unwrap_or(Value::Null),
                    "role": if m.get("user_type").and_then(Value::as_i64) == Some(1) { "user" } else { "assistant" },
                    "section_name": m.get("section_name").and_then(Value::as_str).unwrap_or(""),
                    "create_time": m.get("create_time").cloned().unwrap_or(Value::Null),
                    "text": message_text(m),
                })
            })
            .collect();
        let name = c
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("会话 {cid}"));
        eprintln!("[{}/{}] {}（{} 条）", i + 1, convs.len(), name, slim.len());
        out_convs.push(json!({
            "conversation_id": cid,
            "name": name,
            "update_time": c.get("update_time").cloned().unwrap_or(Value::Null),
            "create_time": c.get("create_time").cloned().unwrap_or(Value::Null),
            "message_count": slim.len(),
            "truncated": msgs.len() >= max_pages * 20,
            "messages": slim,
        }));
    }

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let export_dir = state.data_dir.join("data").join("exports");
    std::fs::create_dir_all(&export_dir).map_err(|e| format!("创建导出目录失败: {e}"))?;
    let json_path = export_dir.join(format!("doubao_chats_{uid}_{ts}.json"));
    let md_path = export_dir.join(format!("doubao_chats_{uid}_{ts}.md"));
    let payload = json!({
        "account": uid,
        "exported_at": now_str(),
        "conversation_count": out_convs.len(),
        "conversations": out_convs,
    });
    std::fs::write(&json_path, serde_json::to_string_pretty(&payload).unwrap_or_default())
        .map_err(|e| format!("写入 JSON 失败: {e}"))?;
    std::fs::write(&md_path, to_markdown(&payload)).map_err(|e| format!("写入 Markdown 失败: {e}"))?;
    Ok(json!({
        "ok": true,
        "conversations": payload["conversation_count"],
        "messages": payload["conversations"]
            .as_array()
            .map(|a| a.iter().map(|c| c["message_count"].as_i64().unwrap_or(0)).sum::<i64>())
            .unwrap_or(0),
        "md_path": md_path.to_string_lossy(),
        "json_path": json_path.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WAL 批次重组：full 记录直达 / first+middle+last 跨记录拼接
    #[test]
    fn wal_batches_reassemble() {
        // 构造一个 32768 块内两条 full 记录
        let mut raw = Vec::new();
        let rec = |payload: &[u8], rtype: u8| {
            let mut r = Vec::new();
            let crc = [0u8; 4]; // python 移植不校验 CRC
            r.extend_from_slice(&crc);
            r.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            r.push(rtype);
            r.extend_from_slice(payload);
            r
        };
        raw.extend_from_slice(&rec(b"AAAA", 1));
        raw.extend_from_slice(&rec(b"BBB", 2));
        raw.extend_from_slice(&rec(b"CC", 3));
        raw.extend_from_slice(&rec(b"D", 4));
        let batches = read_wal_batches(&raw);
        assert_eq!(batches, vec![b"AAAA".to_vec(), b"BBBCCD".to_vec()]);
    }

    /// snappy 解压：literal + 1 字节 copy 回引
    #[test]
    fn snappy_literal_and_copy() {
        // "abcabcabcabc"：literal "abc" + copy tag1 (len=9, off=3)
        let mut src = vec![12]; // 解压后长度 varint
        src.push(((3 - 1) << 2) as u8); // literal len 3（<60 无扩展）
        src.extend_from_slice(b"abc");
        src.push((5 << 2) | 1); // copy1: ((t>>2)&7)+4=9 → t>>2=5；off 高位 0
        src.push(3); // copy1 offset 低 8 位
        let out = snappy_decompress(&src).unwrap();
        assert_eq!(out, b"abcabcabcabc");
    }

    /// varint 边界：多字节 / 截断报错
    #[test]
    fn varint_roundtrip() {
        assert_eq!(read_varint(&[0x7F], 0).unwrap(), (0x7F, 1));
        assert_eq!(read_varint(&[0xFF, 0x01], 0).unwrap(), (0xFF, 2));
        assert!(read_varint(&[0xFF], 0).is_err());
    }

    /// cookie_enc：原始值全量编码 / 已含 % 原样透传
    #[test]
    fn cookie_enc_rules() {
        assert_eq!(cookie_enc("a|b"), "a%7Cb");
        assert_eq!(cookie_enc("a%7Cb"), "a%7Cb");
    }
}
