//! CodeArts（华为云码道）上游 SDK-HMAC-SHA256 请求签名。
//!
//! 国内站固定区域 `cn-north-4`。所有上游请求（模型推理 / 活动签到 / 积分查询 /
//! 模型目录）都用 AK/SK 做华为云 APIG 的 `SDK-HMAC-SHA256` 签名，凭据长期有效，
//! 无需浏览器 OAuth（OAuth 仅作为后续可选的凭据录入通道）。
//!
//! 算法（对齐社区实测实现 codearts2api / magpie-codearts-auth）：
//! 1. 收集参与签名的头：调用方附加头（如小写 `agent-type: PromptCenter`）+ `host`
//!    + `content-type`（缺省 application/json）+ `x-sdk-date`（UTC 时间戳）；
//! 2. 头名一律小写、按字典序排序，`SignedHeaders` 用 `;` 连接；
//! 3. CanonicalRequest = METHOD + URI(带尾斜杠) + CanonicalQuery + CanonicalHeaders
//!    + SignedHeaders + SHA256(body)，各段以 `\n` 分隔；
//! 4. StringToSign = "SDK-HMAC-SHA256" + x-sdk-date + SHA256(CanonicalRequest)；
//! 5. Signature = HMAC-SHA256(SK, StringToSign) 的十六进制；
//! 6. `Authorization: SDK-HMAC-SHA256 Access={AK}, SignedHeaders={...}, Signature={...}`。
//!
//! 关键坑（社区实测）：`/v1/ops/*` 的可选头必须小写 `agent-type`（大写 `Agent-Type`
//! 被 APIG 当系统头、不入 SignedHeaders，直接 401 APIG.0301）；`/v1/ops/*` 前缀
//! 不能带 `snap-manager`，而积分统计 `/snap-manager/v1/statistics/plugin` 必须带。

use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// 国内站上游主机（snap 引擎：模型 / ops 活动 / 模型目录 / 积分统计共用）。
pub const SNAP_HOST: &str = "snap-access.cn-north-4.myhuaweicloud.com";
/// 模型推理端点。
pub const CHAT_PATH: &str = "/api/v2/chat/completions";
/// 活动签到：列出活动与状态（四态预检，唯一真相源）。
pub const OPS_DELIVERY_PATH: &str = "/v1/ops/delivery";
/// 活动签到：领取（claim）。
pub const OPS_CLAIM_PATH: &str = "/v1/ops/claim";
/// 活动签到：确认（confirm，claim 后服务端要求时补）。
pub const OPS_CONFIRM_PATH: &str = "/v1/ops/confirm";
/// 模型目录（upstream 真实模型清单）。
pub const MODEL_BUILTIN_PATH: &str = "/v1/model/builtin";
/// 积分统计（必须带 snap-manager 前缀）。
pub const STATS_PLUGIN_PATH: &str = "/snap-manager/v1/statistics/plugin";

/// AK/SK 凭据（长期有效 or OAuth 临时凭据）。
/// OAuth 模式 security_token 非空，签名时额外参与头集。
#[derive(Debug, Clone, Default)]
pub struct CodeartsIdentity {
    pub ak: String,
    pub sk: String,
    /// OAuth STS 临时安全令牌（手工 AK/SK 模式为空）
    pub security_token: String,
}

impl CodeartsIdentity {
    pub fn new(ak: impl Into<String>, sk: impl Into<String>) -> Self {
        Self { ak: ak.into(), sk: sk.into(), security_token: String::new() }
    }

    /// OAuth 临时凭据构造。
    pub fn new_oauth(ak: impl Into<String>, sk: impl Into<String>, security_token: impl Into<String>) -> Self {
        Self { ak: ak.into(), sk: sk.into(), security_token: security_token.into() }
    }

    pub fn is_valid(&self) -> bool {
        !self.ak.trim().is_empty() && !self.sk.trim().is_empty()
    }

    pub fn is_oauth(&self) -> bool {
        !self.security_token.trim().is_empty()
    }
}

/// 已签名请求头集合（返回顺序即发送顺序；调用方逐条 set 到 ureq 请求）。
#[derive(Debug, Clone)]
pub struct SignedHeaders {
    pub authorization: String,
    pub x_sdk_date: String,
    pub content_type: String,
    /// OAuth 模式：x-security-token 值（手工 AK/SK 模式为 None，不发送）。
    pub x_security_token: Option<String>,
    /// OAuth 模式：x-sdk-content-sha256 值（body 的 SHA256 hex）。
    pub x_sdk_content_sha256: Option<String>,
    /// 调用方附加头（小写名 → 值），如 agent-type。
    pub extra: Vec<(String, String)>,
}

/// 用固定时间戳签名（测试用，锁定向量）。
pub fn sign_with_date(
    identity: &CodeartsIdentity,
    method: &str,
    url: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
    sdk_date: &str,
) -> Result<SignedHeaders, String> {
    if !identity.is_valid() {
        return Err("CodeArts AK/SK 为空".into());
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("非法 URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL 缺少 host".to_string())?;
    // host 含端口时（默认端口由 url crate 规整）按权威名参与签名
    let host = match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };

    // 头名一律小写；额外头在前，host/content-type/x-sdk-date 由框架补齐。
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in extra_headers {
        let key = k.trim().to_ascii_lowercase();
        if key.is_empty() || key == "authorization" {
            continue;
        }
        headers.push((key, v.trim().to_string()));
    }
    headers.push(("host".into(), host));
    if !headers.iter().any(|(k, _)| k == "content-type") {
        headers.push(("content-type".into(), "application/json".into()));
    }
    headers.retain(|(k, _)| k != "x-sdk-date");
    headers.push(("x-sdk-date".into(), sdk_date.to_string()));
    // OAuth 临时凭据模式：x-security-token / x-sdk-content-sha256 参与签名
    // （对齐社区参考 signRequestHuawei 头集；手工 AK/SK 模式两者均不加入，保持既有向量）
    let oauth_mode = identity.is_oauth();
    if oauth_mode {
        headers.retain(|(k, _)| k != "x-security-token" && k != "x-sdk-content-sha256");
        headers.push(("x-security-token".into(), identity.security_token.trim().to_string()));
        headers.push(("x-sdk-content-sha256".into(), hex(&Sha256::digest(body))));
    }

    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let signed_names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed_headers = signed_names.join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v))
        .collect();

    // URI：path 必须以 / 结尾（无 query 的简单路径可直接补）。
    let mut canonical_uri = parsed.path().to_string();
    if !canonical_uri.ends_with('/') {
        canonical_uri.push('/');
    }
    // query：RFC3986 编码后按 key（再 value）排序。
    let canonical_query = canonical_query(&parsed);

    let payload_hash = hex(&Sha256::digest(body));
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let string_to_sign = format!(
        "SDK-HMAC-SHA256\n{sdk_date}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(identity.sk.as_bytes())
        .map_err(|_| "HMAC 初始化失败".to_string())?;
    mac.update(string_to_sign.as_bytes());
    let signature = hex(&mac.finalize().into_bytes());

    let authorization = format!(
        "SDK-HMAC-SHA256 Access={}, SignedHeaders={}, Signature={}",
        identity.ak, signed_headers, signature
    );
    let content_type = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "application/json".into());
    let extra = headers
        .into_iter()
        .filter(|(k, _)| {
            !(k == "host" || k == "content-type" || k == "x-sdk-date"
                || k == "x-security-token" || k == "x-sdk-content-sha256")
        })
        .collect();

    Ok(SignedHeaders {
        authorization,
        x_sdk_date: sdk_date.to_string(),
        content_type,
        x_security_token: if oauth_mode { Some(identity.security_token.trim().to_string()) } else { None },
        x_sdk_content_sha256: if oauth_mode { Some(hex(&Sha256::digest(body))) } else { None },
        extra,
    })
}

/// 用当前 UTC 时间戳签名（生产路径）。
pub fn sign(
    identity: &CodeartsIdentity,
    method: &str,
    url: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> Result<SignedHeaders, String> {
    let sdk_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    sign_with_date(identity, method, url, body, extra_headers, &sdk_date)
}

/// 从统一凭据构造签名身份：OAuth 凭据（security_token 非空）自动带上 security_token，
/// 手工 AK/SK 凭据走既有路径（签名向量不变）。所有上游调用点统一经此函数，避免
/// OAuth 账号漏带 x-security-token 被 APIG 拒签。
pub fn identity_from_creds(creds: &crate::tasks::codearts_common::CodeartsCreds) -> CodeartsIdentity {
    if creds.is_oauth() {
        CodeartsIdentity::new_oauth(&creds.ak, &creds.sk, &creds.security_token)
    } else {
        CodeartsIdentity::new(&creds.ak, &creds.sk)
    }
}

/// RFC3986 query 规整：key / value 均按 unreserved 保留、其余 %XX（大写 hex）编码，
/// 再按 (key, value) 字典序排序，用 `&` 连接。
fn canonical_query(url: &url::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (pct_encode(k.as_ref()), pct_encode(v.as_ref())))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> CodeartsIdentity {
        CodeartsIdentity::new("AK_TEST123", "SK_TEST456")
    }

    const TS: &str = "20261117T120000Z";
    const HOST_URL: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";

    #[test]
    fn chat_signature_matches_reference_vector() {
        let body = br#"{"model":"glm-5.3","messages":[]}"#;
        let signed = sign_with_date(
            &identity(),
            "POST",
            &format!("{HOST_URL}{CHAT_PATH}"),
            body,
            &[],
            TS,
        )
        .unwrap();
        assert_eq!(
            signed.authorization,
            "SDK-HMAC-SHA256 Access=AK_TEST123, SignedHeaders=content-type;host;x-sdk-date, \
             Signature=2fa6b555ef59f2d9e3cdd981dfb92a62b8f57b40b1842902e42b3582c223dab3"
        );
    }

    #[test]
    fn oauth_chat_signature_matches_reference_vector() {
        // OAuth 临时凭据：x-security-token / x-sdk-content-sha256 参与签名（向量由 Python 参考实现生成）
        let body = br#"{"model":"glm-5.3","messages":[]}"#;
        let id = CodeartsIdentity::new_oauth("AK_TEST123", "SK_TEST456", "ST_TEST789");
        let signed = sign_with_date(&id, "POST", &format!("{HOST_URL}{CHAT_PATH}"), body, &[], TS).unwrap();
        assert_eq!(
            signed.authorization,
            "SDK-HMAC-SHA256 Access=AK_TEST123, \
             SignedHeaders=content-type;host;x-sdk-content-sha256;x-sdk-date;x-security-token, \
             Signature=f542fbba5492bb845e37779ab2f72d4876c3f0c230f64cc760efe37840a85404"
        );
        assert_eq!(signed.x_security_token.as_deref(), Some("ST_TEST789"));
        assert!(signed.x_sdk_content_sha256.is_some());
    }

    #[test]
    fn ops_delivery_signature_matches_reference_vector() {
        let signed = sign_with_date(
            &identity(),
            "GET",
            &format!("{HOST_URL}{OPS_DELIVERY_PATH}?channel=IDE"),
            b"",
            &[("agent-type", "PromptCenter")],
            TS,
        )
        .unwrap();
        assert_eq!(
            signed.authorization,
            "SDK-HMAC-SHA256 Access=AK_TEST123, SignedHeaders=agent-type;content-type;host;x-sdk-date, \
             Signature=09cdffd4f60e6b2e266c508924dc5eb3cb00ada240ea6c5d2a6f292bdd3c7f7d"
        );
    }

    #[test]
    fn ops_claim_signature_matches_reference_vector() {
        let body = br#"{"campaignId":1,"channel":"IDE","idempotentKey":"k1"}"#;
        let signed = sign_with_date(
            &identity(),
            "POST",
            &format!("{HOST_URL}{OPS_CLAIM_PATH}"),
            body,
            &[("agent-type", "PromptCenter")],
            TS,
        )
        .unwrap();
        assert_eq!(
            signed.authorization,
            "SDK-HMAC-SHA256 Access=AK_TEST123, SignedHeaders=agent-type;content-type;host;x-sdk-date, \
             Signature=eca12570c5e542d28e884258247e3915b62b446716c49f242bec773831ad0283"
        );
    }

    #[test]
    fn empty_credentials_rejected() {
        assert!(sign(&CodeartsIdentity::default(), "GET", HOST_URL, b"", &[]).is_err());
    }
}
