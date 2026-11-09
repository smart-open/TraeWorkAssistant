//! WorkBuddy/CodeBuddy 客户端 auth 文件字段级加密（`$wbEncrypted`）兼容层（issue #198）。
//!
//! 客户端升级（WorkBuddy 5.9+ / CodeBuddy CN 4.12+ 实测）把共享 auth 文件
//! `workbuddy-desktop.info` 的 accessToken / refreshToken / nickname / phoneNumber 等
//! 敏感字段改为加密包裹对象：`{ $wbEncrypted: 1, envelope: "<base64(...)>" }`，
//! envelope 内层 JSON 为 `{ suite: 1, keyId, nonce, authTag, ciphertext }`（AES-256-GCM）。
//! `account.uid` 仍为明文，身份识别/守卫链路不受影响。
//!
//! 算法逆向自客户端自带 JS（WorkBuddy `resources/app.asar.unpacked/cli/dist/codebuddy-lite-wb.mjs`
//! 的 `@genie/at-rest-crypto` 模块，与 app.asar 内主进程实现逐字对齐）：
//! - `deriveAtRestKeyId(key)` = hex(SHA256(key)) 前 16 位；
//! - seal：AES-256-GCM，12B nonce、16B authTag，AAD =
//!   `"WB-AAD\0"` || 0x01 || LP("WBEV1") || LP("sym-v1") || u32be(suite=1) || LP(keyId) || 0x02 || 0x00 || 0x00
//!   （LP(s)=u32be(len)+utf8；0x02 为 field framing 码；两个 0x00 = sym-v1 整段值无 sequence/final）；
//! - 数据密钥（keyId 即信封 keyId）由客户端 native 层 `electron_browser_workbuddy_storage`
//!   的 `loggerGet()` 提供（构建期注入，非 DPAPI；`~/.workbuddy/keyblob` 的 static-v1 槽
//!   仅存其包裹副本，包裹密钥同为该密钥，静态提取不可行）。
//!
//! 密钥获取：`ELECTRON_RUN_AS_NODE=1` 启动本机已安装客户端 exe（Node 模式，不运行应用
//! 本体，无窗口无单实例副作用）调用同一 native binding 提取。仅读本机自有凭证；
//! 密钥只驻进程内存（按 keyId 缓存），不落盘、不进日志；stdout 捕获即用即弃。
//!
//! 兼容性：旧明文 auth 文件不含包裹对象，`decrypt_auth_file_fields` 为零改动空操作。
//!
//! 接入边界（设计决策）：仅用户显式触发的低频命令（扫描/导入/续期）接解密——
//! 探针需拉起客户端进程（秒级），不可进入 tasks 侧热路径；签到/续期任务走
//! `effective_creds`（token store 副本 + 401 自愈链兜底），不读解密。

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// 信封固定参数（逆向自客户端 parseSupportedEnvelope + AtRestCrypto）
const SUITE_AES_256_GCM_V1: i64 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_ID_LEN: usize = 16;
/// field framing 的 magic 与 framing 码（A/E 表，逆向自 buildAuthenticatedContextAad）
const FIELD_MAGIC: &str = "WBEV1";
const FIELD_FRAMING_CODE: u8 = 2;

// ── 解密统计 ────────────────────────────────────────────────────────────────

/// 字段解密扫描统计（decrypt_auth_file_fields 返回）
#[derive(Debug, Default, Clone)]
pub struct DecryptStats {
    /// 成功解密并原位替换为明文的字段数
    pub decrypted: usize,
    /// 解密失败的字段数（信封仍原样保留）
    pub failed: usize,
    /// 非对称形态（scheme=asym-v1）字段数——本工具暂不支持，原样保留
    pub unsupported_asym: usize,
    /// 最近一次失败原因（供上层报错引用）
    pub last_error: Option<String>,
}

impl DecryptStats {
    /// 是否所有信封字段都已成功解密（无包裹残留）
    pub fn all_clear(&self) -> bool {
        self.failed == 0 && self.unsupported_asym == 0
    }
}

// ── 包裹对象识别 ────────────────────────────────────────────────────────────

/// 标准 sym-v1 包裹：恰好 `{$wbEncrypted:1, envelope:<string>}`
fn is_standard_wrapper(obj: &serde_json::Map<String, Value>) -> bool {
    obj.len() == 2
        && obj.get("$wbEncrypted").and_then(Value::as_i64) == Some(1)
        && obj.get("envelope").and_then(Value::as_str).is_some()
}

/// 非对称包裹：恰好 `{$wbEncrypted:1, envelope:<string>, scheme:"asym-v1"}`
fn is_asym_wrapper(obj: &serde_json::Map<String, Value>) -> bool {
    obj.len() == 3
        && obj.get("$wbEncrypted").and_then(Value::as_i64) == Some(1)
        && obj.get("scheme").and_then(Value::as_str) == Some("asym-v1")
        && obj.get("envelope").and_then(Value::as_str).is_some()
}

/// 值是否为任意 `$wbEncrypted` 包裹（供报错分支识别，区别于「结构变更」）
pub fn is_encrypted_wrapper(v: &Value) -> bool {
    v.as_object().map(|o| is_standard_wrapper(o) || is_asym_wrapper(o)).unwrap_or(false)
}

// ── AAD 构造（sym-v1 整段值上下文，逐字节对齐 buildAuthenticatedContextAad）──

fn push_lp(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// sym-v1 AAD：`"WB-AAD\0"` + 版本 1 + LP(framing magic) + LP("sym-v1") + u32be(suite)
/// + LP(keyId) + framing 码 + 无 sequence + 无 final
fn aad_sym_v1(key_id: &str, framing_magic: &str, framing_code: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(b"WB-AAD\0");
    out.push(1);
    push_lp(&mut out, framing_magic);
    push_lp(&mut out, "sym-v1");
    out.extend_from_slice(&(SUITE_AES_256_GCM_V1 as u32).to_be_bytes());
    push_lp(&mut out, key_id);
    out.push(framing_code);
    out.push(0); // sequence 缺省
    out.push(0); // final 缺省
    out
}

// ── 信封解析 ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct FieldEnvelope {
    key_id: String,
    nonce: Vec<u8>,
    auth_tag: Vec<u8>,
    ciphertext: Vec<u8>,
}
/// 宽容 base64 解码：标准/URL-safe 字母表均可，缺 padding 自动补齐
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("base64 为空".into());
    }
    let normalized: String = s
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            _ => c,
        })
        .collect();
    let normalized = match normalized.len() % 4 {
        2 => format!("{normalized}=="),
        3 => format!("{normalized}="),
        _ => normalized,
    };
    base64::engine::general_purpose::STANDARD
        .decode(normalized.as_bytes())
        .map_err(|e| format!("base64 解码失败: {e}"))
}

fn parse_envelope(envelope_b64: &str) -> Result<FieldEnvelope, String> {
    let raw = b64_decode(envelope_b64)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("信封非合法 JSON: {e}"))?;
    let obj = v.as_object().ok_or("信封 JSON 非对象")?;
    let suite = obj.get("suite").and_then(Value::as_i64).ok_or("信封缺 suite")?;
    if suite != SUITE_AES_256_GCM_V1 {
        return Err(format!("不支持的加密套件 suite={suite}"));
    }
    let key_id = obj.get("keyId").and_then(Value::as_str).ok_or("信封缺 keyId")?;
    if key_id.len() != KEY_ID_LEN || !key_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("keyId 形态非法（预期 {KEY_ID_LEN} 位十六进制）"));
    }
    let nonce = b64_decode(obj.get("nonce").and_then(Value::as_str).ok_or("信封缺 nonce")?)?;
    if nonce.len() != NONCE_LEN {
        return Err(format!("nonce 长度 {0} 非 {NONCE_LEN} 字节", nonce.len()));
    }
    let auth_tag =
        b64_decode(obj.get("authTag").and_then(Value::as_str).ok_or("信封缺 authTag")?)?;
    if auth_tag.len() != TAG_LEN {
        return Err(format!("authTag 长度 {0} 非 {TAG_LEN} 字节", auth_tag.len()));
    }
    let ciphertext =
        b64_decode(obj.get("ciphertext").and_then(Value::as_str).ok_or("信封缺 ciphertext")?)?;
    if ciphertext.is_empty() {
        return Err("ciphertext 为空".into());
    }
    Ok(FieldEnvelope { key_id: key_id.to_ascii_lowercase(), nonce, auth_tag, ciphertext })
}

// ── 解密 ────────────────────────────────────────────────────────────────────

/// keyId 派生（对齐 deriveAtRestKeyId）：hex(SHA256(key)) 前 16 位
fn derive_key_id(key: &[u8]) -> String {
    let h = Sha256::digest(key);
    h[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// 解开单个信封 → 明文 Value（字符串原样；JSON 对象/数组还原结构）
fn open_envelope(key: &[u8], env: &FieldEnvelope) -> Result<Value, String> {
    let actual = derive_key_id(key);
    if actual != env.key_id {
        return Err(format!("密钥不匹配（信封 keyId={}，密钥派生 {}）", env.key_id, actual));
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("AES 初始化失败: {e}"))?;
    let mut sealed = env.ciphertext.clone();
    sealed.extend_from_slice(&env.auth_tag);
    let aad = aad_sym_v1(&env.key_id, FIELD_MAGIC, FIELD_FRAMING_CODE);
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&env.nonce),
            Payload { msg: &sealed, aad: &aad },
        )
        .map_err(|_| "GCM 认证失败（密钥错误或载荷被篡改）".to_string())?;
    let s = String::from_utf8(plain).map_err(|_| "明文非合法 UTF-8".to_string())?;
    let trimmed = s.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(&s).map_err(|e| format!("明文 JSON 解析失败: {e}"))
    } else {
        Ok(Value::String(s))
    }
}

// ── 密钥获取（客户端运行时探针 + 进程内缓存）───────────────────────────────

static KEY_CACHE: std::sync::LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 探针 JS：调 native binding 的 loggerGet()，payload 以魔数前缀经 fs.writeSync(1,…)
/// 同步写 stdout（管道异步缓冲下 process 退出会随机丢输出，writeSync 根除竞态）。
/// 必须以 .cjs 落盘——用户仓库/工作目录上层 package.json 带 "type":"module" 时 .js
/// 会被 Node 按 ESM 解析（require 未定义、顶层行为漂移），.cjs 强制 CommonJS。
const PROBE_JS: &str = r#"const fs=require('fs');fs.writeSync(1,"WBATREST1\n"+process._linkedBinding('electron_browser_workbuddy_storage').loggerGet());"#;

/// 候选客户端 exe（存在即按序尝试；首个成功匹配 keyId 者生效）。
/// WorkBuddy 桌面（本机实测 binding 可达）在前，CodeBuddy CN（auth 文件实际写入方）次之。
/// WorkBuddyAI\ 独立目录两形态与 commands/env.rs workbuddy_ai 档案 exe_candidates
/// 口径对齐（并存实测未观测到，标准布局防御性兜底，防「客户端在盘却报未找到」）。
fn probe_exe_candidates() -> Vec<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let programs = std::path::PathBuf::from(local).join("Programs");
    vec![
        programs.join("WorkBuddy").join("WorkBuddyAI.exe"),
        programs.join("WorkBuddy").join("WorkBuddy.exe"),
        programs.join("WorkBuddyAI").join("WorkBuddyAI.exe"),
        programs.join("WorkBuddyAI").join("WorkBuddy AI.exe"),
        programs.join("CodeBuddy CN").join("CodeBuddy CN.exe"),
        programs.join("CodeBuddy").join("CodeBuddy.exe"),
    ]
}

/// 从客户端运行时提取 at-rest 主密钥（keyId 匹配校验在 Rust 侧）。
/// Ok(None) = 全部候选不可用；Err = 有候选但提取异常。
fn probe_master_key(key_id: &str) -> Result<Option<Vec<u8>>, String> {
    let mut tried = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for exe in probe_exe_candidates() {
        if !exe.is_file() {
            continue;
        }
        tried += 1;
        match probe_once(&exe) {
            Ok(stdout) => {
                let parsed = (|| -> Result<Vec<u8>, String> {
                    let payload = stdout
                        .split_once("WBATREST1\n")
                        .map(|(_, rest)| rest)
                        .ok_or("探针输出缺魔数前缀")?;
                    let v: Value = serde_json::from_str(payload.trim())
                        .map_err(|e| format!("探针 payload 非合法 JSON: {e}"))?;
                    let secret = v
                        .get("atRestSecretKey")
                        .and_then(Value::as_str)
                        .ok_or("探针 payload 缺 atRestSecretKey")?;
                    // 密钥派生（对齐客户端 normalizeAtRestKeyPayload）：
                    // key = SHA256(utf8(atRestSecretKey 字符串本身))——不是对 base64
                    // 解码后的字节（解码值仅作 32 字节规范校验）
                    if b64_decode(secret).map(|k| k.len()).unwrap_or(0) != 32 {
                        return Err("atRestSecretKey 非 32 字节规范 base64".into());
                    }
                    Ok(Sha256::digest(secret.as_bytes()).to_vec())
                })();
                match parsed {
                    Ok(key) if derive_key_id(&key) == key_id => return Ok(Some(key)),
                    // 密钥与 auth 文件信封不属同一客户端体系（双客户端并存构建不同
                    // 密钥）——换下一候选继续；解析类错误同记并继续
                    Ok(_) => errs.push(format!("{} 提供的密钥与信封 keyId 不匹配", exe.display())),
                    Err(e) => errs.push(format!("{} 解析失败: {e}", exe.display())),
                }
            }
            Err(e) => {
                errs.push(format!("{} 探针失败: {e}", exe.display()));
            }
        }
    }
    if tried == 0 {
        Ok(None)
    } else {
        Err(errs.join("；"))
    }
}

/// 探针 JS（ESM 兜底版）：.cjs 版 require 不可用（patched 内核强制 ESM）时使用。
/// dynamic import 同样受 ESM 解析器支持；fs.writeSync(1,…) 同步写根除管道 flush 竞态。
const PROBE_JS_ESM: &str = r#"import fs from 'node:fs';fs.writeSync(1,"WBATREST1\n"+process._linkedBinding('electron_browser_workbuddy_storage').loggerGet());"#;

/// 探针临时脚本序号（同进程并发命令桥防同名互踩：scan/import/续期可能同时触发探针）
static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 单次探针：ELECTRON_RUN_AS_NODE=1 跑客户端 exe 执行探针脚本，捕获 stdout。
/// 先 .cjs（标准 Node CommonJS），输出无魔数且报错时改 .mjs 兜底（patched 内核
/// 可能无视扩展名强制 ESM）。stderr 始终捕获并附入失败原因（诊断用，不含密钥）。
fn probe_once(exe: &std::path::Path) -> Result<String, String> {
    let mut last_err = String::new();
    for (script, body) in [
        ("cjs", PROBE_JS),
        ("mjs", PROBE_JS_ESM),
    ] {
        let n = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 审查修复（L-2，临时脚本名不可预测）：pid+原子序号可被同权限进程预测并
        // 抢先创建/替换（占位攻击），追加纳秒级时间戳提高不可预测性
        let tmp = std::env::temp_dir().join(format!(
            "wb-atrest-probe-{}-{}-{}.{script}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        if let Err(e) = std::fs::write(&tmp, body) {
            // 审查修复：写失败（磁盘满/权限/杀软锁定）记入 last_err——原静默
            // continue 会让两轮均失败时返回空错误串，用户只见「探针失败: 」
            last_err = format!("探针脚本写入 {} 失败: {e}", tmp.display());
            continue;
        }
        let result = run_probe_script(exe, &tmp);
        let _ = std::fs::remove_file(&tmp);
        match result {
            Ok(out) if out.contains("WBATREST1\n") => return Ok(out),
            Ok(out) => {
                last_err = format!("{} 探针输出缺魔数（len={}）", exe.display(), out.len());
            }
            Err(e) => {
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// 跑单份探针脚本：返回 stdout 全文；失败附 stderr 摘要（不含密钥——stderr 只有
/// Node 报错栈，密钥仅经 stdout 传递）。
fn run_probe_script(exe: &std::path::Path, script: &std::path::Path) -> Result<String, String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(script)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // Node 模式跑探针不闪控制台窗口（对齐 switcher/proc.rs 口径）
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("客户端启动失败: {e}"))?;
    // stdout/stderr 侧线程收集（payload 数 KB）；主线程限时轮询退出。
    // 审查修复（C-13，join 无超时）：child 被 kill 但孙进程继承管道写端时
    // read_to_end 永不 EOF，原 reader.join() 会永久挂死。改 mpsc channel 传递 +
    // 主线程 recv_timeout 限时收取，超时即放弃收集（线程 detach 留在后台，
    // 收不到就不再等），收集线程句柄不再持有
    let mut out = child.stdout.take().ok_or("客户端 stdout 不可读")?;
    let mut err = child.stderr.take().ok_or("客户端 stderr 不可读")?;
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (err_tx, err_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut buf);
        let _ = out_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut buf);
        let _ = err_tx.send(String::from_utf8_lossy(&buf).to_string());
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if std::time::Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => {
                // try_wait 系统调用失败：先终止子进程防其继续占住管道写端
                let _ = child.kill();
                return Err(format!("等待客户端退出失败: {e}"));
            }
        }
    };
    // 审查修复（超时死锁）：超时先 kill 再收集——子进程存活时管道写端不关，
    // 读线程的 read_to_end 永久阻塞，原「先 join 后 kill」顺序使 20s 超时在
    // 真正挂起场景完全失效（kill 不可达）。kill 关闭本子进程的管道写端使其
    // 退出；孙进程继承写端的残余场景由下方 recv_timeout 兜底放弃收集
    if status.is_none() {
        let _ = child.kill();
    }
    // stdout 严格（缺数据无法判定魔数，报错）；stderr 仅供诊断，尽力而为
    let stdout = out_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .map_err(|_| "探针 stdout 收集超时（孙进程可能继承管道持有句柄），已放弃收集".to_string())?;
    let stderr = err_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap_or_default();
    if status.is_none() {
        return Err("客户端探针超时（20s）".into());
    }
    let ok = status.expect("checked").success();
    let s = String::from_utf8_lossy(&stdout).to_string();
    if !ok && !s.contains("WBATREST1\n") {
        let stderr_brief: String = stderr.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
        let stderr_brief = stderr_brief.chars().take(200).collect::<String>();
        return Err(format!(
            "客户端探针退出码异常（stderr: {}）",
            if stderr_brief.is_empty() { "无输出".to_string() } else { stderr_brief }
        ));
    }
    Ok(s)
}

/// 取指定 keyId 的主密钥：进程内缓存 → 客户端运行时探针。
fn key_for(key_id: &str) -> Result<Vec<u8>, String> {
    if let Some(k) = KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(key_id) {
        return Ok(k.clone());
    }
    match probe_master_key(key_id) {
        Ok(Some(key)) => {
            KEY_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key_id.to_string(), key.clone());
            Ok(key)
        }
        Ok(None) => Err(
            "本机未找到可用的 WorkBuddy/CodeBuddy 客户端（解密密钥随客户端分发，无客户端则无法解密）"
                .into(),
        ),
        Err(e) => Err(e),
    }
}

// ── 全文件遍历解密 ──────────────────────────────────────────────────────────

/// 递归解密 JSON 树中所有 `$wbEncrypted` 包裹并原位替换为明文。
/// 单字段失败不阻断其他字段（计数记录，信封原样保留）——上层若关键字段仍为
/// 包裹形态，由 extract_access_token 的包裹感知报错给出明确指引。
pub fn decrypt_auth_file_fields(raw: &mut Value) -> DecryptStats {
    let mut stats = DecryptStats::default();
    let mut resolved: HashMap<String, Vec<u8>> = HashMap::new();
    walk(raw, &mut stats, &mut resolved);
    stats
}

/// 测试缝隙：预置 keyId→密钥 表（优先于缓存/探针），单测无需拉起真实客户端。
#[cfg(test)]
#[doc(hidden)]
pub fn decrypt_with_keys_for_test(raw: &mut Value, keys: &HashMap<String, Vec<u8>>) -> DecryptStats {
    let mut stats = DecryptStats::default();
    let mut resolved: HashMap<String, Vec<u8>> = keys.clone();
    walk(raw, &mut stats, &mut resolved);
    stats
}

fn walk(v: &mut Value, stats: &mut DecryptStats, resolved: &mut HashMap<String, Vec<u8>>) {
    match v {
        Value::Object(obj) => {
            if is_standard_wrapper(obj) {
                let envelope = obj.get("envelope").and_then(Value::as_str).unwrap_or_default();
                match (|| -> Result<Value, String> {
                    let env = parse_envelope(envelope)?;
                    let key = if let Some(k) = resolved.get(&env.key_id) {
                        k.clone()
                    } else {
                        let k = key_for(&env.key_id)?;
                        resolved.insert(env.key_id.clone(), k.clone());
                        k
                    };
                    open_envelope(&key, &env)
                })() {
                    Ok(plain) => {
                        *v = plain;
                        stats.decrypted += 1;
                    }
                    Err(e) => {
                        stats.failed += 1;
                        stats.last_error = Some(e);
                    }
                }
            } else if is_asym_wrapper(obj) {
                stats.unsupported_asym += 1;
                stats.last_error =
                    Some("字段为客户端非对称加密形态（asym-v1），本工具暂不支持解密".into());
            } else {
                for child in obj.values_mut() {
                    walk(child, stats, resolved);
                }
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                walk(child, stats, resolved);
            }
        }
        _ => {}
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

/// 测试缝隙：与客户端 seal 同算法封装 wrapper（cfg(test) 期内 crate 级可见，
/// 供 accounts.rs 端到端用例构造加密形态 auth 文件）
#[cfg(test)]
#[doc(hidden)]
pub fn seal_wrapper_export_for_test(key: &[u8], key_id: &str, plaintext: &str) -> Value {
    use aes_gcm::KeyInit;
    use serde_json::json;
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let nonce = [0x11u8; NONCE_LEN];
    let aad = aad_sym_v1(key_id, FIELD_MAGIC, FIELD_FRAMING_CODE);
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext.as_bytes(), aad: &aad })
        .unwrap();
    let (ct, tag) = sealed.split_at(sealed.len() - TAG_LEN);
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    json!({
        "$wbEncrypted": 1,
        "envelope": base64::engine::general_purpose::STANDARD.encode(
            serde_json::json!({
                "suite": SUITE_AES_256_GCM_V1,
                "keyId": key_id,
                "nonce": b64(&nonce),
                "authTag": b64(tag),
                "ciphertext": b64(ct),
            }).to_string()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用 seal：复用模块级 cfg(test) 导出实现
    fn seal_for_test(key: &[u8], key_id: &str, plaintext: &str, framing_magic: &str, framing_code: u8) -> Value {
        // 直接内联实现（framing 可变，与导出的 field 固定版区分）
        use aes_gcm::KeyInit;
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let nonce = [0x11u8; NONCE_LEN];
        let aad = aad_sym_v1(key_id, framing_magic, framing_code);
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext.as_bytes(), aad: &aad })
            .unwrap();
        let (ct, tag) = sealed.split_at(sealed.len() - TAG_LEN);
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        json!({
            "$wbEncrypted": 1,
            "envelope": base64::engine::general_purpose::STANDARD.encode(
                serde_json::json!({
                    "suite": SUITE_AES_256_GCM_V1,
                    "keyId": key_id,
                    "nonce": b64(&nonce),
                    "authTag": b64(tag),
                    "ciphertext": b64(ct),
                }).to_string()
            ),
        })
    }

    fn wrapper(key: &[u8], plaintext: &str) -> Value {
        let kid = derive_key_id(key);
        seal_for_test(key, &kid, plaintext, FIELD_MAGIC, FIELD_FRAMING_CODE)
    }

    /// 测试密钥表（避免单测触发真实客户端探针）
    fn test_keys(key: [u8; 32]) -> HashMap<String, Vec<u8>> {
        HashMap::from([(derive_key_id(&key), key.to_vec())])
    }

    #[test]
    fn 新格式_端到端_解密还原全部字段() {
        // 模拟升级后客户端 auth 文件形态：uid 明文 + auth.accessToken/refreshToken 包裹
        let key = [0x42u8; 32];
        let mut raw = json!({
            "account": { "uid": "cf1473ce-a60e-4f7c-a9aa-b4e19480d90e" },
            "uid": "cf1473ce-a60e-4f7c-a9aa-b4e19480d90e",
            "auth": {
                "accessToken": wrapper(&key, "eyJhbGciOiJSUzI1NiJ9.e30.sig"),
                "refreshToken": wrapper(&key, "refresh-token-value"),
                "tokenType": "Bearer",
            },
        });
        let stats = decrypt_with_keys_for_test(&mut raw, &test_keys(key));
        assert_eq!(stats.decrypted, 2, "accessToken+refreshToken 应解密");
        assert!(stats.all_clear(), "无失败残留: {:?}", stats.last_error);
        assert_eq!(
            raw["auth"]["accessToken"].as_str().unwrap(),
            "eyJhbGciOiJSUzI1NiJ9.e30.sig"
        );
        assert_eq!(
            raw["auth"]["refreshToken"].as_str().unwrap(),
            "refresh-token-value"
        );
        assert_eq!(raw["auth"]["tokenType"].as_str().unwrap(), "Bearer");
    }

    #[test]
    fn 旧明文格式_零改动空操作() {
        let mut raw = json!({
            "uid": "u1",
            "auth": { "accessToken": "tok-plain", "refreshToken": "rt" },
        });
        let before = raw.clone();
        let stats = decrypt_auth_file_fields(&mut raw);
        assert_eq!(stats.decrypted, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(raw, before, "明文文件不得有任何改动");
    }

    #[test]
    fn 错误密钥_解密失败_信封保留() {
        let key = [0x42u8; 32];
        let wrapper = wrapper(&key, "secret-token");
        let raw = json!({ "auth": { "accessToken": wrapper } });
        // 换一把不匹配的密钥：先让 resolved 里塞入错误密钥不可行（key_for 走探针），
        // 直接构造 keyId 指向真实密钥但运行时探针拿不到——改为校验 open_envelope 直斥
        let env = parse_envelope(raw["auth"]["accessToken"]["envelope"].as_str().unwrap()).unwrap();
        let wrong = [0x01u8; 32];
        let err = open_envelope(&wrong, &env).unwrap_err();
        assert!(err.contains("密钥不匹配"), "{err}");
        // 包裹原样保留
        assert!(raw["auth"]["accessToken"]["$wbEncrypted"].as_i64() == Some(1));
    }

    #[test]
    fn 载荷篡改_gcm认证失败() {
        let key = [0x42u8; 32];
        let mut w = wrapper(&key, "secret-token");
        // 篡改 envelope 内 ciphertext：翻一个字节后重编码
        let inner = b64_decode(w["envelope"].as_str().unwrap()).unwrap();
        let mut inner_json: Value = serde_json::from_slice(&inner).unwrap();
        let mut ct = b64_decode(inner_json["ciphertext"].as_str().unwrap()).unwrap();
        ct[0] ^= 0x01;
        inner_json["ciphertext"] = Value::String(base64::engine::general_purpose::STANDARD.encode(&ct));
        w["envelope"] = Value::String(
            base64::engine::general_purpose::STANDARD.encode(inner_json.to_string()),
        );
        let mut raw = json!({ "auth": { "accessToken": w } });
        let stats = decrypt_with_keys_for_test(&mut raw, &test_keys(key));
        assert_eq!(stats.failed, 1);
        assert!(stats.last_error.as_deref().unwrap_or_default().contains("GCM 认证失败"));
    }

    #[test]
    fn 不支持_asym_v1包裹_计数保留() {
        let mut raw = json!({
            "auth": {
                "accessToken": {
                    "$wbEncrypted": 1,
                    "scheme": "asym-v1",
                    "envelope": "e30=",
                },
            },
        });
        let stats = decrypt_auth_file_fields(&mut raw);
        assert_eq!(stats.unsupported_asym, 1);
        assert!(is_encrypted_wrapper(&raw["auth"]["accessToken"]));
    }

    #[test]
    fn 信封解析_形态校验() {
        // 非 JSON
        assert!(parse_envelope(&base64::engine::general_purpose::STANDARD.encode("not-json")).is_err());
        // 缺 suite
        let bad = base64::engine::general_purpose::STANDARD.encode(json!({"keyId": "x"}).to_string());
        assert!(parse_envelope(&bad).is_err());
        // suite 不支持
        let bad2 = base64::engine::general_purpose::STANDARD.encode(
            json!({"suite": 9, "keyId": "0123456789abcdef", "nonce": "", "authTag": "", "ciphertext": ""}).to_string(),
        );
        let err = parse_envelope(&bad2).unwrap_err();
        assert!(err.contains("suite"), "{err}");
        // keyId 形态非法
        let bad3 = base64::engine::general_purpose::STANDARD.encode(
            json!({"suite": 1, "keyId": "zz", "nonce": "", "authTag": "", "ciphertext": ""}).to_string(),
        );
        assert!(parse_envelope(&bad3).unwrap_err().contains("keyId"));
    }

    #[test]
    fn aad_错位_framing_解密必败() {
        // 用 file framing（WBEF1/码1）封的载荷按 field 解必败——钉死 AAD 逐字节正确性
        let key = [0x42u8; 32];
        let kid = derive_key_id(&key);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = [0x11u8; NONCE_LEN];
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: b"abc", aad: &aad_sym_v1(&kid, "WBEF1", 1) },
            )
            .unwrap();
        let (ct, tag) = sealed.split_at(sealed.len() - TAG_LEN);
        let env = FieldEnvelope {
            key_id: kid.clone(),
            nonce: nonce.to_vec(),
            auth_tag: tag.to_vec(),
            ciphertext: ct.to_vec(),
        };
        let err = open_envelope(&key, &env).unwrap_err();
        assert!(err.contains("GCM 认证失败"), "{err}");
    }

    #[test]
    fn 明文非json_还原为字符串_json对象_还原结构() {
        let key = [0x42u8; 32];
        let mut raw = json!({
            "a": wrapper(&key, "plain-jwt"),
            "b": wrapper(&key, "{\"nested\":1}"),
        });
        let stats = decrypt_with_keys_for_test(&mut raw, &test_keys(key));
        assert_eq!(stats.decrypted, 2);
        assert_eq!(raw["a"].as_str().unwrap(), "plain-jwt");
        assert_eq!(raw["b"]["nested"].as_i64().unwrap(), 1);
    }

    #[test]
    fn keyid_派生_与客户端算法一致() {
        // 客户端实现：hex(SHA256(key))[0..16]
        let key = [0u8; 32];
        let h = Sha256::digest(&key);
        let expect: String = h[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(derive_key_id(&key), expect);
        assert_eq!(derive_key_id(&key).len(), KEY_ID_LEN);
    }

    /// 真机端到端（#[ignore]，手工运行）：读取本机真实 auth 文件走完整解密链
    /// （探针提密钥 → 信封解密 → 原位还原），输出仅元数据（统计/uid 掩码/JWT 形态判定），
    /// 绝不输出任何凭证明文。issue #198 真机复验入口：`cargo test 真机 -- --ignored`
    #[test]
    #[ignore]
    fn 真机_本机auth文件端到端解密() {
        let path = std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")
            .join("workbuddy-desktop.info");
        let raw_text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                println!("auth 文件不存在，跳过: {e}");
                return;
            }
        };
        let mut raw: Value =
            serde_json::from_str(raw_text.trim_start_matches('\u{feff}')).unwrap();
        let stats = decrypt_auth_file_fields(&mut raw);
        println!(
            "stats: decrypted={} failed={} asym={} last_error={:?}",
            stats.decrypted, stats.failed, stats.unsupported_asym, stats.last_error
        );
        let access = raw["auth"]["accessToken"].as_str().unwrap_or_default();
        println!(
            "accessToken: len={} jwt_shape={}",
            access.len(),
            access.starts_with("eyJ") && access.split('.').count() == 3
        );
        let uid = raw["account"]["uid"].as_str().or(raw["uid"].as_str()).unwrap_or_default();
        println!("uid 掩码: {}…（{} 位）", &uid[..uid.len().min(8)], uid.len());
        assert!(stats.failed == 0, "解密存在失败: {:?}", stats.last_error);
        assert!(access.starts_with("eyJ"), "accessToken 未还原为 JWT 字符串形态");
    }
}
