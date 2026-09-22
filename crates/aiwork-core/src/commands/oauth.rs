use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use crate::fs_utils;
use crate::jwt;
use crate::models::{DeviceMap, RawAccount};
use crate::state::AppState;

/// 最近签发的 OAuth 登录会话（CSRF 防护 + PKCE）：oauth_get_login_url 签发时记录，
/// oauth_parse_callback 用 loginTraceID 双向绑定校验（抓包实证：login_trace_id 参数
/// 会被授权页原样回传为回调的 loginTraceID），PKCE verifier 供 AuthCode 交换
struct PendingLogin {
    state: String,
    pkce_verifier: String,
}
static LAST_OAUTH_STATE: Mutex<Option<PendingLogin>> = Mutex::new(None);

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
/// 授权完成后的回跳地址：粘贴回调模式下浏览器跳到此本机地址「无法连接」页，
/// 用户复制地址栏 URL 回 Web UI 提交完成登录（端口 17388 与桌面版一致）
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

/// 读取外置 OAuth 客户端配置（全局一次；缺失/损坏回退默认值）
pub fn oauth_client() -> &'static OAuthClientConfig {
    static CFG: std::sync::OnceLock<OAuthClientConfig> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        std::env::var("APPDATA")
            .ok()
            .map(|d| {
                std::path::PathBuf::from(d)
                    .join(crate::state::DATA_DIR_NAME)
                    .join("conf")
                    .join("oauth_client.json")
            })
            .map(|p| fs_utils::read_json::<OAuthClientConfig>(&p))
            .unwrap_or_default()
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
/// （%APPDATA%/AIWorkAssistant/certs/ca.crt，目录解析与 state.rs::new 同源），
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
    let dir = std::env::var("APPDATA")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join(crate::state::DATA_DIR_NAME));
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

/// 生成随机 hex 字符串。
/// 熵源：OS CSPRNG（Web 化改造后统一走 rand::rngs::OsRng，跨平台安全随机；桌面版
/// 为 Windows BCryptGenRandom 系统首选 RNG）。旧 LCG 以时间戳作种子，输出可预测，
/// 不适合 OAuth state / machine_id 等安全场景（审查 P2）；OS RNG 失败时
/// 保留 LCG 兜底（仅影响随机性，不中断流程）。
pub fn random_hex(len: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; len.div_ceil(2)];
    if rand::rngs::OsRng.try_fill_bytes(&mut bytes).is_ok() {
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
        return out;
    }
    // 兜底：旧 LCG（仅 OS CSPRNG 不可用时）
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        // 简单 LCG
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let nibble = ((seed >> 32) & 0xF) as u8;
        out.push(if nibble < 10 {
            (b'0' + nibble) as char
        } else {
            (b'a' + nibble - 10) as char
        });
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
    /// 上游归一化设备绑定 ID（交换成功后响应 Result.BoundDeviceID）。
    /// 刷新时必须用此值做请求 DeviceID，否则新端点 20403 Token device not match。
    #[serde(default)]
    pub bound_device_id: Option<String>,
}

/// 读取（缺失则生成并写回）本机稳定的 OAuth 设备标识。
/// device_id 优先对齐 device_map.json 已有条目（登录前无法预知账号，取 user_id 字典序
/// 最小的条目作为本机基准，保证不再每次随机）；machine_id 无对应字段，首次随机后固定。
/// 注意：不回写 device_map.json——DeviceEntry 三元组（device_id/market_user_id/session_id）
/// 与 OAuth 二元组字段语义不同，部分写入会破坏签到脚本的完整三元组假设。
fn load_or_create_oauth_device(state: &AppState) -> OAuthDevice {
    // SQLite 化（P2）：oauth_device.json → kv `oauth_device`
    let store = crate::store::db(&state.data_dir);
    let mut dev: OAuthDevice = store.kv_get("oauth_device");
    // device_id 首选 icube 设备凭证（F-78 DeviceProof）：签名私钥与 icube-dc
    // deviceId 绑定，登录 URL 的 device_id 必须与之同源，否则服务端 20403/20405；
    // 恒覆盖旧值（旧值是随机/device_map 对齐的，与私钥不匹配）
    if let Some(cred) = icube_device_creds(&state.data_dir).first() {
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

/// 生成 PKCE code_verifier（RFC 7636：43-128 字符非 reserved 字符；hex 64字符合规）
/// 与 S256 code_challenge（BASE64URL-NOPAD(SHA256(verifier))）
fn pkce_pair() -> (String, String) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let verifier = random_hex(64);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// 生成 OAuth 登录 URL（2026-09-16 抓包固化：对齐真实 Trae IDE 登录页参数形态）。
/// 旧形态（client_secret/app_id/response_type=code）会让授权页停在
/// cn_credits_billing_status 后无后续、不回跳；真实流程：
/// login_channel=native_ide → 页面前端调 GetPCAuthCode（绑定 PKCE challenge）→
/// 302 回 auth_callback_url，回调参数为 authCodeInfo（JSON）而非 refreshToken/code。
/// Web 粘贴回调模式（ADR-3）：服务端只构造并返回登录 URL，由用户在浏览器完成授权后
/// 粘贴回调 URL（不再打开浏览器/启动回环监听）。
pub fn oauth_get_login_url(state: &AppState) -> OAuthLoginUrl {
    let dev = load_or_create_oauth_device(state);
    let machine_id = dev.machine_id;
    let device_id = dev.device_id;
    // login_trace_id 兼作 CSRF 绑定值（抓包实证：授权页原样回传为回调 loginTraceID）
    let trace_id = random_hex(32);
    let (pkce_verifier, code_challenge) = pkce_pair();

    let hostname = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows-PC".into());
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
        &x_device_type=windows\
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
        os_version = urlencoding::encode("Windows"),
        app_version = OAUTH_PAGE_APP_VERSION,
        code_challenge = code_challenge,
    );

    // 记录本机登录会话（CSRF + PKCE）供回调校验/交换使用
    {
        let mut guard = LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(PendingLogin { state: trace_id.clone(), pkce_verifier });
    }

    OAuthLoginUrl {
        url,
        state: trace_id,
        redirect_uri: OAUTH_REDIRECT_URI.to_string(),
    }
}

/// 解析 OAuth 回调 URL
pub fn oauth_parse_callback(
    state: &AppState,
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

    // CSRF 校验（抓包固化：state 已不适用，native_ide 流程回调不回传 state，改用
    // loginTraceID 双向绑定——授权页把 login_trace_id 原样回传为 loginTraceID）。
    // 本进程签发过登录会话且回调携带 loginTraceID 时两者必须一致；回调不带
    // loginTraceID 或本进程未签发过（重启后粘贴回调）时保持宽容，不阻断正常登录。
    if let Some(cb_trace) = params.get("loginTraceID").or_else(|| params.get("login_trace_id")) {
        let issued = LAST_OAUTH_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.state.clone());
        if let Some(expected) = issued {
            if !expected.is_empty() && cb_trace != &expected {
                return Err("OAuth loginTraceID 校验失败：回调 URL 与本机发起的登录请求不匹配（可能为伪造或重放），已拒绝".into());
            }
        }
    } else if auth_code.is_some() {
        // 新流程回调必带 loginTraceID：缺失且本机有在途会话时视为不匹配
        let issued = LAST_OAUTH_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.state.clone());
        if issued.is_some() {
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
            let verifier = LAST_OAUTH_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|p| p.pkce_verifier.clone());
            let device_id = load_or_create_oauth_device(state).device_id;
            // host：授权页回传的 API 域（main.js 逆向：交换 URL = ${host}/trae/api/v3/oauth/ExchangeToken）
            let host = params
                .get("host")
                .cloned()
                .unwrap_or_else(|| "https://api.trae.com.cn".into());
            match exchange_code(&code, verifier.as_deref(), &device_id, &host, &state.data_dir) {
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
fn exchange_code(code: &str, verifier: Option<&str>, device_id: &str, host: &str, data_dir: &std::path::Path) -> Result<(String, String), String> {
    let client_id = oauth_client().client_id.clone();
    let code_v = code.to_string();
    let verifier_v = verifier.unwrap_or("").to_string();
    let device_v = device_id.to_string();

    let mut variants: Vec<(String, String, serde_json::Value, bool)> = Vec::new();

    // 主变体（真实客户端形态）：DeviceInfo 对象 + IDEVersion，无 DeviceProof
    if let Some(cred) = icube_device_creds(data_dir).first() {
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
    if let Some(cred) = icube_device_creds(data_dir).first() {
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
        match try_exchange_variant(tag, url, payload.clone(), device_id, *with_proof_header, data_dir) {
            Ok(t) => {
                fs_utils::app_log(data_dir, &format!("[OAuth交换] 变体 {tag} 成功（协议固化前保留探测链）"));
                return Ok(t);
            }
            Err(e) => errs.push(format!("{tag}: {e}")),
        }
    }
    Err(format!("全部交换变体失败 → {}", errs.join(" | ")))
}

/// 本机 Trae 客户端 icube 设备凭证（进程内缓存；首次调用解析）。
/// 解析链：扫描本机 storage.json（桌面机真实凭证）→ 空则加载/生成合成设备
///（kv `oauth_synthetic_device` 持久化，跨重启稳定）——Web-only 容器部署无 Trae
/// 客户端，此前仅剩裸 DeviceID 兜底变体，上游 20405 Device proof required 直接拒绝。
/// data_dir 仅首次调用生效（OnceLock 进程级缓存；进程内 data_dir 恒定）。
fn icube_device_creds(data_dir: &std::path::Path) -> &'static [crate::icube_auth::DeviceCredential] {
    static CREDS: std::sync::OnceLock<Vec<crate::icube_auth::DeviceCredential>> = std::sync::OnceLock::new();
    CREDS.get_or_init(|| {
        let extracted = crate::icube_auth::extract_device_credentials();
        if !extracted.is_empty() {
            return extracted;
        }
        // 合成设备兜底（等同全新安装 IDE 的自生成密钥对 + DeviceInfo 自声明）：
        // 服务端自有身份落 store（与 token 同信任域），私钥不进日志
        let store = crate::store::db(data_dir);
        let saved: serde_json::Value = store.kv_get("oauth_synthetic_device");
        if let Some(cred) = crate::icube_auth::device_credential_from_json(&saved) {
            return vec![cred];
        }
        match crate::icube_auth::generate_synthetic_device_credential(OAUTH_PAGE_APP_VERSION) {
            Ok(cred) => {
                let json = serde_json::to_value(&cred).unwrap_or_default();
                let _ = store.kv_set("oauth_synthetic_device", &json);
                fs_utils::app_log(
                    data_dir,
                    &format!(
                        "OAuth: 本机无 Trae 客户端凭证，已生成合成设备身份并持久化（device_id={}，来源 synthetic）",
                        cred.device_id
                    ),
                );
                vec![cred]
            }
            Err(e) => {
                fs_utils::app_log(data_dir, &format!("OAuth: 合成设备凭证生成失败: {e}"));
                Vec::new()
            }
        }
    })
}

/// 交换/刷新成功后回写上游归一化 BoundDeviceID（kv `oauth_device.bound_device_id`）：
/// 上游把本地声明 device_id 归一化为独立 BoundDeviceID 并与 Token 绑定，后续新端点
/// 刷新必须用它作 DeviceID（否则 20403 Token device not match）；值不变时跳过写盘。
fn persist_bound_device_id(data_dir: &std::path::Path, body: &serde_json::Value) {
    let Some(bound) = crate::fs_utils::dig(body, &["Result", "BoundDeviceID"])
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
    else {
        return;
    };
    let store = crate::store::db(data_dir);
    let mut dev: OAuthDevice = store.kv_get("oauth_device");
    if dev.bound_device_id.as_deref() == Some(bound.as_str()) {
        return;
    }
    dev.bound_device_id = Some(bound.clone());
    let _ = store.kv_set("oauth_device", &dev);
    fs_utils::app_log(
        data_dir,
        &format!("[OAuth] 已持久化上游 BoundDeviceID={bound}（后续刷新优先用作 DeviceID）"),
    );
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
    let err_code = crate::fs_utils::dig(&body, &["ResponseMetadata", "Error", "Code"])
        .map(|v| {
            v.as_i64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if !err_code.is_empty() && err_code != "0" {
        let err_msg = crate::fs_utils::dig(&body, &["ResponseMetadata", "Error", "Message"])
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let std_code = crate::fs_utils::dig(&body, &["ResponseMetadata", "Error", "StandardCode"])
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Err(format!("code={err_code}/{}: {}", std_code, if err_msg.is_empty() { "未知错误" } else { &err_msg }));
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
                persist_bound_device_id(data_dir, &body);
                return Ok((a, r));
            }
        }
    }
    match (
        find_token_value(&body, ACCESS_KEYS),
        find_token_value(&body, REFRESH_KEYS),
    ) {
        (Some(a), Some(r)) if !a.is_empty() && !r.is_empty() => {
            persist_bound_device_id(data_dir, &body);
            Ok((a, r))
        }
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
        let head: String = s.chars().take(6).collect();
        format!("{head}…(len={})", s.chars().count())
    }
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if is_sensitive_key(k) {
                    if let Some(s) = val.as_str() {
                        if !s.is_empty() {
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
pub(crate) fn exchange_token_refresh(
    state: &AppState,
    refresh_token: &str,
) -> Result<(String, Option<String>, serde_json::Value), RefreshExchangeError> {
    let client_id = oauth_client().client_id.clone();
    let dev = load_or_create_oauth_device(state);
    let local_device_id = dev.device_id;
    // 刷新 DeviceID 选择：新端点校验 Token 绑定设备（20403 Token device not match），
    // 须用交换成功后持久化的上游归一化 BoundDeviceID；旧端点不校验，保持本地声明 id
    let bound_device_id = dev
        .bound_device_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| local_device_id.clone());

    // 变体链（主→兜底）：固化协议 P1363/DER → 旧端点 P1363 → 旧协议（无凭证时唯一路径）
    let new_url = "https://api.trae.com.cn/trae/api/v3/oauth/ExchangeToken";
    let mut variants: Vec<(String, String, serde_json::Value, String, bool)> = Vec::new();
    if let Some(cred) = icube_device_creds(&state.data_dir).first() {
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
                crate::icube_auth::device_proof(cred, sign_path, &client_id, refresh_token, fmt)
            else {
                continue;
            };
            // proof 签名原文不含 device_id，请求 DeviceID 按端点选择不受签名影响
            let req_device_id = if url == new_url { &bound_device_id } else { &local_device_id };
            variants.push((
                format!("Refresh/Proof{}", fmt.suffix()),
                url.to_string(),
                ureq::json!({
                    "ClientID": client_id,
                    "RefreshToken": refresh_token,
                    "DeviceID": req_device_id,
                    "PlatformCode": OAUTH_PAGE_PLATFORM_CODE,
                    "DeviceProof": proof,
                }),
                req_device_id.clone(),
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
        local_device_id.clone(),
        false,
    ));

    let mut last_err: Option<RefreshExchangeError> = None;
    for (tag, url, payload, device_id, with_proof_header) in &variants {
        match try_refresh_variant(tag, url, payload.clone(), device_id, *with_proof_header, &state.data_dir) {
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

    // 火山信封错误：协议级失败（可能是 DeviceProof/设备问题），继续探测下一变体
    let err_code = crate::fs_utils::dig(&body, &["ResponseMetadata", "Error", "Code"])
        .map(|v| {
            v.as_i64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if !err_code.is_empty() && err_code != "0" {
        let err_msg = crate::fs_utils::dig(&body, &["ResponseMetadata", "Error", "Message"])
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        return Err(RefreshExchangeError {
            msg: format!("code={err_code}: {err_msg}"),
            server_rejected: false,
        });
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
        Some(a) => {
            persist_bound_device_id(data_dir, &body);
            Ok((a, refresh, body))
        }
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

/// OAuth 登录闭环：解析回调 → 换取 accessToken → 获取用户信息 → 保存账号
// async：内含最多两次 120s 超时的串行网络请求（exchange_token/get_user_info），同步命令会冻结 UI（审查修复）
// Web 化（ADR-3）：runtime 参数已删除，池热重载改走 crate::api_server::runtime::reload_pools_after_change
pub fn oauth_login(
    state: &AppState,
    callback_url: String,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<OAuthLoginResult, String> {
    // 1. 解析回调 URL
    let callback_info = oauth_parse_callback(state, callback_url)?;

    // 2. 如果回调中没有 accessToken，则用 refresh_token 换取
    let new_pair = if let Some(ref at) = callback_info.access_token {
        Some((at.clone(), None))
    } else {
        // 此处 refresh_token 刚从 OAuth 回调取得（非存量失效凭证），
        // server_rejected 分支仅转为错误信息，不涉及生命周期标记
        exchange_token_refresh(state, &callback_info.refresh_token)
            .map_err(|e| e.msg)
            .ok()
            .map(|(a, r, _)| (a, r))
    };
    let (access_token, new_refresh_token) = new_pair
        .ok_or_else(|| "refresh_token 交换失败".to_string())?;

    // 3. 规范化 JWT 格式
    let jwt = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.clone()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    // 4. 解析 JWT 获取 user_id
    let jwt_info = jwt::parse(&jwt);
    let user_id = callback_info
        .user_id
        .clone()
        .or_else(|| jwt_info.user_id.clone())
        .ok_or_else(|| "无法从回调或 JWT 中获取 user_id".to_string())?;

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

    // 7. 检查账号是否已存在
    let mut accounts = crate::vault::load_accounts(state);
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(&user_id))
    {
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
        crate::vault::save_accounts(state, &mut accounts)?;

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：更新已有账号 [{}] jwt + refresh_token", name),
        );
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
            refresh_token_expires_at: None,
            refresh_token_fails: 0,
            refresh_token_invalid: false,
            // 凭证最近落盘时间（F-78 批次 3 收尾，对齐 Buddy auth_saved_at）
            auth_saved_at: Some(fs_utils::now_iso()),
        });
        crate::vault::save_accounts(state, &mut accounts)?;

        // 设置分组
        if let Some(g) = group_id {
            let mut groups: crate::models::GroupsFile =
                crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
            groups.membership.insert(user_id.clone(), g);
            crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
        }

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：新增账号 [{}] user_id={}", name, user_id),
        );
    }

    // 重新登录拿到新凭证 → 运行中 API 池热重载（全量重建，覆盖单点回填管不到的
    // 陈旧快照：SessionDead 禁用 / 冷却 / 积分 / 新账号缺失；服务未运行时 no-op）
    crate::api_server::runtime::reload_pools_after_change(state);

    Ok(OAuthLoginResult {
        user_id: user_id.clone(),
        name,
        jwt,
        refresh_token: final_refresh_token,
        has_refresh_token: true,
    })
}
