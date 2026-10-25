//! Qoder OAuth 设备流（F-80 M1，R-10 抓包固化 2026-09-27；issue #140 修复 2026-10-24）。
//!
//! 客户端真实链路（PKCE device flow）：
//! 1. 生成 `nonce`（uuid v4）、`verifier`（64 字符）、`machine_id`（uuid v4）；`client_id` 用官方固定常量
//! 2. 浏览器打开 SSO 登录页并自构 oauth_callback（issue #140 修复）：
//!    `https://qoder.cn/sso/login/aliyun?oauth_callback=<百分号编码的设备授权页 URL>`
//!    回调（设备授权页）形如 `https://qoder.cn/device/selectAccounts?challenge=..
//!    &challenge_method=S256&nonce=..&machine_id=..&directLogin=true&client_id=732aef47-..`
//!    —— **client_id 必须用官方注册的固定常量**（2026-10-02 实测：随机生成 client_id
//!    会被判「参数无效」；值取自新版 Qoder CN 客户端 `authClientIds.prod`，与真实
//!    客户端跳转样例逐字一致；社区实现（10router/cockpit-tools）则完全不带
//!    client_id，同样可用）。directLogin=true 仅出现一次（模拟授权页自身生成的
//!    回调形态；2026-10-02 的「参数无效」系 directLogin 重复所致，单个安全）。
//!    **不再直接打开 /device/selectAccounts**：2026-10-24 实测（issue #140）该页
//!    在前端自构 SSO 跳转时会向 oauth_callback 写入显式 `:443` 端口
//!    （`https://qoder.cn:443/device/...`），与其 SSO 端点的回调白名单（仅
//!    `https://qoder.cn`）不匹配，未登录用户经授权页登录必现 400 Bad Request。
//!    客户端直连 SSO 登录页绕过该损坏的重定向（实测 200 正常登录表单），用户
//!    完成阿里云登录后回跳设备授权页继续 PKCE 流程
//! 3. 轮询 `GET {open_api}/api/v1/deviceToken/poll?nonce=..&verifier=..&challenge_method=S256`
//!    - **pending = HTTP 404** `{"errorCode":"NotFound",...}`（实测）
//!    - **成功 = HTTP 200**：`{id, token(dt-), user_id, expires_in:2591999999(≈30d ms),
//!      refresh_token, refresh_token_expires_in:31103999999(≈360d ms), ...}`
//!
//! 红线：verifier/token 全程不入日志/事件（脱敏红线）。

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::qoder_common::{self, QoderCreds};
use crate::fs_utils;

/// 授权页基址（需求方实测样例：qoder.cn/device/selectAccounts）
pub const DEVICE_AUTH_BASE: &str = "https://qoder.cn/device/selectAccounts";

/// SSO 登录页基址（issue #140 修复 2026-10-24）：设备授权页自构 SSO 跳转时向
/// oauth_callback 写入显式 `:443` 端口，被 SSO 端点回调白名单（仅 `https://qoder.cn`）
/// 拒绝 → 400 Bad Request。客户端改为直连本页并自构正确回调（实测 200）。
/// 可用环境变量 `AIWORK_QODER_SSO_BASE` 覆盖（审查修复 低-4：官方端点轮换/
/// 自托管代理场景免改代码；仅接受 http(s) URL，非法值一律回退本常量）。
pub const SSO_LOGIN_BASE: &str = "https://qoder.cn/sso/login/aliyun";

/// SSO 基址解析核心（参数注入版，审查修复 低-4 + 中-2 同一原则：单测不触碰
/// 进程级全局环境变量）。覆盖值要求：去除首尾空白与尾部 `/` 后必须以
/// `https://` 或 `http://` 开头，否则回退默认常量。
/// 返回 (生效基址, 诊断日志)（审查修复 中-1：覆盖生效/非法回退必须留痕——
/// 官方端点轮换排障与 env 被注入的告警均依赖此线索；缺失/空白视为未设置不落日志）。
fn sso_login_base_from(env: Option<&str>) -> (String, Option<String>) {
    let candidate = env
        .map(str::trim)
        .map(|v| v.trim_end_matches('/'))
        .filter(|v| v.starts_with("https://") || v.starts_with("http://"));
    match candidate {
        Some(v) => {
            // http 明文基址：回调 URL（含 challenge/nonce）将发往非加密信道且
            // 页面可被仿冒——不构成 token 泄露（verifier 仅走 poll 端点），但属
            // 真实钓鱼面，落日志告警
            let warn = if v.starts_with("http://") {
                "（警告：非 https 明文基址，存在仿冒/劫持面，仅限本机调试）"
            } else {
                ""
            };
            (v.to_string(), Some(format!("AIWORK_QODER_SSO_BASE 覆盖生效{warn}: {v}")))
        }
        None => match env.map(str::trim).filter(|v| !v.is_empty()) {
            Some(bad) => (
                SSO_LOGIN_BASE.to_string(),
                Some(format!("AIWORK_QODER_SSO_BASE 值非法（{bad}），已回退默认基址")),
            ),
            None => (SSO_LOGIN_BASE.to_string(), None),
        },
    }
}

/// 授权页 client_id（官方注册常量，新版 Qoder CN 客户端 `authClientIds.prod`）。
/// 实测 2026-10-02：随机生成 client_id 会被授权页判「参数无效」；社区实现
/// （10router/cockpit-tools）则完全省略该参数，两者均为可行方案。
pub const DEVICE_AUTH_CLIENT_ID: &str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";

/// 轮询间隔（抓包实测约 1s）
pub const POLL_INTERVAL_MS: u64 = 1000;
/// 轮询超时（180s，覆盖人工在浏览器完成登录的时延）
pub const POLL_TIMEOUT_MS: u64 = 180_000;

/// 一次设备流会话的入参（nonce/verifier/machine_id/client_id 全程自持）
pub struct DeviceFlow {
    pub nonce: String,
    pub verifier: String,
    /// 会话标识（仅入授权页 URL 与测试断言；M2 MITM 透传时复用）
    #[allow(dead_code)]
    pub machine_id: String,
    /// 兼容模式（None）时不携带：社区实现（10router/cockpit-tools）验证可正常授权
    #[allow(dead_code)]
    pub client_id: Option<String>,
    /// SSO 登录页完整 URL（oauth_callback 指向设备授权页，challenge = BASE64URL(SHA256(verifier))，S256）
    pub auth_url: String,
    /// SSO 基址解析诊断（审查修复 中-1）：env 覆盖生效/非法回退的日志内容，
    /// 由登录入口落 app_log；默认路径为 None
    pub(crate) sso_base_note: Option<String>,
}

impl DeviceFlow {
    /// 常规模式：携带官方注册固定 client_id（与真实客户端跳转逐字一致）
    pub fn new() -> Self {
        Self::build(true, None)
    }

    /// 兼容模式：不带 client_id（2026-10-02 审查预案落地）——官方常量一旦被
    /// Qoder 轮换导致授权页「参数无效」，改用本模式即可恢复（前端在授权超时后
    /// 自动切换到本模式重试）。
    pub fn new_compat() -> Self {
        Self::build(false, None)
    }

    fn build(with_client_id: bool, injected_sso_base: Option<String>) -> Self {
        // verifier：双 uuid v4 拼接 = 64 hex 字符（抓包样本同为 64 字符长度量级）
        let verifier = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let nonce = uuid::Uuid::new_v4().to_string();
        let machine_id = uuid::Uuid::new_v4().to_string();
        // client_id：官方注册固定常量（随机生成会被授权页判「参数无效」）
        let client_id = with_client_id.then(|| DEVICE_AUTH_CLIENT_ID.to_string());
        // challenge：BASE64URL_NO_PAD(SHA256(verifier))（PKCE S256）
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        // 设备授权页完整 URL（不再直接打开——issue #140：该页自构 SSO 跳转时向
        // oauth_callback 写入显式 :443 端口，被 SSO 回调白名单拒绝 → 400）。
        // directLogin=true 单个出现，模拟授权页自身生成的回调形态（去 :443 后
        // 实测 200；2026-10-02「参数无效」系重复参数所致，单个安全）。
        let mut device_url = format!(
            "{DEVICE_AUTH_BASE}?challenge={challenge}&challenge_method=S256&nonce={nonce}&machine_id={machine_id}&directLogin=true"
        );
        if let Some(cid) = &client_id {
            device_url.push_str(&format!("&client_id={cid}"));
        }
        // 直连 SSO 登录页，oauth_callback 全量百分号编码（跳过授权页损坏的重定向）；
        // 基址可用 AIWORK_QODER_SSO_BASE 覆盖（审查修复 低-4）；覆盖/回退诊断随
        // 会话返回由登录入口落日志（审查修复 中-1）
        let (sso_base, sso_base_note) = match injected_sso_base {
            Some(b) => (b, None),
            None => sso_login_base_from(std::env::var("AIWORK_QODER_SSO_BASE").ok().as_deref()),
        };
        let auth_url = format!("{sso_base}?oauth_callback={}", urlencoding::encode(&device_url));
        Self { nonce, verifier, machine_id, client_id, auth_url, sso_base_note }
    }
}

impl Default for DeviceFlow {
    fn default() -> Self {
        Self::new()
    }
}

/// 单次轮询：GET /api/v1/deviceToken/poll（无 Authorization 头，与客户端一致）。
/// 返回 (http_status, parsed)；status=0 网络不可达。
pub fn poll_once(agent: &ureq::Agent, flow: &DeviceFlow) -> (u16, Option<Value>) {
    let url = format!(
        "{}/api/v1/deviceToken/poll?nonce={}&verifier={}&challenge_method=S256",
        qoder_common::OPEN_API_BASE,
        flow.nonce,
        urlencoding::encode(&flow.verifier)
    );
    let headers = vec![
        ("User-Agent".to_string(), qoder_common::CLIENT_USER_AGENT.to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let (status, parsed, _raw) = qoder_common::get_json(agent, &url, &headers);
    (status, parsed)
}

/// 轮询成功响应 → (凭证, uid)。宽容解析：token/accessToken 候选，
/// expires_in 毫秒级归一（2591999999 ≈ 30d）。
/// nonce 回验（审查 L-防会话混淆）：响应 nonce 与本会话 flow.nonce 必须一致——
/// poll 端点按 nonce 定位授权会话，响应携带其他会话的 token 即为异常（结构变更/
/// 中间人替换），一律拒绝。同时解析 refresh_token_expires_in 落库（≈360d，审查 L-RT）。
pub fn parse_poll_success(body: &Value, expected_nonce: &str) -> Option<(QoderCreds, String)> {
    let resp_nonce = fs_utils::dig(body, &["nonce"])
        .and_then(Value::as_str)
        .unwrap_or("");
    if resp_nonce != expected_nonce {
        return None;
    }
    let token = fs_utils::dig(body, &["token", "accessToken", "access_token"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return None;
    }
    let uid = fs_utils::dig(body, &["user_id", "userId", "uid"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    // expires_in 毫秒/秒归一统一走 qoder_common::normalize_expires_in
    //（原内联阈值 1e7 与 common 的 2_592_000 两套口径，中间值语义有歧义）
    let expires_at_ms = fs_utils::dig(body, &["expires_in", "expiresIn"])
        .and_then(|v| {
            v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
        })
        .map(|e| now_ms.saturating_add(qoder_common::normalize_expires_in(e)));
    let refresh_expires_at_ms = fs_utils::dig(
        body,
        &["refresh_token_expires_in", "refreshTokenExpiresIn"],
    )
    .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok()))
    .map(|e| now_ms.saturating_add(qoder_common::normalize_expires_in(e)));
    let refresh_token = fs_utils::dig(body, &["refresh_token", "refreshToken"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((
        QoderCreds {
            access_token: token,
            refresh_token,
            expires_at_ms,
            refresh_expires_at_ms,
            kind: "client".into(),
            uid: uid.clone(),
            ..Default::default()
        },
        uid,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_generates_distinct_sessions_and_pkce_url() {
        let f1 = DeviceFlow::new();
        let f2 = DeviceFlow::new();
        assert_ne!(f1.nonce, f2.nonce);
        assert_ne!(f1.verifier, f2.verifier);
        assert_eq!(f1.verifier.len(), 64);
        // auth_url 直连 SSO 登录页（issue #140），oauth_callback 全量编码且无顶层附加参数
        assert!(f1.auth_url.starts_with(SSO_LOGIN_BASE));
        let (_, cb) = f1.auth_url.split_once("?oauth_callback=").unwrap();
        assert!(!cb.contains('&'), "oauth_callback 必须全量编码，SSO URL 不得有其他顶层参数");
        let device_url = urlencoding::decode(cb).unwrap().into_owned();
        // 回调不得携带显式 :443 端口（issue #140 的 400 根因）
        assert!(!device_url.contains(":443"), "oauth_callback 不得携带 :443 端口");
        // 回调 query 参数完整性（模拟授权页自身生成的形态，与真实客户端跳转参数集一致）
        for key in ["challenge=", "challenge_method=S256", "nonce=", "machine_id=", "directLogin=true", "client_id="] {
            assert!(device_url.contains(key), "oauth_callback 缺少 {key}");
        }
        // 值绑定：URL 中必须是本会话 f1 的值（防参数互串回归）
        assert!(
            device_url.contains(&format!("nonce={}", f1.nonce)),
            "oauth_callback nonce 未绑定本会话"
        );
        assert!(
            device_url.contains(&format!("machine_id={}", f1.machine_id)),
            "oauth_callback machine_id 未绑定本会话"
        );
        // client_id 必须是官方注册固定常量（随机 uuid 会被判「参数无效」）
        assert!(
            device_url.contains(&format!("client_id={DEVICE_AUTH_CLIENT_ID}")),
            "oauth_callback client_id 必须为官方固定常量"
        );
        // directLogin 仅出现一次（2026-10-02 实测：重复参数被授权页判「参数无效」）
        assert_eq!(device_url.matches("directLogin").count(), 1, "directLogin 只能出现一次");
        // challenge 可由 verifier 复算（S256 绑定）
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(f1.verifier.as_bytes()));
        assert!(device_url.contains(&format!("challenge={expect}")));
    }

    /// 兼容模式（2026-10-02 审查预案）：不带 client_id，其余参数集与 PKCE 绑定不变
    #[test]
    fn flow_compat_omits_client_id() {
        let f = DeviceFlow::new_compat();
        assert!(f.client_id.is_none());
        let (_, cb) = f.auth_url.split_once("?oauth_callback=").unwrap();
        let device_url = urlencoding::decode(cb).unwrap().into_owned();
        assert!(!device_url.contains("client_id="), "兼容模式不应携带 client_id");
        assert!(
            device_url.contains("challenge=") && device_url.contains("challenge_method=S256")
        );
        assert!(device_url.contains("nonce=") && device_url.contains("machine_id="));
        assert!(!device_url.contains(":443"));
        assert_eq!(device_url.matches("directLogin").count(), 1);
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(f.verifier.as_bytes()));
        assert!(device_url.contains(&format!("challenge={expect}")));
    }

    /// SSO 基址环境变量覆盖与回退（审查修复 低-4）：参数注入版解析核心，
    /// 非法/空/缺失值一律回退默认常量，合法 http(s) URL 去尾斜杠后生效；
    /// 覆盖生效/非法回退必须携带诊断日志（审查修复 中-1），缺失/空白不带
    #[test]
    fn sso_login_base_env覆盖与回退() {
        // 缺失 / 空白 / 非法协议 → 回退默认常量；缺失/空白不落日志
        assert_eq!(sso_login_base_from(None), (SSO_LOGIN_BASE.to_string(), None));
        assert_eq!(sso_login_base_from(Some("")), (SSO_LOGIN_BASE.to_string(), None));
        assert_eq!(sso_login_base_from(Some("   ")), (SSO_LOGIN_BASE.to_string(), None));
        // 非法协议 → 回退 + 诊断日志（「改了 env 没生效」的可观测性）
        let (_, note) = sso_login_base_from(Some("ftp://mirror.example.com"));
        assert!(note.unwrap().contains("ftp://mirror.example.com"));
        let (_, note) = sso_login_base_from(Some("qoder.cn/sso/login/aliyun"));
        assert!(note.unwrap().contains("qoder.cn/sso/login/aliyun"));
        // 合法覆盖：https / http 均接受；首尾空白与尾部斜杠被清理；均携带生效日志
        let (base, note) = sso_login_base_from(Some("https://mirror.example.com/sso/login/aliyun/"));
        assert_eq!(base, "https://mirror.example.com/sso/login/aliyun");
        assert!(note.unwrap().starts_with("AIWORK_QODER_SSO_BASE 覆盖生效: "));
        let (base, note) = sso_login_base_from(Some("  http://127.0.0.1:8080/sso/login/aliyun  "));
        assert_eq!(base, "http://127.0.0.1:8080/sso/login/aliyun");
        let note = note.unwrap();
        assert!(note.contains("警告：非 https") && note.contains("127.0.0.1:8080"));
        // 覆盖基址确实拼入 DeviceFlow 的 auth_url 前缀（参数注入直调 build，
        // 不触碰进程级 env；默认常量路径另由 flow_generates_distinct_sessions_and_pkce_url 锁定）
        let f = DeviceFlow::build(true, Some("https://mirror.example.com/sso/login/aliyun".into()));
        assert!(f.auth_url.starts_with("https://mirror.example.com/sso/login/aliyun?oauth_callback="));
        // 注入基址不携带 env 诊断（该路径不经 env 解析）
        assert!(f.sso_base_note.is_none());
    }

    #[test]
    fn parse_poll_success_maps_captured_shape() {
        // R-10 抓包样本（token/refresh_token 已脱敏为占位）
        let body: Value = serde_json::from_str(
            r#"{"id":"01a0e03b-5f90-73b8-958e-443488ccf81e","token":"dt-placeholder","user_id":"01a0dff8-cc47-7b64-bd6d-d9cb2aea6792","code_challenge":"kiGczYiWCRH2Csra26Fx4NlhHp2n2CLT8QhMD678dqQ","code_challenge_method":"S256","nonce":"8872dcb1-d9c6-4453-bfb6-3bcbe9b0500b","expires_at":"2026-10-27T00:19:42Z","refresh_token_id":"01a0e03b-5f8f-7e4c-b112-1de3f25908cd","created_at":"2026-09-27T00:19:42Z","updated_at":"2026-09-27T00:19:42Z","refresh_token":"rt-placeholder","expires_in":2591999999,"refresh_token_expires_in":31103999999,"refresh_token_expires_at":"2027-09-22T00:19:42Z"}"#,
        )
        .unwrap();
        let (creds, uid) = parse_poll_success(&body, "8872dcb1-d9c6-4453-bfb6-3bcbe9b0500b").unwrap();
        assert_eq!(creds.access_token, "dt-placeholder");
        assert_eq!(creds.kind, "client");
        assert_eq!(uid, "01a0dff8-cc47-7b64-bd6d-d9cb2aea6792");
        // ≈30 天
        let now = chrono::Utc::now().timestamp_millis();
        let days = (creds.expires_at_ms.unwrap() - now) as f64 / 86_400_000.0;
        assert!((29.0..=31.0).contains(&days));
        // refresh_token_expires_in=31103999999 ms ≈360 天（审查 L-RT 持久化）
        let rt_days = (creds.refresh_expires_at_ms.unwrap() - now) as f64 / 86_400_000.0;
        assert!((350.0..=370.0).contains(&rt_days));
    }

    #[test]
    fn parse_poll_success_rejects_pending_shape() {
        let body: Value =
            serde_json::from_str(r#"{"errorCode":"NotFound","errorMessage":"Not found"}"#).unwrap();
        assert!(parse_poll_success(&body, "n").is_none());
    }

    /// nonce 回验（审查 L）：响应 nonce 与本会话不一致 → 拒绝（防会话混淆）
    #[test]
    fn parse_poll_success_rejects_nonce_mismatch() {
        let body: Value = serde_json::from_str(
            r#"{"token":"dt-x","user_id":"u1","nonce":"session-a","expires_in":2591999999}"#,
        )
        .unwrap();
        assert!(parse_poll_success(&body, "session-a").is_some());
        assert!(parse_poll_success(&body, "session-b").is_none(), "nonce 不匹配必须拒绝");
    }
}
