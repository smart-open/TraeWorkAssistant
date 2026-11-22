use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::State;

use crate::fs_utils;
use crate::jwt;
use crate::models::{DeviceEntry, DeviceMap, RawAccount};
use crate::state::AppState;

/// 最近签发的 OAuth 登录会话（CSRF 防护 + PKCE）：oauth_get_login_url 签发时记录，
/// oauth_parse_callback 用 loginTraceID 双向绑定校验（抓包实证：login_trace_id 参数
/// 会被授权页原样回传为回调的 loginTraceID），PKCE verifier 供 AuthCode 交换。
/// device_id：本会话选定的凭证设备（issue #76 轮转 / issue #110 合成身份）——
/// 登录 URL 参数、ExchangeToken DeviceInfo、device_map 回写三者共用同一台，保证同源。
#[derive(Clone)]
struct PendingLogin {
    state: String,
    pkce_verifier: String,
    device_id: String,
}
static LAST_OAUTH_STATE: Mutex<Option<PendingLogin>> = Mutex::new(None);

/// 按账号持久化的合成设备身份（issue #110：多账号全绑同一台设备时服务端签到
/// 按 x-device-id 每日去重，除首个账号外全部撞车）。uid 是键，设备私钥只存
/// 本机自有数据库，不进日志/事件。`{prefix}pending` 为会话暂存（登录 URL 签发
/// 时生成，oauth_login 成功解析 uid 后转正为 `{prefix}{uid}`）。
const SYNTHETIC_DEVICE_KEY_PREFIX: &str = "oauth_synthetic_device:";

/// OAuth 常量（2026-09-16 抓包固化）：client_id 取真实 Trae IDE 登录 URL 实证值
/// （授权页 native_ide 流程 GetPCAuthCode 接受；旧值 en1oxy7wnw8j9n 会让页面
/// 停留在 billing status 后无后续，不回跳）。conf/oauth_client.json 可覆盖。
const OAUTH_CLIENT_ID: &str = "ono9krqynydwx5";
const OAUTH_CLIENT_SECRET: &str = "-";
/// 客户端应用 id（tech-framework §6.2：云 IDE API 附带 X-App-Id 头；旧登录 URL 曾用）
const OAUTH_APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
/// 真实 IDE 页面参数快照（抓包 2026-09-16）：授权页据此进入 native_ide 原生授权
/// 流程（前端调 GetPCAuthCode 后 302 回 auth_callback_url）
const OAUTH_PAGE_PLUGIN_VERSION: &str = "2.3.83560";
const OAUTH_PAGE_APP_VERSION: &str = "3.3.100";
const OAUTH_PAGE_PLATFORM_CODE: &str = "IDE_PC";
/// 本机回环监听端口（F-78 批次 1：commands/oauth_loopback.rs 在此端口收 OAuth 回调）
pub(crate) const OAUTH_LOOPBACK_PORT: u16 = 17388;
pub(crate) const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:17388/authorize";
const OAUTH_EXCHANGE_URL: &str = "https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken";

/// OAuth 客户端凭证外置配置（缺陷10）：conf/oauth_client.json 可覆盖
/// client_id / client_secret / exchange_url（上游更换凭证或端点时无需发版）。
/// 文件缺失或字段缺省回退内置默认；进程内 OnceLock 缓存，修改后需重启应用生效。
#[derive(serde::Deserialize, Clone)]
pub struct OAuthClientConfig {
    #[serde(default = "default_client_id")]
    pub client_id: String,
    #[serde(default = "default_client_secret")]
    pub client_secret: String,
    #[serde(default = "default_exchange_url")]
    pub exchange_url: String,
}

impl Default for OAuthClientConfig {
    fn default() -> Self {
        Self {
            client_id: default_client_id(),
            client_secret: default_client_secret(),
            exchange_url: default_exchange_url(),
        }
    }
}

fn default_client_id() -> String {
    OAUTH_CLIENT_ID.to_string()
}
fn default_client_secret() -> String {
    OAUTH_CLIENT_SECRET.to_string()
}
fn default_exchange_url() -> String {
    OAUTH_EXCHANGE_URL.to_string()
}

/// OAuth 域外置资源根目录解析（F-75 跨平台收口）：`AIWORK_DATA_DIR` 环境变量优先
/// （已设置且非空 → 直接作为应用数据目录，conf/、certs/、logs/ 均在其下；
/// 容器/自定义部署挂载点），未设置回退平台数据根 + DATA_DIR_NAME
/// （Windows=%APPDATA%，mac=~/Library/Application Support；桌面行为不变）；
/// 两者皆不可得返回 None。
// 审查修复（外置 OAuth 目录）：原 oauth_client 与 exchange_agent 两处恒读 %APPDATA%，
// Docker 容器内 APPDATA 恒空 → 外置 oauth_client.json / 本地 CA 永远读不到且无日志；
// 现抽本函数统一两处口径（AIWORK_DATA_DIR 优先）。macOS 兼容：回退根经
// platform::app_support_root 跨平台解析，替代原 Windows 专用 APPDATA 直读。
// 注意：state.rs::new 的数据目录解析独立（同样跨平台），本函数仅覆盖 OAuth 域这两处调用
fn oauth_data_root() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("AIWORK_DATA_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir));
        }
    }
    crate::platform::app_support_root()
        .ok()
        .map(|d| d.join(crate::state::DATA_DIR_NAME))
}

/// 读取外置 OAuth 客户端配置（全局一次；缺失/损坏回退默认值）
pub fn oauth_client() -> &'static OAuthClientConfig {
    static CFG: std::sync::OnceLock<OAuthClientConfig> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        // 审查修复（外置 OAuth 目录，融合 F-75 跨平台收口）：目录解析走 oauth_data_root
        // （AIWORK_DATA_DIR 优先，回退平台数据根 Windows=%APPDATA% / mac=Application
        // Support），回落内置默认时补一条轻量日志不再静默（进程级 OnceLock 仅记一条）
        match oauth_data_root() {
            Some(root) => {
                let p = root.join("conf").join("oauth_client.json");
                if !p.exists() {
                    fs_utils::app_log(
                        &root,
                        &format!(
                            "[OAuth] 外置 OAuth 客户端配置缺失（{}），回落内置默认 client_id/secret",
                            p.display()
                        ),
                    );
                }
                fs_utils::read_json::<OAuthClientConfig>(&p)
            }
            None => {
                // 数据目录不可解析时 app.log 无处可写，走 stderr（容器日志可见）兜底
                eprintln!(
                    "[OAuth] 数据目录不可解析（AIWORK_DATA_DIR 未设置且平台数据根不可得），外置 OAuth 客户端配置不可用，回落内置默认"
                );
                OAuthClientConfig::default()
            }
        }
    })
}

/// OAuth 回调解析结果
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OAuthCallbackInfo {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub avatar: Option<String>,
    /// 本次登录会话使用的凭证设备（issue #76 轮转）：oauth_login 据此走刷新
    /// 交换与 device_map 回写，保证整条登录链路设备同源
    #[serde(default)]
    pub device_id: Option<String>,
}

/// OAuth 登录 URL 响应
#[derive(Serialize)]
pub struct OAuthLoginUrl {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
}

/// OAuth 登录完成后的账号信息
#[derive(Serialize)]
pub struct OAuthLoginResult {
    pub user_id: String,
    pub name: String,
    pub jwt: String,
    pub refresh_token: String,
    pub has_refresh_token: bool,
    /// 本次登录是否与已有账号同 uid（合并更新而非新增）
    pub merged: bool,
    /// 合并时被更新的已有账号名（merged=false 时为 None）
    pub existing_name: Option<String>,
}

/// 短请求 Agent（项目未启用 ureq 的 proxy-from-env feature，Agent 默认直连）
fn short_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 交换/用户信息请求 Agent（F-78 批次 3 抓包调试）：
/// 默认与 short_agent 同款直连；设置环境变量 AIWORK_OAUTH_DEBUG_PROXY（值如
/// `127.0.0.1:8899`，即本软件 MITM 代理端口）时改走该代理并信任本地 CA
/// （AIWORK_DATA_DIR/certs/ca.crt，未设置时 %APPDATA%/AIWorkAssistant/certs/ca.crt，
/// 目录解析见 oauth_data_root），
/// ExchangeToken/GetUserInfo 流量落入代理日志（api.trae.com.cn 已默认入抓包域名），
/// 供抓包固化 client_secret 校验行为与 refresh_token 轮换语义。
/// 仅影响 OAuth 交换链路，签到/续期等其余直连请求不受影响。
fn exchange_agent() -> Result<ureq::Agent, String> {
    let addr = std::env::var("AIWORK_OAUTH_DEBUG_PROXY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(addr) = addr else {
        return Ok(short_agent());
    };
    // 调试代理容错（2026-09-16 实测）：设置了调试代理但尚未启动过代理（CA 未生成）
    // 时降级直连并记日志，不阻断登录闭环；只有 CA 存在但损坏才视为错误
    // 审查修复（外置 OAuth 目录，融合 F-75 跨平台收口）：CA 目录解析改走 oauth_data_root
    // （AIWORK_DATA_DIR 优先，回退平台数据根），与外置 oauth_client.json 同口径；
    // 降级直连逻辑不变
    let dir = oauth_data_root();
    let ca_path = dir.as_ref().map(|d| d.join("certs").join("ca.crt"));
    let ca_pem = match ca_path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(p) => p,
        None => {
            if let Some(d) = dir.as_ref() {
                fs_utils::app_log(
                    d,
                    "[OAuth] AIWORK_OAUTH_DEBUG_PROXY 已设置但本地 CA 缺失/读取失败，交换请求降级直连；如需抓包请先启动一次代理生成证书",
                );
            }
            return Ok(short_agent());
        }
    };
    let der = pem_cert_der(&ca_pem)?;
    let mut roots = ureq::rustls::RootCertStore::empty();
    roots
        .add(ureq::rustls::pki_types::CertificateDer::from(der))
        .map_err(|e| format!("本地 CA 载入失败: {e}"))?;
    let config = ureq::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(4)
        .proxy(
            ureq::Proxy::new(&addr)
                .map_err(|e| format!("AIWORK_OAUTH_DEBUG_PROXY 值无效（{addr}）: {e}"))?,
        )
        .tls_config(std::sync::Arc::new(config))
        .build())
}

/// PEM(CERTIFICATE) → DER（与 device_proxy/ca.rs::pem_to_der 同实现，避免跨模块 pub 暴露）
fn pem_cert_der(pem: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let body = pem
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|s| s.split("-----END CERTIFICATE-----").next())
        .ok_or_else(|| "本地 CA 文件缺少 CERTIFICATE PEM 块".to_string())?;
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("本地 CA base64 解码失败: {e}"))
}

/// 生成随机 hex 字符串（安全场景入口：OAuth state / PKCE verifier / 签名 nonce）。
/// 熵源：OS CSPRNG（Windows=BCryptGenRandom 系统首选 RNG；mac/其他平台=uuid v4
/// 底层 getrandom CSPRNG，F-75 M2-2.5——旧 LCG/哈希熵链以时间戳作种子输出可预测，
/// 不适合安全场景（审查 P1-6）——安全路径已全部移除弱实现）。CSPRNG 失败返回 Err
/// 上抛为用户可见错误：不 panic（审查修复——panic 在异步命令任务内会中止任务，
/// 前端 invoke 永不 resolve），也不静默降级为可预测随机。
#[cfg(windows)]
pub(crate) fn random_hex_result(len: usize) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut bytes = vec![0u8; len.div_ceil(2)];
    let halg: windows_sys::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE =
        unsafe { std::mem::zeroed() };
    // STATUS_SUCCESS == 0
    let status = unsafe {
        BCryptGenRandom(halg, bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    if status != 0 {
        return Err(format!(
            "系统熵池不可用（BCryptGenRandom status={status:#x}），已拒绝生成随机数"
        ));
    }
    Ok(hex_encode(&bytes, len))
}

/// 非 Windows：mac/其他平台走 uuid v4（getrandom CSPRNG，永失败）——
/// macos_main 合并适配：安全场景同样走 CSPRNG 主路径，弱哈希熵链仅保留给
/// [random_hex] 非安全场景兜底，不用于安全令牌。
#[cfg(not(windows))]
pub(crate) fn random_hex_result(len: usize) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(len.div_ceil(2) + 16);
    while bytes.len() < len.div_ceil(2) {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    Ok(hex_encode(&bytes, len))
}

/// 非安全场景便捷封装（request-id / trace-id / 设备号等仅需唯一性）：
/// CSPRNG 不可用时退化为哈希熵链（唯一性有保障，时间可预测性可接受），
/// 不 panic 不上抛。安全场景（state/PKCE/nonce）必须走 [random_hex_result]
/// 并上抛错误（审查 P1-6：熵链不可预测性弱于 CSPRNG，勿用于安全令牌）。
pub(crate) fn random_hex(len: usize) -> String {
    random_hex_result(len).unwrap_or_else(|_| hash_hex_fallback(len))
}

/// 字节 → 定长 hex 字符串（截断到 len）
fn hex_encode(bytes: &[u8], len: usize) -> String {
    let mut out = String::with_capacity(len);
    for b in bytes {
        if out.len() >= len {
            break;
        }
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        if out.len() >= len {
            break;
        }
        out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
    }
    out
}

/// 哈希熵链回退（sha256：时间纳秒 + pid + 计数器迭代扩展）。仅保证唯一性，
/// 供 [random_hex] 非安全场景兜底（CSPRNG 不可用时；mac 上 uuid v4 恒成功，
/// 本函数实际只在 Windows BCrypt 失败时触达）。
fn hash_hex_fallback(len: usize) -> String {
    use sha2::{Digest, Sha256};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = Sha256::new();
    h.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_le_bytes())
            .unwrap_or([0u8; 16]),
    );
    h.update(std::process::id().to_le_bytes());
    h.update(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_le_bytes());
    let mut digest = h.finalize();
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in digest.iter() {
            if out.len() >= len {
                break;
            }
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
            if out.len() >= len {
                break;
            }
            out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
        }
        let mut h2 = Sha256::new();
        h2.update(digest);
        h2.update(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_le_bytes());
        digest = h2.finalize();
    }
    out
}

/// OAuth 登录设备标识（F-78 批次 3）：持久化于 data/oauth_device.json。
/// 原实现每次随机生成 machine_id/device_id，与 device_map.json 的账号稳定伪设备漂移，
/// OAuth 换发的 JWT 绑定设备与签到用设备不一致，存在被服务端判定异动/顶替的风控隐患。
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct OAuthDevice {
    pub machine_id: String,
    pub device_id: String,
}

/// 读取（缺失则生成并写回）本机稳定的 OAuth 设备标识。
/// device_id 优先对齐 device_map.json 已有条目（登录前无法预知账号，取 user_id 字典序
/// 最小的条目作为本机基准，保证不再每次随机）；machine_id 无对应字段，首次随机后固定。
/// 注意：不回写 device_map.json——DeviceEntry 三元组（device_id/market_user_id/session_id）
/// 与 OAuth 二元组字段语义不同，部分写入会破坏签到脚本的完整三元组假设。
/// issue #76 后本函数退化为「无本机凭证时的兜底路径」；有凭证的登录/刷新走
/// select_login_device / exchange_token_refresh 的凭证轮转选择。
fn load_or_create_oauth_device(state: &AppState) -> OAuthDevice {
    // SQLite 化（P2）：oauth_device.json → kv `oauth_device`
    let store = crate::store::db(&state.data_dir);
    let mut dev: OAuthDevice = store.kv_get("oauth_device");
    // device_id 首选 icube 设备凭证（F-78 DeviceProof）：签名私钥与 icube-dc
    // deviceId 绑定，登录 URL 的 device_id 必须与之同源，否则服务端 20403/20405；
    // 恒覆盖旧值（旧值是随机/device_map 对齐的，与私钥不匹配）
    if let Some(cred) = icube_device_creds().first() {
        if dev.device_id != cred.device_id {
            dev.device_id = cred.device_id.clone();
            let _ = store.kv_set("oauth_device", &dev);
        }
    }
    if dev.machine_id.is_empty() || dev.device_id.is_empty() {
        if dev.device_id.is_empty() {
            let map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(&state.data_dir));
            if let Some((_, entry)) = map.iter().min_by_key(|(k, _)| k.as_str()) {
                if !entry.device_id.is_empty() {
                    dev.device_id = entry.device_id.clone();
                }
            }
        }
        if dev.machine_id.is_empty() {
            dev.machine_id = random_hex(32);
        }
        if dev.device_id.is_empty() {
            dev.device_id = (0..15)
                .map(|_| {
                    let n = (random_hex(2).chars().next().unwrap() as u8).wrapping_rem(10);
                    (b'0' + n) as char
                })
                .collect();
        }
        // 写回失败不阻断登录（下次重新生成，仅损失一次稳定性）
        let _ = store.kv_set("oauth_device", &dev);
    }
    dev
}

// ── 凭证轮转（issue #76：服务端每设备账号绑定数上限）────────────────────────
//
// 服务端按 device_id 统计账号绑定数，超限后该设备上新登录的账号能换到 token
// 但所有使用类接口被拒（「该设备绑定的账户数量已达上限」）。历史实现登录/刷新
// 恒用 icube_device_creds().first()，所有 OAuth 账号在服务端全部绑到同一台设备。
// 本机各 Trae 客户端（Trae CN / TRAE SOLO CN / Trae / Trae Work）各有一套独立
// 凭证，按「已绑定账号数最少」轮转选择即可成倍分散绑定上限。

/// 按 device_id 查找凭证（DeviceInfo/DeviceProof 的设备与签名私钥必须同源，
/// 20403/20405 实测拒绝），三级匹配：
/// 1. `{prefix}pending` 会话暂存身份——登录窗口内 synthetic 尚未绑定 uid（uid 要等
///    JWT 解析后才可知），必须先按 pending 设备 ID 直接命中，供 AuthCode 交换使用
/// 2. 本机真实客户端凭证
/// 3. device_map 反查 uid → `{prefix}{uid}` per-uid synthetic（刷新链路消费）
/// 匹配不到返回 None 由调用方按协议变体处理（不再全局兜底首个凭证，防设备漂移）
fn find_cred_by_device(
    state: &AppState,
    device_id: &str,
) -> Option<crate::icube_auth::DeviceCredential> {
    let store = crate::store::db(&state.data_dir);
    let pending: serde_json::Value =
        store.kv_get(&format!("{SYNTHETIC_DEVICE_KEY_PREFIX}pending"));
    if let Some(cred) = crate::icube_auth::device_credential_from_json(&pending)
        .filter(|c| c.device_id == device_id)
    {
        return Some(cred);
    }
    let cred = icube_device_creds()
        .iter()
        .find(|c| c.device_id == device_id)
        .cloned();
    if cred.is_some() {
        return cred;
    }
    let map = crate::store::docs::device_map_load(&store);
    // 反查确定性说明（审查 L3）：多 uid 共享同一 device_id（存量共享条目）时 find
    // 命中顺序不确定，但 per-uid synthetic 设备 ID 全局唯一，至多一条能通过末端
    // filter 校验——命中错 uid 时末端校验不通过返回 None，语义不受影响。
    // 依赖该不变量，勿改成「首个命中即返回」
    let uid = map
        .iter()
        .find(|(_, e)| e.device_id == device_id)
        .map(|(uid, _)| uid.clone())?;
    let saved: serde_json::Value = store.kv_get(&format!("{SYNTHETIC_DEVICE_KEY_PREFIX}{uid}"));
    crate::icube_auth::device_credential_from_json(&saved).filter(|c| c.device_id == device_id)
}

/// 统计本机各凭证设备的账号绑定数（轮转选择依据，纯函数供单测）：
/// - device_map 条目 device_id 命中凭证 → 计入该凭证
/// - 伪设备条目（derive/代理捕获）或无条目账号 → 计入首个凭证：历史 OAuth
///   登录/刷新恒用首个凭证，服务端绑定即在它；伪设备条目多为签到路径落盘的
///   派生设备（get_device_for），其账号的 server 侧绑定仍是首个凭证。手动导入
///   账号被高估计入首凭证仅影响轮转起点（多避让），无风险
fn credential_binding_counts(
    creds: &[crate::icube_auth::DeviceCredential],
    map: &DeviceMap,
    account_uids: &[Option<String>],
) -> std::collections::HashMap<String, usize> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let Some(first) = creds.first() else {
        return counts;
    };
    let cred_ids: std::collections::HashSet<&str> =
        creds.iter().map(|c| c.device_id.as_str()).collect();
    for uid in account_uids.iter().flatten() {
        let target = match map.get(uid) {
            Some(e) if cred_ids.contains(e.device_id.as_str()) => e.device_id.as_str(),
            _ => first.device_id.as_str(),
        };
        *counts.entry(target.to_string()).or_insert(0) += 1;
    }
    counts
}

/// 选择绑定数最少的凭证下标（并列取凭证表序靠前者，保证同状态选择稳定）
fn select_credential_index(
    creds: &[crate::icube_auth::DeviceCredential],
    counts: &std::collections::HashMap<String, usize>,
) -> Option<usize> {
    creds
        .iter()
        .enumerate()
        .min_by_key(|(i, c)| (counts.get(&c.device_id).copied().unwrap_or(0), *i))
        .map(|(i, _)| i)
}

/// 混合轮转决策（issue #76 轮转 + issue #110 synthetic 隔离，纯函数供单测）：
/// 真实凭证中存在绑定数 0 的 → 返回该凭证下标（并列取表序靠前者）；全部真实
/// 凭证均已绑定账号或本机无凭证 → None，调用方走 synthetic 生成路径——每台设备
/// 只绑 1 个账号才不会触发服务端签到按 x-device-id 的每日去重（issue #110 根因）
fn select_unbound_credential(
    creds: &[crate::icube_auth::DeviceCredential],
    map: &DeviceMap,
    account_uids: &[Option<String>],
) -> Option<usize> {
    if creds.is_empty() {
        return None;
    }
    let counts = credential_binding_counts(creds, map, account_uids);
    let i = select_credential_index(creds, &counts)?;
    if counts.get(&creds[i].device_id).copied().unwrap_or(0) == 0 {
        Some(i)
    } else {
        None
    }
}

/// 状态版混合轮转决策：从 AppState 读取凭证表与账号绑定分布后决策
///（select_login_device 与 fallback_login_device_id 共用，保证两处同状态同决策）
fn unbound_credential_index(state: &AppState) -> Option<usize> {
    let creds = icube_device_creds();
    let store = crate::store::db(&state.data_dir);
    let map = crate::store::docs::device_map_load(&store);
    let uids: Vec<Option<String>> = crate::vault::load_accounts(state)
        .accounts
        .iter()
        .map(|a| a.user_id.clone())
        .collect();
    select_unbound_credential(creds, &map, &uids)
}

/// 选定本次登录使用的设备（混合轮转）：
/// 1. 真实凭证中有未绑定的 → 用它（issue #76 轮转），machine_id 取凭证自带值
/// 2. 全部真实凭证已绑定或本机无凭证 → 为本次登录会话生成全新 synthetic 身份
///   （等价在「新本机」上重新安装 IDE 登录），oauth_login 成功拿到 uid 后持久化
///    为 per-uid 身份；生成失败（CSPRNG 不可用等）回落 kv 持久化设备
///   （load_or_create_oauth_device，与历史行为一致）
fn select_login_device(state: &AppState) -> (String, String) {
    if let Some(i) = unbound_credential_index(state) {
        let cred = &icube_device_creds()[i];
        return (cred.device_id.clone(), cred.machine_id.clone());
    }
    match generate_and_persist_login_device(state) {
        Ok(cred) => (cred.device_id, cred.machine_id),
        Err(e) => {
            fs_utils::app_log(&state.data_dir, &format!("OAuth: 合成设备身份生成失败: {e}"));
            let dev = load_or_create_oauth_device(state);
            (dev.device_id, dev.machine_id)
        }
    }
}

/// 为当前 OAuth 登录会话生成一套合成设备身份并暂存 `{prefix}pending`。
/// 只生成/暂存，不绑定账号；绑定发生在 oauth_login 成功解析 uid 后
///（take_pending_login_device → persist_account_synthetic_device）。
fn generate_and_persist_login_device(
    state: &AppState,
) -> Result<crate::icube_auth::DeviceCredential, String> {
    let cred = crate::icube_auth::generate_synthetic_device_credential(OAUTH_PAGE_APP_VERSION)?;
    let json = serde_json::to_value(&cred).map_err(|e| format!("合成设备身份序列化失败: {e}"))?;
    crate::store::db(&state.data_dir)
        .kv_set(&format!("{SYNTHETIC_DEVICE_KEY_PREFIX}pending"), &json)
        .map_err(|e| format!("合成设备身份写入失败: {e}"))?;
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "OAuth: 无未绑定真实凭证，已为登录会话生成合成设备身份（device_id={}，来源 synthetic）",
            cred.device_id
        ),
    );
    Ok(cred)
}

/// 无在途会话（重启后粘贴回调）时的 fallback 设备：上次登录决策若为 synthetic
///（无未绑定真实凭证），优先复用登录 URL 签发时持久化的 pending 身份——跨重启
/// 存活，与授权 URL device_id 同源，避免重新生成覆盖导致交换设备漂移；
/// 真实凭证有未绑定时按轮转规则重选（选择确定性，重启不改变绑定数分布）；
/// 两者皆不可得才走 select_login_device 生成新身份（旧身份已无法与本次回调对应，
/// 属可接受损失）
fn fallback_login_device_id(state: &AppState) -> String {
    if unbound_credential_index(state).is_none() {
        // 上次登录决策必为 synthetic：复用存活的 pending（与授权 URL device_id 同源）
        let pending: serde_json::Value = crate::store::db(&state.data_dir)
            .kv_get(&format!("{SYNTHETIC_DEVICE_KEY_PREFIX}pending"));
        if let Some(cred) = crate::icube_auth::device_credential_from_json(&pending) {
            return cred.device_id;
        }
    }
    select_login_device(state).0
}

/// pending 合成身份预取（只读不删，审查 L1 修复）：设备匹配则返回凭证，交由调用方
/// **先持久化 per-uid 身份、成功后再删 pending**——旧顺序「先删 pending 再 persist」
/// 在 persist 失败时两头皆失（pending 已删、per-uid 缺失、device_map 已回写该设备，
/// 三级匹配全未命中致后续刷新失配）；反转后 persist 失败时 pending 仍存活，
/// 刷新可经第一级命中，设备身份不丢
fn peek_pending_login_device(
    state: &AppState,
    device_id: &str,
) -> Option<crate::icube_auth::DeviceCredential> {
    let key = format!("{SYNTHETIC_DEVICE_KEY_PREFIX}pending");
    let store = crate::store::db(&state.data_dir);
    let saved: serde_json::Value = store.kv_get(&key);
    let cred = crate::icube_auth::device_credential_from_json(&saved)?;
    if cred.device_id != device_id {
        return None;
    }
    Some(cred)
}

/// pending 暂存键删除（转正收尾；失败仅落日志——残留 pending 与 per-uid 同设备
/// 同凭证，刷新经第一级命中语义等价，且会被下次登录生成的新身份覆盖，无害）
fn delete_pending_login_device(state: &AppState, device_id: &str) {
    let key = format!("{SYNTHETIC_DEVICE_KEY_PREFIX}pending");
    if crate::store::db(&state.data_dir).kv_delete(&key).is_err() {
        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth: pending 合成身份删除失败（device_id={device_id}），残留键将被下次登录覆盖，无碍"),
        );
    }
}

/// 将合成身份持久化为指定账号的设备身份（kv `{prefix}{uid}`，供刷新签 DeviceProof）
pub(crate) fn persist_account_synthetic_device(
    state: &AppState,
    uid: &str,
    cred: &crate::icube_auth::DeviceCredential,
) -> Result<(), String> {
    fs_utils::ensure_uid_safe(uid)?;
    let json = serde_json::to_value(cred).map_err(|e| format!("合成设备身份序列化失败: {e}"))?;
    crate::store::db(&state.data_dir)
        .kv_set(&format!("{SYNTHETIC_DEVICE_KEY_PREFIX}{uid}"), &json)
        .map_err(|e| format!("合成设备身份写入失败: {e}"))
}

/// 删除账号的合成设备身份（account_delete 生命周期清理；失败静默——孤儿键仅占存储，
/// device_map 条目已删后永远不会被反查命中）
pub(crate) fn remove_account_synthetic_device(state: &AppState, uid: &str) {
    if fs_utils::ensure_uid_safe(uid).is_err() {
        return;
    }
    let _ = crate::store::db(&state.data_dir)
        .kv_delete(&format!("{SYNTHETIC_DEVICE_KEY_PREFIX}{uid}"));
}

/// OAuth 登录成功后回写 device_map（issue #76 次因修复）：登录/交换签发的新 JWT
/// 绑定本次登录设备，签到/积分查询经 get_device_for/resolve_device 取 x-device-id
/// 必须与之同源——此前 OAuth 路径明确不回写，签到路径对无条目账号按 uid 派生另一台
/// 伪设备，2026-09 实测新 JWT 严格校验设备指纹时被拒（老 JWT 宽容放行掩盖了漂移）。
/// market_user_id/session_id 无本地来源：无既有条目时按 uid 派生补全三元组（派生
/// device_id 弃用）；已有条目保留原 market/session 值（不覆盖 device_proxy 捕获的
/// 真实三元组中的非关键字段），device_id 始终以最新登录设备为准（新 JWT 校验口径）
fn writeback_oauth_device(state: &AppState, uid: &str, device_id: &str) {
    if uid.is_empty() || device_id.is_empty() {
        return;
    }
    let store = crate::store::db(&state.data_dir);
    // C3（审查）：整表 load→save 持 DEVICE_MAP_LOCK（叶子锁）——签到
    // get_device_for 写回分支并发时防整表覆盖丢条目
    let _g = crate::store::docs::DEVICE_MAP_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut map = crate::store::docs::device_map_load(&store);
    let entry = map.entry(uid.to_string()).or_insert_with(|| {
        let d = crate::commands::accounts::derive_device(uid);
        DeviceEntry {
            device_id: String::new(),
            market_user_id: d.market_user_id,
            session_id: d.session_id,
        }
    });
    if entry.device_id != device_id {
        entry.device_id = device_id.to_string();
        // 写回失败不阻断登录（下次登录重写，仅损失一次指纹同源）
        let _ = crate::store::docs::device_map_save(&store, &map);
        fs_utils::app_log(
            &state.data_dir,
            &format!("[OAuth登录] 回写 device_map[{uid}] device_id={device_id}（JWT 与签到设备指纹同源）"),
        );
    }
}

/// 生成 PKCE code_verifier（RFC 7636：43-128 字符非 reserved 字符；hex 64字符合规）
/// 与 S256 code_challenge（BASE64URL-NOPAD(SHA256(verifier))）。
/// 安全令牌走 [random_hex_result]：CSPRNG 不可用上抛 Err，不静默降级
fn pkce_pair() -> Result<(String, String), String> {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let verifier = random_hex_result(64)?;
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Ok((verifier, challenge))
}

/// 生成 OAuth 登录 URL（2026-09-16 抓包固化：对齐真实 Trae IDE 登录页参数形态）。
/// 旧形态（client_secret/app_id/response_type=code）会让授权页停在
/// cn_credits_billing_status 后无后续、不回跳；真实流程：
/// login_channel=native_ide → 页面前端调 GetPCAuthCode（绑定 PKCE challenge）→
/// 302 回 auth_callback_url，回调参数为 authCodeInfo（JSON）而非 refreshToken/code。
#[tauri::command]
pub fn oauth_get_login_url(state: State<AppState>) -> Result<OAuthLoginUrl, String> {
    let state = &*state;
    // issue #76 + #110：登录设备 = 未绑定的真实凭证（轮转分散每设备绑定上限）；
    // 全部真实凭证已绑定或无凭证时为本次登录生成 synthetic 身份（每设备 1 账号）
    let (device_id, machine_id) = select_login_device(state);
    // login_trace_id 兼作 CSRF 绑定值（抓包实证：授权页原样回传为回调 loginTraceID）；
    // 安全场景 CSPRNG fail-closed（审查 P3：random_hex 为 fail-open，熵源故障时
    // 会退化为非安全值，CSRF 绑定值不可降级）
    let trace_id = random_hex_result(32)?;
    let (pkce_verifier, code_challenge) = pkce_pair()?;

    // 主机名兜底（F-75 M2-2.5）：Windows 读 COMPUTERNAME；mac 无此环境变量，
    // 恒落 "Windows-PC"——改 scutil --get ComputerName 取值（scutil 失败回退
    // hostname 命令，仍失败回落 macOS-Device）
    let hostname = host_fallback();
    fn host_fallback() -> String {
        #[cfg(windows)]
        {
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows-PC".into())
        }
        #[cfg(target_os = "macos")]
        {
            crate::platform::cmd::sys_command("scutil")
                .arg("--get")
                .arg("ComputerName")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    // main 侧改进：scutil 失败回退 hostname 命令取真实主机名
                    crate::platform::cmd::sys_command("hostname")
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "macOS-Device".into())
                })
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            "Unknown-Device".into()
        }
    }
    // 设备类型/OS 版本按平台分派（审查 H2）：此前 mac 也硬编码 windows/Windows，
    // 与 mac 设备凭证指纹（machine_id/hostname 均已按 mac 取值）自相矛盾，
    // 设备指纹严格校验时有触发风控（20403）的实际风险。
    // 注意：mac 侧取值（x_device_type=mac / "macOS <ver>"）暂为最贴近的合理值，
    // 真实客户端上报形态待抓包核实——若不一致仅需调整本函数。
    fn device_platform_fields() -> (String, String) {
        #[cfg(windows)]
        {
            ("windows".into(), "Windows".into())
        }
        #[cfg(target_os = "macos")]
        {
            let ver = crate::platform::cmd::sys_command("sw_vers")
                .arg("-productVersion")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            let os_version = match ver {
                Some(v) => format!("macOS {v}"),
                None => "macOS".into(),
            };
            ("mac".into(), os_version)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            ("unknown".into(), "Unknown".into())
        }
    }
    let (device_type, os_version) = device_platform_fields();
    let url = format!(
        "https://www.trae.cn/authorization?\
        login_version=1\
        &auth_from=trae\
        &login_channel=native_ide\
        &plugin_version={plugin_version}\
        &auth_type=local\
        &client_id={client_id}\
        &redirect=0\
        &login_trace_id={trace_id}\
        &auth_callback_url={redirect_uri}\
        &machine_id={machine_id}\
        &device_id={device_id}\
        &x_device_id={device_id}\
        &x_machine_id={machine_id}\
        &x_device_brand={hostname}\
        &x_device_type={device_type}\
        &x_os_version={os_version}\
        &x_env=\
        &x_app_version={app_version}\
        &x_app_type=stable\
        &code_challenge={code_challenge}\
        &code_challenge_method=S256\
        &channel_name=common",
        plugin_version = OAUTH_PAGE_PLUGIN_VERSION,
        client_id = oauth_client().client_id,
        trace_id = trace_id,
        redirect_uri = urlencoding::encode(OAUTH_REDIRECT_URI),
        machine_id = machine_id,
        device_id = device_id,
        hostname = urlencoding::encode(&hostname),
        device_type = device_type,
        os_version = urlencoding::encode(&os_version),
        app_version = OAUTH_PAGE_APP_VERSION,
        code_challenge = code_challenge,
    );

    // 记录本机登录会话（CSRF + PKCE + 会话设备）供回调校验/交换/回写使用
    {
        let mut guard = LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(PendingLogin { state: trace_id.clone(), pkce_verifier, device_id: device_id.clone() });
    }

    Ok(OAuthLoginUrl {
        url,
        state: trace_id,
        redirect_uri: OAUTH_REDIRECT_URI.to_string(),
    })
}

/// 解析 OAuth 回调 URL。
/// P1 审查修复：exchange_code 是分钟级网络交换（见下方调用点注释），同步命令
/// 会把整个 UI 事件循环冻结同样时长——改为 async 命令（对齐 oauth.rs:1683 与
/// accounts.rs refresh_jwt 的既有口径），Tauri 在独立线程执行不阻塞主线程。
#[tauri::command(async)]
pub fn oauth_parse_callback(
    state: State<AppState>,
    callback_url: String,
) -> Result<OAuthCallbackInfo, String> {
    // 回调 URL 格式：http://127.0.0.1:port/authorize?refreshToken=xxx&accessToken=xxx&userId=xxx&userName=xxx&avatar=xxx
    // 或可能带 code 参数需要交换
    let query_str = callback_url
        .split('?')
        .nth(1)
        .ok_or_else(|| "回调 URL 中缺少查询参数".to_string())?;

    let mut params: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pair in query_str.split('&') {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("").to_string();
        let value = kv.next().unwrap_or("").to_string();
        // URL decode
        let decoded = urlencoding::decode(&value)
            .map(|c| c.to_string())
            .unwrap_or(value);
        params.insert(key, decoded);
    }

    let user_id = params
        .get("userId")
        .or_else(|| params.get("user_id"))
        .or_else(|| params.get("UserID"))
        .cloned();

    let user_name = params
        .get("userName")
        .or_else(|| params.get("user_name"))
        .or_else(|| params.get("nickname"))
        .cloned();

    let avatar = params.get("avatar").cloned();

    // 抓包固化（2026-09-16）主路径：授权页 native_ide 流程 302 回调携带
    // authCodeInfo=<URL编码JSON>{"AuthCode","ExpireAt","ExpireDuration"} 与
    // userInfo=<JSON>{"UserID","ScreenName","AvatarUrl",...}、loginTraceID、host、
    // userRegion——从这两个 JSON 里补全身份信息（免调 GetUserInfo）
    let mut auth_code: Option<String> = None;
    if let Some(raw) = params.get("authCodeInfo") {
        let v: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| format!("authCodeInfo 解析失败（{e}）：授权页回调格式异常"))?;
        let code = v
            .get("AuthCode")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("authCodeInfo 中缺少 AuthCode 字段")?
            .to_string();
        auth_code = Some(code);
    }
    if let Some(raw) = params.get("userInfo") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
            if user_id.is_none() {
                if let Some(uid) = v.get("UserID").and_then(|x| x.as_str()) {
                    params.insert("UserID".into(), uid.to_string());
                }
            }
            if let Some(name) = v
                .get("ScreenName")
                .or_else(|| v.get("NickName"))
                .and_then(|x| x.as_str())
            {
                params.insert("userName".into(), name.to_string());
            }
            if let Some(ava) = v.get("AvatarUrl").and_then(|x| x.as_str()) {
                params.insert("avatar".into(), ava.to_string());
            }
        }
    }
    // 上面 params 插入后重新取值（保持下游逻辑单一出口）
    let user_id = params.get("UserID").cloned().or(user_id);
    let user_name = params.get("userName").cloned().or(user_name);
    let avatar = params.get("avatar").cloned().or(avatar);

    // 审查修复：入口一次性快照整份会话。此前 state/verifier/device_id 分 5 处独立
    // lock 读取，exchange_code（分钟级网络调用）横跨两个 lock 窗口，重叠登录会话
    // （oauth_get_login_url 会整体覆盖写入）会让回调透传的 device_id 与实际交换
    // 设备错配 → device_map 静默污染。快照后本次回调全部读值同源，竞态消除。
    let session = LAST_OAUTH_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let issued_state = session.as_ref().map(|p| p.state.clone());
    let issued_verifier = session.as_ref().map(|p| p.pkce_verifier.clone());
    let issued_device_id = session.as_ref().map(|p| p.device_id.clone());
    // 审查修复：无在途会话（重启后粘贴回调）的回退设备入口选定一次，供下方
    // AuthCode 交换与尾部透传共用——此前两处独立 select_login_device 横跨
    // exchange_code（分钟级网络调用）窗口，重叠登录会话（oauth_get_login_url
    // 整体覆盖写入 LAST_OAUTH_STATE）会让两处选中不同设备 → 交换设备与
    // device_map 回写/刷新交换设备错配。选择是确定性的，同状态下两次调用
    // 结果一致，去重后仅消除窗口期漂移，行为不变。
    // fallback 走 fallback_login_device_id（issue #110）：pending 合成身份优先
    // 复用，其次才 select_login_device 生成新身份
    let fallback_device = issued_device_id
        .clone()
        .unwrap_or_else(|| fallback_login_device_id(&state));

    // CSRF 校验（抓包固化：state 已不适用，native_ide 流程回调不回传 state，改用
    // loginTraceID 双向绑定——授权页把 login_trace_id 原样回传为 loginTraceID）。
    // 本进程签发过登录会话且回调携带 loginTraceID 时两者必须一致；回调不带
    // loginTraceID 或本进程未签发过（重启后粘贴回调）时保持宽容，不阻断正常登录。
    if let Some(cb_trace) = params.get("loginTraceID").or_else(|| params.get("login_trace_id")) {
        if let Some(expected) = &issued_state {
            if !expected.is_empty() && cb_trace != expected {
                return Err("OAuth loginTraceID 校验失败：回调 URL 与本机发起的登录请求不匹配（可能为伪造或重放），已拒绝".into());
            }
        }
    } else if auth_code.is_some() {
        // 新流程回调必带 loginTraceID：缺失且本机有在途会话时视为不匹配
        if issued_state.is_some() {
            return Err("OAuth 回调缺少 loginTraceID，无法确认与本机登录请求的对应关系，已拒绝".into());
        }
    }

    let mut access_token = params
        .get("accessToken")
        .or_else(|| params.get("access_token"))
        .cloned();

    // 主路径（抓包固化）：authCodeInfo.AuthCode → ExchangeToken（带 PKCE verifier）。
    // 兼容路径：refreshToken 直传（老形态）、code 参数（标准授权码）
    let refresh_token = match params
        .get("refreshToken")
        .or_else(|| params.get("refresh_token"))
        .cloned()
    {
        Some(t) => t,
        None => {
            let code = auth_code
                .or_else(|| params.get("code").cloned())
                .ok_or_else(|| "回调 URL 中缺少 authCodeInfo/refreshToken 参数".to_string())?;
            let verifier = issued_verifier.as_deref();
            // 会话设备优先（登录 URL 签发时轮转选定）；无在途会话（重启后粘贴回调）
            // 用入口选定的 fallback_device（与回调尾部透传同一台，见上方审查修复注释）
            let device_id = fallback_device.clone();
            // host：授权页回传的 API 域（main.js 逆向：交换 URL = ${host}/trae/api/v3/oauth/ExchangeToken）
            let host = params
                .get("host")
                .cloned()
                .unwrap_or_else(|| "https://api.trae.com.cn".into());
            match exchange_code(&code, verifier, &device_id, &host, &state) {
                Ok((at, rt)) => {
                    access_token = Some(at);
                    rt
                }
                Err(e) => {
                    return Err(format!(
                        "AuthCode 交换失败（{e}）；可复制完整回调 URL 与 app.log 中的交换诊断反馈排查，或改用手动登录兜底"
                    ));
                }
            }
        }
    };

    Ok(OAuthCallbackInfo {
        refresh_token,
        access_token,
        user_id,
        user_name,
        avatar,
        // 会话设备透传给 oauth_login：刷新交换与 device_map 回写共用同一台
        //（与 AuthCode 交换共用入口选定值，防分钟级交换窗口内两处漂移）
        device_id: Some(fallback_device),
    })
}

/// 用 AuthCode 交换 token。
/// 协议形态（2026-09-16 Trae CN main.js 逆向固化）：
/// - 端点：`${host}/trae/api/v3/oauth/ExchangeToken`（host 为授权页回传的 API 域）
/// - 请求体：{ClientID, AuthCode, CodeVerifier, DeviceInfo{...含 DevicePublicKey}, IDEVersion}
///   ——AuthCode 场景**不发 DeviceProof**（那是 refreshToken 刷新场景专属），
///   设备身份通过 DeviceInfo（DeviceID+DevicePublicKey）声明
/// - 请求头：仅 Content-Type + x-cloudide-token: ""（空串；真实客户端 m() 方法原样）
/// - 响应：Result.Token / Result.RefreshToken
/// 保留旧形态变体作兜底探测（协议固化验证后移除）。
/// 交换端点 host 白名单（审查 m9）：host 来自用户粘贴的回调 URL 参数，直接拼接为
/// ExchangeToken 端点——AuthCode + PKCE CodeVerifier 均属安全令牌，必须限定官方
/// API 域，防伪造回调把令牌发往任意第三方主机。host 口径与默认值 / bypass.rs
/// 已登记域一致（api.trae.com.cn 为主路径）。
const EXCHANGE_HOST_ALLOWLIST: [&str; 3] = ["api.trae.com.cn", "api.trae.cn", "api.trae.com"];

fn validate_exchange_host(host: &str) -> Result<(), String> {
    // 形如 "https://api.trae.com.cn"（可带路径/尾斜杠）；无 scheme 时按裸域处理
    let bare = host
        .split("://")
        .nth(1)
        .unwrap_or(host)
        .trim_end_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    let host_only = bare.rsplit('@').next().unwrap_or(bare).to_ascii_lowercase();
    let host_only = host_only.split(':').next().unwrap_or("");
    if EXCHANGE_HOST_ALLOWLIST.contains(&host_only) {
        Ok(())
    } else {
        Err(format!(
            "回调 URL 的 host 参数不在官方 API 域白名单内（{host_only}），已拒绝交换以防令牌外发"
        ))
    }
}

fn exchange_code(code: &str, verifier: Option<&str>, device_id: &str, host: &str, state: &AppState) -> Result<(String, String), String> {
    validate_exchange_host(host)?;
    let client_id = oauth_client().client_id.clone();
    let code_v = code.to_string();
    let verifier_v = verifier.unwrap_or("").to_string();
    let device_v = device_id.to_string();

    let mut variants: Vec<(String, String, serde_json::Value, bool)> = Vec::new();

    // issue #76 + #110：凭证按会话选定 device_id 三级匹配（DeviceInfo.DeviceID 与
    // 签名私钥必须同源）。匹配不到不兜底首个凭证（防设备漂移）——仅剩无凭证
    // 旧形态变体，如实报错
    let cred = find_cred_by_device(state, device_id);

    // 主变体（真实客户端形态）：DeviceInfo 对象 + IDEVersion，无 DeviceProof
    if let Some(ref cred) = cred {
        let pub_pem = crate::icube_auth::device_public_key_pem(cred)
            .unwrap_or_default();
        let url = format!("{}/trae/api/v3/oauth/ExchangeToken", host.trim_end_matches('/'));
        variants.push((
            "ExchangeToken/DeviceInfo".into(),
            url,
            ureq::json!({
                "ClientID": client_id,
                "AuthCode": code_v,
                "CodeVerifier": verifier_v,
                "DeviceInfo": {
                    "DeviceID": cred.device_id,
                    "MachineID": cred.machine_id,
                    "PlatformCode": OAUTH_PAGE_PLATFORM_CODE,
                    "DeviceType": "PC",
                    "DeviceName": "",
                    "DeviceModel": "",
                    "ClientVersion": cred.app_version,
                    "DevicePublicKey": pub_pem,
                    "DeviceBrand": "",
                    "DeviceCPU": "",
                    "OSInfo": "",
                    "OSVersion": "",
                },
                "IDEVersion": cred.app_version,
            }),
            // 真实客户端 m("") 头形态：x-cloudide-token 必须带空串（F-70 实测缺失报 20403）
            true,
        ));
    }

    // 兜底：旧形态探测变体（DeviceProof 是 refreshToken 刷新场景的结构，AuthCode
    // 场景按逆向结论不应携带；保留以验证逆向结论，全部失败时错误码进 app.log）
    if let Some(ref cred) = cred {
        let proof_path_com = "/cloudide/api/v3/trae/oauth/ExchangeToken";
        for (fmt, tag_base, url) in [
            (
                crate::icube_auth::ProofSigFormat::P1363,
                "ExchangeToken/AuthCode+Proof",
                oauth_client().exchange_url.clone(),
            ),
            (
                crate::icube_auth::ProofSigFormat::Der,
                "ExchangeToken/AuthCode+Proof",
                oauth_client().exchange_url.clone(),
            ),
        ] {
            if let Ok(proof) = crate::icube_auth::device_proof(cred, proof_path_com, &client_id, code, fmt) {
                variants.push((
                    format!("{}{}", tag_base, fmt.suffix()),
                    url.clone(),
                    ureq::json!({
                        "ClientID": client_id,
                        "AuthCode": code_v,
                        "CodeVerifier": verifier_v,
                        "DeviceID": cred.device_id,
                        "PlatformCode": OAUTH_PAGE_PLATFORM_CODE,
                        "DeviceProof": proof,
                    }),
                    true,
                ));
            }
        }
    }

    // 无 proof 兜底变体（旧形态对照）
    let mk = |tag: &str, auth_key: &str, url: String| {
        (
            tag.to_string(),
            url,
            ureq::json!({
                "ClientID": client_id,
                auth_key: code_v,
                "CodeVerifier": verifier_v,
                "DeviceID": device_v,
                "PlatformCode": OAUTH_PAGE_PLATFORM_CODE,
            }),
            false,
        )
    };
    variants.push(mk("ExchangeToken/AuthCode", "AuthCode", oauth_client().exchange_url.clone()));
    variants.push(mk("ExchangeToken/Code", "Code", oauth_client().exchange_url.clone()));

    let mut errs: Vec<String> = Vec::new();
    for (tag, url, payload, with_proof_header) in &variants {
        match try_exchange_variant(tag, url, payload.clone(), device_id, *with_proof_header, &state.data_dir) {
            Ok(t) => {
                fs_utils::app_log(&state.data_dir, &format!("[OAuth交换] 变体 {tag} 成功（协议固化前保留探测链）"));
                return Ok(t);
            }
            Err(e) => errs.push(format!("{tag}: {e}")),
        }
    }
    Err(format!("全部交换变体失败 → {}", errs.join(" | ")))
}

/// 本机 Trae 客户端 icube 设备凭证（进程内缓存；首次调用扫描 storage.json）。
/// 只返回真实客户端凭证——synthetic 身份按 uid 持久化（issue #110），经
/// find_cred_by_device 三级匹配命中，本函数不提供「全局首个凭证」兜底，
/// 防刷新/签到悄悄落回共享设备
fn icube_device_creds() -> &'static [crate::icube_auth::DeviceCredential] {
    static CREDS: std::sync::OnceLock<Vec<crate::icube_auth::DeviceCredential>> = std::sync::OnceLock::new();
    CREDS.get_or_init(crate::icube_auth::extract_device_credentials)
}

/// 单个交换变体尝试：脱敏日志（请求+响应全量）→ 设备头请求 → 火山信封错误解析 →
/// token 三级提取（容器精确键 → 全树深挖 → 键路径诊断）
fn try_exchange_variant(
    tag: &str,
    url: &str,
    payload: serde_json::Value,
    device_id: &str,
    with_proof_header: bool,
    data_dir: &std::path::Path,
) -> Result<(String, String), String> {
    {
        let mut masked = payload.clone();
        mask_sensitive(&mut masked);
        fs_utils::app_log(data_dir, &format!("[OAuth交换-请求:{tag}] {masked}"));
    }
    let mut req = exchange_agent()?
        .post(url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        // 设备头（F-70 情报：ExchangeToken 专属错误码 20403=Device not match /
        // 20405=Device proof required → 交换与设备绑定强相关，请求须携带设备标识）
        .set("x-device-id", device_id)
        .set("x-app-id", OAUTH_APP_ID)
        .set("x-platform-code", OAUTH_PAGE_PLATFORM_CODE);
    if with_proof_header {
        // F-70 实测：x-cloudide-token 必须为空字符串——带旧 token 报 20405，
        // 完全不带该头报 20403
        req = req.set("x-cloudide-token", "");
    }
    let resp = match req.send_json(payload) {
        Ok(r) => r,
        // 4xx/5xx：ureq 返回 Error::Status 且响应体仍可读——保留用于诊断
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("请求失败: {e}")),
    };

    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => return Err(format!("解析响应失败: {e}")),
    };
    {
        let mut masked = body.clone();
        mask_sensitive(&mut masked);
        fs_utils::app_log(data_dir, &format!("[OAuth交换-响应:{tag}] {masked}"));
    }

    // 火山引擎标准信封（2026-09-16 实测）：错误 ResponseMetadata.Error.{Code,Message,
    // StandardCode}；成功 ResponseMetadata + Result.{...}（对照 GetPCAuthCode 形态）
    if let Some((code, msg, std_code)) = volcano_error(&body) {
        if code != "0" {
            return Err(format!(
                "code={code}/{}: {}",
                std_code,
                if msg.is_empty() { "未知错误" } else { &msg }
            ));
        }
    }
    // 兼容旧解析（顶层 code/message 形态）
    let code_val = body.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
    if code_val != 0 {
        let msg = body.get("message").and_then(|v| v.as_str()).unwrap_or("未知错误");
        return Err(format!("code={code_val}: {msg}"));
    }

    // token 提取三级：Result/data/Data 容器精确键 → 全树深挖（键名变体大小写不敏感）→
    // 失败记键路径（脱敏不含值）
    const ACCESS_KEYS: &[&str] = &["AccessToken", "access_token", "token", "Jwt", "JWT"];
    const REFRESH_KEYS: &[&str] = &["RefreshToken", "refresh_token"];
    let container = body
        .get("Result")
        .or_else(|| body.get("data"))
        .or_else(|| body.get("Data"));
    if let Some(d) = container {
        let a = find_token_value(d, ACCESS_KEYS);
        let r = find_token_value(d, REFRESH_KEYS);
        if let (Some(a), Some(r)) = (a, r) {
            if !a.is_empty() && !r.is_empty() {
                return Ok((a, r));
            }
        }
    }
    match (
        find_token_value(&body, ACCESS_KEYS),
        find_token_value(&body, REFRESH_KEYS),
    ) {
        (Some(a), Some(r)) if !a.is_empty() && !r.is_empty() => Ok((a, r)),
        _ => {
            let mut paths = Vec::new();
            collect_key_paths_public(&body, &mut paths);
            fs_utils::app_log(
                data_dir,
                &format!(
                    "[OAuth交换:{tag}] 响应未找到 Token 字段（无 Error 信封），响应键路径: {}",
                    paths.join(" | ")
                ),
            );
            Err("响应中未找到 AccessToken/RefreshToken 字段（键路径已记入 app.log）".to_string())
        }
    }
}

/// 火山引擎信封错误解析（2026-09-16 实测形态）：
/// `ResponseMetadata.Error.{Code,Message,StandardCode}`。
///
/// 定级红线（2026-10-06 缺陷修复）：`fs_utils::dig()` 是「按候选键名全树查找」而
/// **不是路径遍历**——写成 `dig(&body, &["ResponseMetadata", "Error", "Code"])` 时
/// 第一个键就命中 `ResponseMetadata` 对象，`as_i64()/as_str()` 双双 None → 字符串化
/// 得空串 → 判定 `!err_code.is_empty()` 恒假，**整个信封分支是死代码**。实测后果：
/// 20101（refresh token is invalid）被降级成「响应中未找到 AccessToken 字段」，并按
/// 「网络类，不计失效」计数，失效凭证永不置 `refresh_token_invalid`（既不提示重新
/// 登录，也不触发本地登录态恢复）。
///
/// 现改为两段式：先用 `dig` 按候选键名定位 `Error` 对象（兼容被 data/result 等任意
/// 包裹键包一层、或直接挂在顶层），再对 Error 对象逐字段严格取值。
/// 返回 `(code, message, standard_code)`；无信封或 Code 缺失时返回 None。
fn volcano_error(body: &serde_json::Value) -> Option<(String, String, String)> {
    let err = body
        .get("Error")
        .or_else(|| crate::fs_utils::path(body, &["ResponseMetadata", "Error"]))
        .or_else(|| crate::fs_utils::dig(body, &["Error"]))?;
    let code = err
        .get("Code")
        .map(|v| {
            v.as_i64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(|s| s.trim().to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if code.is_empty() {
        return None;
    }
    let field = |key: &str| -> String {
        err.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Some((code, field("Message"), field("StandardCode")))
}

/// refresh_token 失效判定：仅 `20101`（StandardCode 040012，Message
/// 「Token 无效：refresh token is invalid.」）是服务端对 refresh_token 的**明确否定**，
/// 采纳为 `server_rejected=true`（立即置 invalid + 停止逐变体探测）；其余信封码
/// （20403/20405 等设备与协议级问题）凭证可能仍有效，不当失效处理。
/// Message 兜底匹配大小写不敏感，防上游改码不改语义。
fn refresh_token_rejected(code: &str, message: &str) -> bool {
    code == "20101" || message.to_ascii_lowercase().contains("refresh token is invalid")
}

/// 脱敏红线（仅打码值不删结构）：键名含 token/jwt/secret/password/authcode/code/credential
/// 的字符串值替换为「前6位…(长度N)」——错误码为数字不受影响，诊断信息完整保留
fn mask_sensitive(v: &mut serde_json::Value) {
    const SENSITIVE: &[&str] = &[
        "token", "jwt", "secret", "password", "authcode", "credential", "verifier", "code",
    ];
    fn is_sensitive_key(k: &str) -> bool {
        let kl = k.to_ascii_lowercase();
        SENSITIVE.iter().any(|s| kl.contains(s))
    }
    fn mask_str(s: &str) -> String {
        // 审查修复：短纯数字串（≤12 位）为错误码等业务标识（如 Error.Code="20101"），
        // 打码会毁掉排障关键信息——豁免不打码；真实凭证（token/jwt 等）远长于此，不受影响
        if !s.is_empty() && s.len() <= 12 && s.bytes().all(|b| b.is_ascii_digit()) {
            return s.to_string();
        }
        let head: String = s.chars().take(6).collect();
        format!("{head}…(len={})", s.chars().count())
    }
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if is_sensitive_key(k) {
                    if let Some(s) = val.as_str() {
                        if !s.is_empty() {
                            // 审查 L：短纯数字串不打码——Error.Code/StandardCode 实测为
                            // 字符串形态数字（"20101"），打码后 [OAuth交换-响应] 日志
                            // 失去最关键的错误码；真实凭证（授权码/令牌）恒为长随机串，
                            // 长度上限 12 + 纯数字双条件确保不会误放行
                            if s.len() <= 12 && s.chars().all(|c| c.is_ascii_digit()) {
                                mask_sensitive(val);
                                continue;
                            }
                            *val = serde_json::Value::String(mask_str(s));
                            continue;
                        }
                    }
                    if val.is_object() || val.is_array() {
                        continue; // Error.Code 等嵌套结构另走递归（数字 code 不打码）
                    }
                }
                mask_sensitive(val);
            }
        }
        serde_json::Value::Array(a) => {
            for x in a.iter_mut() {
                mask_sensitive(x);
            }
        }
        _ => {}
    }
}

/// 在 JSON 树中递归查找指定键（大小写不敏感）的首个非空字符串值（仅取值，不落日志）
fn find_token_value(v: &serde_json::Value, keys: &[&str]) -> Option<String> {    match v {
        serde_json::Value::Object(m) => {
            // 精确匹配优先，其次大小写不敏感
            for k in keys {
                if let Some(val) = m.get(*k).and_then(|x| x.as_str()) {
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
            for (mk, val) in m {
                if keys.iter().any(|k| k.eq_ignore_ascii_case(mk)) {
                    if let Some(s) = val.as_str() {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
            for val in m.values() {
                if let Some(found) = find_token_value(val, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_token_value(x, keys)),
        _ => None,
    }
}

/// 键路径收集（oauth.rs 本地版，脱敏：仅键名不含值）
fn collect_key_paths_public(v: &serde_json::Value, out: &mut Vec<String>) {
    fn rec(v: &serde_json::Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
        if depth > 6 || out.len() >= 40 {
            return;
        }
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    out.push(p.clone());
                    rec(val, &p, depth + 1, out);
                }
            }
            serde_json::Value::Array(a) => {
                for (i, x) in a.iter().enumerate().take(3) {
                    rec(x, &format!("{prefix}[{i}]"), depth + 1, out);
                }
            }
            _ => {}
        }
    }
    rec(v, "", 0, out)
}

/// refresh_token 换取 access_token 的失败详情：
/// server_rejected=true 表示服务端明确拒绝（旧形态数字 code != 0），refresh_token 已确定失效；
/// 网络/解析/错误信封等本地与协议级失败为 false，不应误标失效（B 判定收窄，app.log 实证
/// 旧协议对失效请求返回无 code/message 的异构响应，被 unwrap_or(-1) 误判为「未知错误」拒绝）
pub(crate) struct RefreshExchangeError {
    pub msg: String,
    pub server_rejected: bool,
}

/// 用 refresh_token 换取 access_token（JWT 刷新路径，2026-09-16 协议迁移）。
/// 与 OAuth 登录链路同源的固化协议：`${host}/trae/api/v3/oauth/ExchangeToken`
/// + DeviceProof 签名（签名原文 POST\n<path>\n<ClientID>\n<RefreshToken>\n<ts>\n<nonce>）
/// + x-cloudide-token: "" 空头，响应 Result.Token/Result.RefreshToken（火山信封）。
/// 旧协议（cloudide/api/v3/trae 端点 + ClientSecret 体）保留为兜底探测变体。
/// 成功返回 (access_token, Option<新 refresh_token>（可能轮换）, 原始响应体)
/// `device_id_override`：账号绑定的设备（issue #76）——device_map 条目或本次登录
/// 会话设备。命中凭证（本机客户端或 per-uid synthetic，find_cred_by_device 三级匹配）
/// 时用该凭证做 DeviceProof/DeviceID（刷新与登录同设备）；未命中（伪设备条目/存量
/// 共享设备）**不回退首个凭证**（防设备漂移），仅走无凭证旧协议变体如实报错；
/// 请求 DeviceID 仍走 kv oauth_device → 本机设备（既有 20403 修复口径）
pub(crate) fn exchange_token_refresh(
    state: &AppState,
    refresh_token: &str,
    device_id_override: Option<&str>,
) -> Result<(String, Option<String>, serde_json::Value), RefreshExchangeError> {
    let client_id = oauth_client().client_id.clone();
    // matched 只在「override 确实命中凭证」时为 Some——cred 与 matched 同值，
    // 未命中走无凭证变体如实报错，不兜底首个凭证（防设备漂移）
    let matched_cred = device_id_override.and_then(|d| find_cred_by_device(state, d));
    let cred = matched_cred.clone();
    let dev = load_or_create_oauth_device(state);
    let device_id = matched_cred
        .as_ref()
        .map(|c| c.device_id.clone())
        .unwrap_or_else(|| dev.device_id.clone());

    // 变体链（主→兜底）：固化协议 P1363/DER → 旧端点 P1363 → 旧协议（无凭证时唯一路径）
    let new_url = "https://api.trae.com.cn/trae/api/v3/oauth/ExchangeToken";
    let mut variants: Vec<(String, String, serde_json::Value, bool)> = Vec::new();
    if let Some(cred) = cred {
        for (fmt, url, sign_path) in [
            (
                crate::icube_auth::ProofSigFormat::P1363,
                new_url,
                "/trae/api/v3/oauth/ExchangeToken",
            ),
            (
                crate::icube_auth::ProofSigFormat::Der,
                new_url,
                "/trae/api/v3/oauth/ExchangeToken",
            ),
            (
                crate::icube_auth::ProofSigFormat::P1363,
                OAUTH_EXCHANGE_URL,
                "/cloudide/api/v3/trae/oauth/ExchangeToken",
            ),
        ] {
            let Ok(proof) =
                crate::icube_auth::device_proof(&cred, sign_path, &client_id, refresh_token, fmt)
            else {
                continue;
            };
            variants.push((
                format!("Refresh/Proof{}", fmt.suffix()),
                url.to_string(),
                ureq::json!({
                    "ClientID": client_id,
                    "RefreshToken": refresh_token,
                    "DeviceID": cred.device_id,
                    "PlatformCode": OAUTH_PAGE_PLATFORM_CODE,
                    "DeviceProof": proof,
                }),
                true,
            ));
        }
    }
    // 旧协议兜底（保留至固化协议验证期结束）
    variants.push((
        "Refresh/Legacy".into(),
        oauth_client().exchange_url.clone(),
        ureq::json!({
            "ClientID": client_id,
            "RefreshToken": refresh_token,
            "ClientSecret": oauth_client().client_secret,
            "UserID": "",
        }),
        false,
    ));

    let mut last_err: Option<RefreshExchangeError> = None;
    for (tag, url, payload, with_proof_header) in &variants {
        match try_refresh_variant(tag, url, payload.clone(), &device_id, *with_proof_header, &state.data_dir) {
            Ok(t) => return Ok(t),
            Err(e) => {
                // 旧形态数字 code != 0：服务端明确拒绝，立即采纳不再探测
                if e.server_rejected {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or(RefreshExchangeError {
        msg: "无可用刷新变体（本机未发现 Trae 设备凭证且旧协议未启用）".into(),
        server_rejected: false,
    }))
}

/// 单个刷新变体尝试：脱敏日志（请求+响应全量）→ 设备头请求 → 宽容解析。
/// 与 try_exchange_variant（AuthCode 场景）的差异：
/// - refresh_token 可能不轮换：响应缺 RefreshToken 视为成功（调用方保留旧值）
/// - 仅旧形态数字 code != 0 判为「服务端明确拒绝」（server_rejected=true）；
///   火山错误信封（20403/20405 等设备/协议错误）视为变体失败继续探测
fn try_refresh_variant(
    tag: &str,
    url: &str,
    payload: serde_json::Value,
    device_id: &str,
    with_proof_header: bool,
    data_dir: &std::path::Path,
) -> Result<(String, Option<String>, serde_json::Value), RefreshExchangeError> {
    {
        let mut masked = payload.clone();
        mask_sensitive(&mut masked);
        fs_utils::app_log(data_dir, &format!("[OAuth刷新-请求:{tag}] {masked}"));
    }
    let agent = match exchange_agent() {
        Ok(a) => a,
        Err(e) => {
            return Err(RefreshExchangeError {
                msg: e,
                server_rejected: false,
            })
        }
    };
    let mut req = agent
        .post(url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("x-device-id", device_id)
        .set("x-app-id", OAUTH_APP_ID)
        .set("x-platform-code", OAUTH_PAGE_PLATFORM_CODE);
    if with_proof_header {
        // F-70 实测：x-cloudide-token 必须为空字符串
        req = req.set("x-cloudide-token", "");
    }
    let resp = match req.send_json(payload) {
        Ok(r) => r,
        // 4xx/5xx：ureq 返回 Error::Status 且响应体仍可读——保留用于诊断
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            return Err(RefreshExchangeError {
                msg: format!("请求失败: {e}"),
                server_rejected: false,
            })
        }
    };
    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return Err(RefreshExchangeError {
                msg: format!("解析响应失败: {e}"),
                server_rejected: false,
            })
        }
    };
    {
        let mut masked = body.clone();
        mask_sensitive(&mut masked);
        fs_utils::app_log(data_dir, &format!("[OAuth刷新-响应:{tag}] {masked}"));
    }

    // 火山信封错误分流（2026-10-06 缺陷修复）：
    // - 20101（refresh token is invalid）= 服务端对凭证的明确否定 → 采纳为 rejected，
    //   立即置 invalid 并停止逐变体探测（旧实现在此处恒不成立，见 volcano_error 注释）；
    // - 其余（20403 Device not match / 20405 Device proof required 等）属设备/协议级失败，
    //   凭证可能仍有效 → rejected=false，继续探测下一变体。
    if let Some((code, msg, std_code)) = volcano_error(&body) {
        if code != "0" {
            let rejected = refresh_token_rejected(&code, &msg);
            return Err(RefreshExchangeError {
                msg: format!(
                    "code={code}/{}: {}",
                    std_code,
                    if msg.is_empty() { "未知错误" } else { &msg }
                ),
                server_rejected: rejected,
            });
        }
    }
    // 旧形态数字 code：唯一可信的「服务端明确拒绝」信号（B 判定收窄——
    // 无 code 字段的异构响应不再被 unwrap_or(-1) 误判为拒绝）
    if let Some(c) = body.get("code").and_then(|v| v.as_i64()) {
        if c != 0 {
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误");
            return Err(RefreshExchangeError {
                msg: format!("code={c}: {msg}"),
                server_rejected: true,
            });
        }
    }

    // token 提取：access 必须有（Result/data/Data 容器 → 全树深挖），refresh 可缺省
    const ACCESS_KEYS: &[&str] = &["AccessToken", "access_token", "token", "Jwt", "JWT"];
    const REFRESH_KEYS: &[&str] = &["RefreshToken", "refresh_token"];
    let access = find_token_value(&body, ACCESS_KEYS).filter(|s| !s.is_empty());
    let refresh = find_token_value(&body, REFRESH_KEYS).filter(|s| !s.is_empty());
    match access {
        Some(a) => Ok((a, refresh, body)),
        None => {
            let mut paths = Vec::new();
            collect_key_paths_public(&body, &mut paths);
            fs_utils::app_log(
                data_dir,
                &format!(
                    "[OAuth刷新:{tag}] 响应未找到 Token 字段（无 Error 信封），响应键路径: {}",
                    paths.join(" | ")
                ),
            );
            Err(RefreshExchangeError {
                msg: "响应中未找到 AccessToken 字段（键路径已记入 app.log）".into(),
                server_rejected: false,
            })
        }
    }
}

/// GetUserInfo：获取用户信息
fn get_user_info(access_token: &str) -> Result<(String, String), String> {
    let auth = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    let resp = exchange_agent()?
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/GetUserInfo")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({}))
        .map_err(|e| format!("GetUserInfo 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let data = body.get("data").or(body.get("result")).ok_or("响应中缺少 data 字段")?;

    let user_id = data
        .get("user_id")
        .or_else(|| data.get("UserID"))
        .or_else(|| data.get("userId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let user_name = data
        .get("name")
        .or_else(|| data.get("user_name"))
        .or_else(|| data.get("userName"))
        .or_else(|| data.get("nickname"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((user_id, user_name))
}

/// 取 uid 尾 6 位用于报错辨认，避免完整 uid 出现在提示中
fn tail6(s: &str) -> String {
    s.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect()
}

/// OAuth 登录闭环：解析回调 → 换取 accessToken → 获取用户信息 → 保存账号
// async：内含最多两次 120s 超时的串行网络请求（exchange_token/get_user_info），同步命令会冻结 UI（审查修复）
#[tauri::command(async)]
pub fn oauth_login(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    callback_url: String,
    account_name: Option<String>,
    group_id: Option<String>,
    // 重新登录模式：指定目标账号 uid。授权账号 uid 与之不一致时拒绝落库，
    // 防止浏览器里登错账号导致凭证合到其他账号/误新增
    expect_uid: Option<String>,
) -> Result<OAuthLoginResult, String> {
    // 1. 解析回调 URL
    let callback_info = oauth_parse_callback(state.clone(), callback_url)?;

    // issue #76：本次登录会话设备（回调签发时轮转选定，回调解析已透传；
    // 极端场景下缺失则走 fallback_login_device_id——pending 合成身份优先复用
    // （与授权 URL device_id 同源），其次轮转/生成新身份，避免直接 select_login_device
    // 覆盖 pending 导致交换设备漂移；与 oauth_parse_callback L866 回退同构）。
    // 登录 URL 参数 / ExchangeToken DeviceInfo / 刷新交换 / device_map 回写全链路同源
    let login_device_id = callback_info
        .device_id
        .clone()
        .unwrap_or_else(|| fallback_login_device_id(&state));

    // 2. 如果回调中没有 accessToken，则用 refresh_token 换取
    // 审查修复（M12，融合 M8）：交换失败原经 .map_err(|e| e.msg).ok() 压扁，恒报
    // 「refresh_token 交换失败」通用文案，上游信封详情丢失排查失据；现透传
    // RefreshExchangeError.msg（内含 volcano_error 解析的火山信封 code/msg 或
    // 旧形态 message/请求失败原因）
    let new_pair: Result<(String, Option<String>, serde_json::Value), String> =
        match callback_info.access_token {
            Some(ref at) => Ok((at.clone(), None, serde_json::Value::Null)),
            None => {
                // 此处 refresh_token 刚从 OAuth 回调取得（非存量失效凭证），
                // server_rejected 分支仅转为错误信息，不涉及生命周期标记
                exchange_token_refresh(&state, &callback_info.refresh_token, Some(&login_device_id))
                    .map(|(a, r, b)| (a, r, b))
                    .map_err(|e| e.msg)
            }
        };
    let (access_token, new_refresh_token, exchange_body) =
        new_pair.map_err(|e| format!("refresh_token 交换失败: {e}"))?;

    // P1（issue #201）：登录交换响应若携带 refresh_token 过期时间则落库，与刷新路径
    //（accounts.rs ExchangeToken 写回）同源提取（兼容秒/毫秒时间戳），消除
    // refresh_token_expires_at=None 盲区——惰性门的 rt 临期续命判定依赖该字段
    // 审查补充（issue #201 复审#1）：key 列表源自 Buddy/Qoder 跨平台类推，无 Trae
    // 侧抓包对照证据；dig 未命中时落一条响应顶层键清单日志（掩码由 log 前置链路
    // 处理，此处仅列键名不含值），便于用真实日志实测校准 key 列表
    let refresh_token_expires_at = crate::fs_utils::dig(
        &exchange_body,
        &[
            "RefreshExpireAt",
            "refresh_token_expires_at",
            "refresh_expires_at",
            "refreshTokenExpiresAt",
            "refresh_expires_at_ms",
        ],
    )
    .and_then(|v| v.as_i64())
    .map(|exp| if exp > 10_000_000_000 { exp / 1000 } else { exp });
    if refresh_token_expires_at.is_none() && exchange_body.is_object() {
        let top_keys: Vec<&str> = exchange_body
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if !top_keys.is_empty() {
            fs_utils::app_log(
                &state.data_dir,
                &format!("[OAuth登录-exchange键清单] 未命中 rt 过期字段，响应顶层键: {top_keys:?}"),
            );
        }
    }

    // 3. 规范化 JWT 格式
    let jwt = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.clone()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    // 4. 解析 JWT 获取 user_id（B3，审查 2026-10-13：以 JWT 解析值为权威 uid——
    // 回调 URL 参数可被用户粘贴/构造篡改，此前 URL 参数优先，构造 userId=<目标uid>
    // + 他人 code 即可绕过 expect_uid 校验把他人凭证写入目标账号。JWT 由授权服务
    // 签发不可篡改；两者并存且不一致时拒绝，仅 JWT 有值或 URL 参数恰好一致才放行）
    let jwt_info = jwt::parse(&jwt);
    let jwt_uid = jwt_info
        .user_id
        .clone()
        .ok_or_else(|| "无法从 JWT 中解析 user_id".to_string())?;
    let callback_uid = callback_info
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(cb_uid) = callback_uid {
        if cb_uid != jwt_uid {
            return Err(format!(
                "回调 URL 中的账号 uid（尾号 {}）与凭证 JWT 解析结果（尾号 {}）不一致，已拒绝——请确认回调 URL 完整未被修改",
                tail6(cb_uid),
                tail6(&jwt_uid)
            ));
        }
    }
    let user_id = jwt_uid;

    // 重新登录防呆：行内「重新登录」携带目标账号 uid，
    // 浏览器误登其他账号时直接拒绝——此刻尚未做任何落库/设备回写，零副作用。
    // 取 uid 尾 6 位用于报错辨认，避免完整 uid 出现在提示中；
    // expect_uid 先 trim，空串视为未指定（前端对空输入曾回传 Some("")，导致重登必失败）
    if let Some(expected) = expect_uid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if expected != user_id {
            return Err(format!(
                "登录账号与目标账号不一致：本次授权的账号 uid 尾号 {}，目标账号 uid 尾号 {}。\
                 已取消且未改动任何数据，请确认浏览器中登录的是同一账号后重试（若浏览器当前为其他账号，请先退出登录）",
                tail6(&user_id),
                tail6(expected)
            ));
        }
    }

    // 4.5 issue #76 次因修复：新 JWT 绑定本次登录设备，立即回写 device_map——
    // 签到/积分查询经 get_device_for/resolve_device 取该设备，x-device-id 与 JWT
    // 同源（此前不回写，签到路径派生另一台伪设备，新 JWT 严格校验指纹时被拒）
    writeback_oauth_device(&state, &user_id, &login_device_id);

    // issue #110：本次登录若使用会话合成身份（无未绑定真实凭证），成功拿到 uid 后
    // 转正为 per-uid 身份，供后续刷新签 DeviceProof；同 uid 重新 OAuth 即换新设备
    //（覆盖旧键，兑现「重新 OAuth 登录轮换设备凭证」的错误转译承诺）。
    // 登录落在真实凭证上时清掉遗留的 per-uid synthetic（死状态清理）
    if let Some(cred) = peek_pending_login_device(&state, &login_device_id) {
        // 审查 L1：先 persist 后删 pending——persist 失败时保留 pending（刷新可经
        // 第一级命中），不再出现「pending 已删 + per-uid 缺失」的双失状态
        match persist_account_synthetic_device(&state, &user_id, &cred) {
            Ok(()) => delete_pending_login_device(&state, &login_device_id),
            Err(e) => fs_utils::app_log(
                &state.data_dir,
                &format!("OAuth: 账号合成设备身份持久化失败（pending 保留，刷新可经会话身份命中）: {e}"),
            ),
        }
    } else if icube_device_creds().iter().any(|c| c.device_id == login_device_id) {
        remove_account_synthetic_device(&state, &user_id);
    }

    // 5. 尝试获取用户名
    let name = account_name
        .or(callback_info.user_name.clone())
        .or_else(|| {
            // 尝试调用 GetUserInfo
            get_user_info(&jwt)
                .map(|(uid, uname)| if uname.is_empty() { uid } else { uname })
                .ok()
        })
        .unwrap_or_else(|| {
            // 按字符截取（字节切片在多字节 UTF-8 边界处会 panic）
            let head: String = user_id.chars().take(8).collect();
            format!("账号_{head}")
        });

    // 6. 确定最终的 refresh_token（优先使用 ExchangeToken 返回的新 token）
    let final_refresh_token = new_refresh_token
        .unwrap_or_else(|| callback_info.refresh_token.clone());

    // 7. 检查账号是否已存在（同 uid 视为同一服务端账号，合并更新而非新增）
    let mut accounts = crate::vault::load_accounts(&state);
    let merged_existing_name = accounts
        .accounts
        .iter()
        .position(|a| a.user_id.as_deref() == Some(&user_id))
        .map(|idx| accounts.accounts[idx].name.clone());
    if let Some(existing_name) = merged_existing_name.clone() {
        // 已存在：更新 JWT 和 refresh_token
        let acct = accounts
            .accounts
            .iter_mut()
            .find(|a| a.user_id.as_deref() == Some(&user_id))
            .unwrap();
        acct.jwt = jwt.clone();
        acct.refresh_token = Some(final_refresh_token.clone());
        acct.updated_at = Some(fs_utils::now_iso());
        // 重新 OAuth 登录拿到新 token：生命周期计数清零、失效标记解除（F-78 批次 3）
        acct.refresh_token_fails = 0;
        acct.refresh_token_invalid = false;
        // 合并路径同属「OAuth 登录成功更新凭证」，auth_saved_at 与新账号/刷新成功路径对齐刷新
        acct.auth_saved_at = Some(fs_utils::now_iso());
        // P1（issue #201）：登录交换响应携带的 rt 过期时间落库（None 时保留旧值不覆盖）
        if refresh_token_expires_at.is_some() {
            acct.refresh_token_expires_at = refresh_token_expires_at;
        }
        crate::vault::save_accounts(&state, &mut accounts)?;

        // 日志必须指向实际被更新的账号名（existing_name），而非本次登录输入的新名，
        // 避免同名不同手机号场景下被误读为「覆盖/串号」（issue #80）
        if existing_name == name {
            fs_utils::app_log(
                &state.data_dir,
                &format!("OAuth 登录：更新已有账号 [{existing_name}] jwt + refresh_token"),
            );
        } else {
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "OAuth 登录：uid={user_id} 与已有账号 [{existing_name}] 相同，本次登录名 [{name}] 视为同一服务端账号，已更新 [{existing_name}] 的 jwt + refresh_token（未新增账号）"
                ),
            );
        }
    } else {
        // 新账号
        accounts.accounts.push(RawAccount {
            name: name.clone(),
            user_id: Some(user_id.clone()),
            jwt: jwt.clone(),
            refresh_token: Some(final_refresh_token.clone()),
            added_at: Some(fs_utils::now_iso()),
            updated_at: Some(fs_utils::now_iso()),
            dc_id: None,
            // P1（issue #201）：登录交换响应携带的 rt 过期时间随新账号一并落库
            refresh_token_expires_at,
            refresh_token_fails: 0,
            refresh_token_invalid: false,
            // 凭证最近落盘时间（F-78 批次 3 收尾，对齐 Buddy auth_saved_at）
            auth_saved_at: Some(fs_utils::now_iso()),
        });
        crate::vault::save_accounts(&state, &mut accounts)?;

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：新增账号 [{}] user_id={}", name, user_id),
        );
    }

    // 所选分组统一在账号落库后应用（新增/合并路径对齐）：用户在登录界面明确选择分组时
    // 同步写入 membership（合并路径此前静默丢弃所选分组）；未选分组时保持原分组不变。
    // 持 group_defs_lock：membership RMW 与并发分组增删/导入互斥防丢更新；
    // invalidate 在锁 drop 后执行
    if let Some(g) = group_id {
        let _defs = state.group_defs_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut groups: crate::models::GroupsFile =
            crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
        groups.membership.insert(user_id.clone(), g);
        crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
        drop(_defs);
        // 成员关系变更 → Key 组/标签展开索引失效（与其余全部写路径对齐，
        // 不依赖 TTL 5s 兜底）
        crate::api_server::org_index::invalidate(&state.data_dir);
    }

    // 重新登录拿到新凭证 → 运行中 API 池热重载（全量重建，覆盖单点回填管不到的
    // 陈旧快照：SessionDead 禁用 / 冷却 / 积分 / 新账号缺失；服务未运行时 no-op）
    crate::commands::api_server::reload_pools_if_running(&state, &runtime);

    // 登录闭环完成：清理本机登录会话（PKCE verifier / loginTraceID / 会话设备），
    // 防止会话状态跨登录残留被重放利用；失败路径不清理（用户可能重试粘贴回调）
    {
        let mut guard = LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    Ok(OAuthLoginResult {
        user_id: user_id.clone(),
        name,
        jwt,
        refresh_token: final_refresh_token,
        has_refresh_token: true,
        merged: merged_existing_name.is_some(),
        existing_name: merged_existing_name,
    })
}

#[cfg(test)]
mod oauth_tests {
    use super::*;

    /// 定级回归（2026-10-06 缺陷）：app.log 实测响应形态必须被识别为 code=20101，
    /// 且判为「服务端明确拒绝」——旧实现对同一响应恒判「无 Error 信封」，
    /// 失效凭证被记成「网络类不计失效」，永不置 refresh_token_invalid。
    #[test]
    fn volcano_error_reads_response_metadata_envelope() {
        let body = serde_json::json!({
            "ResponseMetadata": {
                "Action": "",
                "Error": {
                    "Code": "20101",
                    "Data": {"__Message.error": "refresh token is invalid"},
                    "Message": "Token 无效：refresh token is invalid.",
                    "StandardCode": "040012"
                },
                "RequestId": "",
                "TraceID": "00000000000000000000000000000000"
            }
        });
        let (code, msg, std_code) = volcano_error(&body).expect("信封必须被识别");
        assert_eq!(code, "20101");
        assert_eq!(std_code, "040012");
        assert!(refresh_token_rejected(&code, &msg));
    }

    /// 数字形态 code（部分端点返回 i64 而非字符串）同样识别；
    /// 且设备/协议级错误码不得判为凭证失效（凭证可能仍有效，应继续探测下一变体）
    #[test]
    fn volcano_error_accepts_numeric_code_without_credential_rejection() {
        let body = serde_json::json!({
            "ResponseMetadata": {"Error": {"Code": 20403, "Message": "Token device not match."}}
        });
        let (code, msg, _) = volcano_error(&body).expect("信封必须被识别");
        assert_eq!(code, "20403");
        assert!(!refresh_token_rejected(&code, &msg));
    }

    /// 顶层 Error / 信封键包裹的 Error 均可识别（抗包裹层变动）
    #[test]
    fn volcano_error_tolerates_wrapping() {
        let flat = serde_json::json!({"Error": {"Code": "20101", "Message": "refresh token is invalid"}});
        assert_eq!(volcano_error(&flat).map(|(c, _, _)| c), Some("20101".to_string()));
        let wrapped = serde_json::json!({"data": {"Error": {"Code": "20101"}}});
        assert_eq!(volcano_error(&wrapped).map(|(c, _, _)| c), Some("20101".to_string()));
    }

    /// 成功信封不得误判为失败；code=0 由调用方按「非 0 才算错误」过滤
    #[test]
    fn volcano_error_ignores_success_envelope() {
        let ok = serde_json::json!({
            "ResponseMetadata": {"Action": ""},
            "Result": {"Token": "x", "RefreshToken": "y"}
        });
        assert!(volcano_error(&ok).is_none());
        let zero = serde_json::json!({"ResponseMetadata": {"Error": {"Code": "0"}}});
        assert_eq!(volcano_error(&zero).map(|(c, _, _)| c), Some("0".to_string()));
    }

    /// Message 兜底匹配（上游改码不改语义）：大小写不敏感命中「refresh token is invalid」
    #[test]
    fn refresh_token_rejected_matches_message_case_insensitively() {
        assert!(refresh_token_rejected("99999", "Token Invalid: Refresh Token Is Invalid."));
        assert!(!refresh_token_rejected("20405", "Device proof required."));
        assert!(!refresh_token_rejected("0", ""));
    }

    // ── issue #76：凭证轮转选择 ──────────────────────────────────────────────

    fn mk_cred(device_id: &str) -> crate::icube_auth::DeviceCredential {
        crate::icube_auth::DeviceCredential {
            device_id: device_id.to_string(),
            private_key_pem: String::new(),
            machine_id: format!("machine-{device_id}"),
            app_version: "0.0.0".into(),
            source_app: "test".into(),
        }
    }

    fn mk_entry(device_id: &str) -> DeviceEntry {
        DeviceEntry {
            device_id: device_id.to_string(),
            market_user_id: None,
            session_id: None,
        }
    }

    /// 绑定计数口径：凭证命中计该凭证；伪设备条目与无条目账号计入首个凭证
    ///（历史 OAuth 登录/刷新恒用首个凭证，服务端绑定在它）
    #[test]
    fn credential_binding_counts_pseudo_and_missing_fall_back_to_first() {
        let creds = vec![mk_cred("111"), mk_cred("222")];
        let map: DeviceMap = DeviceMap::from([
            ("u1".to_string(), mk_entry("111")),
            ("u2".to_string(), mk_entry("222")),
            ("u3".to_string(), mk_entry("999888888888888")), // 伪设备（derive/代理捕获）
        ]);
        // u4 无条目
        let uids = vec![
            Some("u1".into()),
            Some("u2".into()),
            Some("u3".into()),
            Some("u4".into()),
        ];
        let counts = credential_binding_counts(&creds, &map, &uids);
        assert_eq!(counts.get("111"), Some(&3)); // u1 + u3（伪设备）+ u4（无条目）归首凭证
        assert_eq!(counts.get("222"), Some(&1));
    }

    /// 轮转目标：绑定数最少的凭证；并列取表序靠前者（同状态选择稳定）
    #[test]
    fn select_credential_prefers_least_bound_and_stable_on_tie() {
        let creds = vec![mk_cred("111"), mk_cred("222"), mk_cred("333")];
        let map: DeviceMap = DeviceMap::from([
            ("u1".to_string(), mk_entry("111")),
            ("u2".to_string(), mk_entry("111")),
            ("u3".to_string(), mk_entry("222")),
        ]);
        let uids = vec![Some("u1".into()), Some("u2".into()), Some("u3".into())];
        let counts = credential_binding_counts(&creds, &map, &uids);
        assert_eq!(select_credential_index(&creds, &counts), Some(2)); // 333 零绑定

        // 全零计数（新装环境）：取首个，不漂移
        let empty = std::collections::HashMap::new();
        assert_eq!(select_credential_index(&creds, &empty), Some(0));
    }

    /// 无凭证时选择返回 None（调用方回落 kv 兜底设备）
    #[test]
    fn select_credential_without_creds_is_none() {
        let creds: Vec<crate::icube_auth::DeviceCredential> = Vec::new();
        let counts = std::collections::HashMap::new();
        assert_eq!(select_credential_index(&creds, &counts), None);
        let counts2 = credential_binding_counts(&creds, &DeviceMap::new(), &[]);
        assert!(counts2.is_empty());
    }

    // ── issue #110：混合轮转决策（未绑定真实凭证优先，全绑定走 synthetic）────

    /// 有绑定数 0 的真实凭证 → 返回该凭证下标（复用真实凭证，不生成 synthetic）
    #[test]
    fn unbound_credential_prefers_zero_binding_credential() {
        let creds = vec![mk_cred("111"), mk_cred("222")];
        let map: DeviceMap = DeviceMap::from([
            ("u1".to_string(), mk_entry("111")),
            ("u2".to_string(), mk_entry("111")),
        ]);
        let uids = vec![Some("u1".into()), Some("u2".into())];
        // 222 零绑定 → 直接用 222
        assert_eq!(select_unbound_credential(&creds, &map, &uids), Some(1));
    }

    /// 全部真实凭证已绑定 → None（调用方走 synthetic 生成路径）：
    /// 多账号全绑同一台设备时服务端签到按 x-device-id 每日去重，issue #110 根因
    #[test]
    fn unbound_credential_none_when_all_bound() {
        let creds = vec![mk_cred("111")];
        let map: DeviceMap = DeviceMap::from([
            ("u1".to_string(), mk_entry("111")),
            ("u2".to_string(), mk_entry("111")),
        ]);
        let uids = vec![Some("u1".into()), Some("u2".into())];
        assert_eq!(select_unbound_credential(&creds, &map, &uids), None);
        // 无条目账号计入首凭证（历史行为），同样构成「已绑定」
        let map2: DeviceMap = DeviceMap::from([("u1".to_string(), mk_entry("111"))]);
        assert_eq!(
            select_unbound_credential(&creds, &map2, &[Some("u1".into()), Some("u2".into())]),
            None
        );
    }

    /// synthetic 绑定的账号（device_map 条目未命中任何真实凭证）计入首凭证，
    /// 不会让真实凭证被误判为未绑定
    #[test]
    fn unbound_credential_counts_synthetic_bindings_toward_first() {
        let creds = vec![mk_cred("111")];
        // u1 绑定 synthetic（设备 999），不在真实凭证表 → 归首凭证
        let map: DeviceMap = DeviceMap::from([("u1".to_string(), mk_entry("999888888888888"))]);
        assert_eq!(
            select_unbound_credential(&creds, &map, &[Some("u1".into())]),
            None
        );
    }

    /// 无凭证 → None（Web-only 生成 synthetic 的判定入口）
    #[test]
    fn unbound_credential_none_without_creds() {
        let creds: Vec<crate::icube_auth::DeviceCredential> = Vec::new();
        assert_eq!(
            select_unbound_credential(&creds, &DeviceMap::new(), &[]),
            None
        );
    }
}
