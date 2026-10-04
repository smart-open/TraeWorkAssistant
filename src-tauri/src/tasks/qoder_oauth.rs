//! Qoder OAuth 设备流（F-80 M1，R-10 抓包固化 2026-09-27）。
//!
//! 客户端真实链路（PKCE device flow）：
//! 1. 生成 `nonce`（uuid v4）、`verifier`（64 字符）、`machine_id`（uuid v4）；`client_id` 用官方固定常量
//! 2. 浏览器打开 `https://qoder.cn/device/selectAccounts?challenge=<BASE64URL(SHA256(verifier))>
//!    &challenge_method=S256&nonce=..&machine_id=..&client_id=732aef47-..`
//!    （用户在页面完成授权；**client_id 必须用官方注册的固定常量**——2026-10-02 实测：
//!    随机生成 client_id 会被授权页判「参数无效」。该值取自新版 Qoder CN 客户端
//!    `authClientIds.prod`，与真实客户端跳转样例逐字一致；社区实现（10router/
//!    cockpit-tools）则完全不带 client_id，同样可用。**任何实现都不携带 directLogin**，
//!    回调 URL 里的 directLogin 系授权页登录流程自行追加，与发起方无关）
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
    /// 授权页完整 URL（challenge = BASE64URL(SHA256(verifier))，S256）
    pub auth_url: String,
}

impl DeviceFlow {
    /// 常规模式：携带官方注册固定 client_id（与真实客户端跳转逐字一致）
    pub fn new() -> Self {
        Self::build(true)
    }

    /// 兼容模式：不带 client_id（2026-10-02 审查预案落地）——官方常量一旦被
    /// Qoder 轮换导致授权页「参数无效」，改用本模式即可恢复（前端在授权超时后
    /// 自动切换到本模式重试）。
    pub fn new_compat() -> Self {
        Self::build(false)
    }

    fn build(with_client_id: bool) -> Self {
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
        let mut url = format!(
            "{DEVICE_AUTH_BASE}?challenge={challenge}&challenge_method=S256&nonce={nonce}&machine_id={machine_id}"
        );
        if let Some(cid) = &client_id {
            url.push_str(&format!("&client_id={cid}"));
        }
        Self { nonce, verifier, machine_id, client_id, auth_url: url }
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
        urlencode(&flow.verifier)
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
        .map(|e| now_ms + qoder_common::normalize_expires_in(e));
    let refresh_expires_at_ms = fs_utils::dig(
        body,
        &["refresh_token_expires_in", "refreshTokenExpiresIn"],
    )
    .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok()))
    .map(|e| now_ms + qoder_common::normalize_expires_in(e));
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

/// verifier 进 query 前的最小转义（verifier 实为双 uuid4 simple 拼接的 64 位纯 hex，
/// 正常不含任何需转义字符；此转义仅为防御性兜底，防 verifier 构造变化引入保留字）
fn urlencode(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20").replace('&', "%26").replace('=', "%3D").replace('~', "%7E")
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
        assert!(f1.auth_url.starts_with(DEVICE_AUTH_BASE));
        // query 参数完整性（与真实客户端跳转参数集一致）
        for key in ["challenge=", "challenge_method=S256", "nonce=", "machine_id=", "client_id="] {
            assert!(f1.auth_url.contains(key), "auth_url 缺少 {key}");
        }
        // client_id 必须是官方注册固定常量（随机 uuid 会被判「参数无效」）
        assert!(
            f1.auth_url.contains(&format!("client_id={DEVICE_AUTH_CLIENT_ID}")),
            "auth_url client_id 必须为官方固定常量"
        );
        // 不携带 directLogin：官方客户端与社区实现均不发送该参数
        assert!(!f1.auth_url.contains("directLogin"), "auth_url 不应携带 directLogin");
        // challenge 可由 verifier 复算（S256 绑定）
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(f1.verifier.as_bytes()));
        assert!(f1.auth_url.contains(&format!("challenge={expect}")));
    }

    /// 兼容模式（2026-10-02 审查预案）：不带 client_id，其余参数集与 PKCE 绑定不变
    #[test]
    fn flow_compat_omits_client_id() {
        let f = DeviceFlow::new_compat();
        assert!(f.client_id.is_none());
        assert!(!f.auth_url.contains("client_id="), "兼容模式不应携带 client_id");
        assert!(f.auth_url.contains("challenge=") && f.auth_url.contains("challenge_method=S256"));
        assert!(f.auth_url.contains("nonce=") && f.auth_url.contains("machine_id="));
        assert!(!f.auth_url.contains("directLogin"));
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(f.verifier.as_bytes()));
        assert!(f.auth_url.contains(&format!("challenge={expect}")));
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
