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
    /// 客户端指纹：按账号稳定派生的设备标识。
    ///
    /// 为什么需要：上游按「同一客户端 / 同一设备」识别调用来源。缺少这组头时，
    /// 一台机器上的全部账号会被上游归并为同一调用来源，账号之间无法区分。
    /// 每个账号派生一份稳定指纹后，两个账号在协议层面就是两台不同的客户端。
    ///
    /// 由账号稳定 id 摘要派生（不含明文），进程重启、凭据轮转都不变，同账号恒同值。
    pub fingerprint: CodeartsFingerprint,
}

/// 账号级客户端指纹（对齐真实 CodeArts Agent 客户端的标识头集合）。
///
/// 真实客户端固定发送：`ide-name: CodeArts Agent` / `ide-version: 1.109.5` /
/// `plugin-name: snap_vscode` / `plugin-version: 26.11.200` / `x-snap-traceid`。
/// 版本类字段取真实客户端同款常量（便于上游按合法客户端版本识别）；
/// `trace_id` 与 `user_agent` 按账号派生（trace_id 每请求随机，UA 稳定）。
#[derive(Debug, Clone, Default)]
pub struct CodeartsFingerprint {
    /// 设备派生 seed（sha256(账号稳定 id) 前 16 hex，不含明文）
    pub seed: String,
    /// 客户端 UA：按 seed 派生设备号，形如真实 Electron 客户端
    pub user_agent: String,
}

/// 真实客户端常量（逆向自 CodeArts Agent 26.11.200 的请求头）。
pub const IDE_NAME: &str = "CodeArts Agent";
pub const IDE_VERSION: &str = "1.109.5";
pub const PLUGIN_NAME: &str = "snap_vscode";
pub const PLUGIN_VERSION: &str = "26.11.200";

impl CodeartsFingerprint {
    /// 按账号稳定标识派生指纹。**同账号恒同值**（进程重启、凭据轮转都不变）。
    ///
    /// `account_seed` 必须取**账号稳定标识**，不能取 AK：OAuth 账号的 AK 是 STS
    /// 临时凭据，每次续期都会换成新值——若按 AK 派生，指纹会跟着凭据一起漂移，
    /// 上游看到的就是「同一账号不断换设备」，比不区分更可疑。调用方传入
    /// 账号池 id（`ca-o-…`，由上游稳定 user_id 派生）或手工 AK/SK 的派生 id。
    pub fn derive(account_seed: &str) -> Self {
        let digest = Sha256::digest(account_seed.as_bytes());
        let seed: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        // UA 用 seed 的 8 位十六进制做设备段，形态对齐真实 Electron 客户端
        let user_agent = format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) CodeArts Agent/{IDE_VERSION} Chrome/126.0.6478.234 \
             Electron/31.7.7 Safari/537.36 device/{seed}"
        );
        Self { seed, user_agent }
    }

    /// 设备段（排障用：确认两个账号的指纹确实不同，不含任何凭据明文）。
    pub fn device_segment(&self) -> &str {
        &self.seed
    }
}


impl CodeartsIdentity {
    pub fn new(ak: impl Into<String>, sk: impl Into<String>) -> Self {
        let ak = ak.into();
        Self {
            fingerprint: CodeartsFingerprint::derive(&ak),
            ak,
            sk: sk.into(),
            security_token: String::new(),
        }
    }

    /// OAuth 临时凭据构造。
    pub fn new_oauth(ak: impl Into<String>, sk: impl Into<String>, security_token: impl Into<String>) -> Self {
        let ak = ak.into();
        Self {
            fingerprint: CodeartsFingerprint::derive(&ak),
            ak,
            sk: sk.into(),
            security_token: security_token.into(),
        }
    }

    pub fn is_valid(&self) -> bool {
        !self.ak.trim().is_empty() && !self.sk.trim().is_empty()
    }

    pub fn is_oauth(&self) -> bool {
        !self.security_token.trim().is_empty()
    }

    /// 客户端标识头（不参与签名，仅随请求发送）：按账号区分调用来源。
    ///
    /// 刻意不放进 `sign()` 的 extra 头集——这组头是**客户端身份**而非请求内容，
    /// 不参与签名可避免改动既有 5 个签名向量（AK/SK 与 OAuth 各半）。
    pub fn client_headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("ide-name", IDE_NAME.to_string()),
            ("ide-version", IDE_VERSION.to_string()),
            ("plugin-name", PLUGIN_NAME.to_string()),
            ("plugin-version", PLUGIN_VERSION.to_string()),
            ("user-agent", self.fingerprint.user_agent.clone()),
            ("x-snap-traceid", new_trace_id()),
        ]
    }
}

/// 每请求随机的 32 位十六进制 trace id（对齐真实客户端 `x-snap-traceid` 形状）。
pub fn new_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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
///
/// `account_id`：账号池稳定 id（`ca-…`），用于派生**不随凭据轮转漂移**的客户端指纹。
/// OAuth 的 AK 每次续期都换新，若拿 AK 派生指纹，同一账号会不断"换设备"。
pub fn identity_from_creds(
    creds: &crate::tasks::codearts_common::CodeartsCreds,
    account_id: &str,
) -> CodeartsIdentity {
    let mut id = if creds.is_oauth() {
        CodeartsIdentity::new_oauth(&creds.ak, &creds.sk, &creds.security_token)
    } else {
        CodeartsIdentity::new(&creds.ak, &creds.sk)
    };
    // 指纹改按稳定账号 id 派生（缺 id 时保留 AK 派生作兜底，仅测试路径会走到）
    if !account_id.trim().is_empty() {
        id.fingerprint = CodeartsFingerprint::derive(account_id);
    }
    id
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

    /// 核心约束：不同账号必须得到不同指纹——这正是「把一个 TPM 拆成多个
    /// 调用来源」的协议层前提。两个账号的 UA / 设备段全等则等于没做区分。
    #[test]
    fn different_accounts_get_different_fingerprints() {
        let a = CodeartsIdentity::new_oauth("AK_ACCOUNT_A", "SK_A", "ST_A");
        let b = CodeartsIdentity::new_oauth("AK_ACCOUNT_B", "SK_B", "ST_B");
        assert_ne!(
            a.fingerprint.device_segment(),
            b.fingerprint.device_segment(),
            "两个账号的设备段必须不同"
        );
        assert_ne!(a.fingerprint.user_agent, b.fingerprint.user_agent, "两个账号的 UA 必须不同");
    }

    /// 同账号必须稳定：进程重启后指纹不变（否则每次重启都被上游当新设备，
    /// 反而制造异常特征）。
    #[test]
    fn same_account_fingerprint_is_stable() {
        let x = CodeartsFingerprint::derive("ca-o-stable-account-id");
        let y = CodeartsFingerprint::derive("ca-o-stable-account-id");
        assert_eq!(x.device_segment(), y.device_segment());
        assert_eq!(x.user_agent, y.user_agent);
    }

    /// 核心约束：指纹必须绑定**账号稳定 id**，不能绑定 AK。
    ///
    /// OAuth 的 AK 是 STS 临时凭据，每次续期都会换新。若指纹跟着 AK 漂移，
    /// 上游看到的就是「同一账号不断换设备」——比不区分更可疑，且会让上游
    /// 无法把多次请求归到同一客户端。
    #[test]
    fn fingerprint_survives_ak_rotation() {
        let account_id = "ca-o-435c429a2560";
        // 同一账号，两套不同 AK（模拟 STS 续期前后）
        let before = identity_from_creds(
            &crate::tasks::codearts_common::CodeartsCreds {
                ak: "HSTA_OLD_AK_1111".into(),
                sk: "SK_OLD".into(),
                security_token: "ST_OLD".into(),
                credential_source: "oauth".into(),
                ..Default::default()
            },
            account_id,
        );
        let after = identity_from_creds(
            &crate::tasks::codearts_common::CodeartsCreds {
                ak: "HSTA_NEW_AK_2222".into(),
                sk: "SK_NEW".into(),
                security_token: "ST_NEW".into(),
                credential_source: "oauth".into(),
                ..Default::default()
            },
            account_id,
        );
        // AK 变了，但指纹不变
        assert_ne!(before.ak, after.ak, "前提：两次的 AK 不同");
        assert_eq!(
            before.fingerprint.device_segment(),
            after.fingerprint.device_segment(),
            "AK 轮转后指纹必须保持不变"
        );
        assert_eq!(before.fingerprint.user_agent, after.fingerprint.user_agent);

        // 不同账号仍必须不同
        let other = identity_from_creds(
            &crate::tasks::codearts_common::CodeartsCreds {
                ak: "HSTA_NEW_AK_2222".into(),
                sk: "SK_NEW".into(),
                security_token: "ST_NEW".into(),
                credential_source: "oauth".into(),
                ..Default::default()
            },
            "ca-o-69763dee1143",
        );
        assert_ne!(
            after.fingerprint.device_segment(),
            other.fingerprint.device_segment(),
            "不同账号即使 AK 相同也必须不同指纹"
        );
    }

    /// 指纹段不得泄露 AK 明文（零明文红线）。
    #[test]
    fn fingerprint_does_not_leak_ak() {
        let ak = "AK_SUPER_SECRET_VALUE_12345";
        let fp = CodeartsFingerprint::derive(ak);
        assert!(!fp.user_agent.contains(ak));
        assert!(!fp.device_segment().contains(ak));
        // 设备段应为 16 位十六进制
        assert_eq!(fp.device_segment().len(), 16);
        assert!(fp.device_segment().chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 客户端标识头必须齐全，且版本类字段取真实客户端常量。
    #[test]
    fn client_headers_carry_real_client_identity() {
        let id = CodeartsIdentity::new_oauth("AK_X", "SK_X", "ST_X");
        let headers: std::collections::HashMap<&str, String> =
            id.client_headers().into_iter().collect();
        assert_eq!(headers.get("ide-name").map(String::as_str), Some(IDE_NAME));
        assert_eq!(headers.get("ide-version").map(String::as_str), Some(IDE_VERSION));
        assert_eq!(headers.get("plugin-name").map(String::as_str), Some(PLUGIN_NAME));
        assert_eq!(headers.get("plugin-version").map(String::as_str), Some(PLUGIN_VERSION));
        // trace id 每请求随机：两次调用必须不同
        let t1 = headers.get("x-snap-traceid").cloned().unwrap_or_default();
        let t2 = id
            .client_headers()
            .into_iter()
            .find(|(k, _)| *k == "x-snap-traceid")
            .map(|(_, v)| v)
            .unwrap_or_default();
        assert_eq!(t1.len(), 32, "trace id 应为 32 位 hex");
        assert_ne!(t1, t2, "trace id 每请求必须随机");
    }

    /// 客户端标识头不参与签名：加入它们不得改变既有签名向量（回归保护——
    /// 改动 header 集合会让 5 个签名向量全红，而这组头是客户端身份、非请求内容）。
    #[test]
    fn client_headers_do_not_affect_signature() {
        let id = identity();
        let signed = sign_with_date(&id, "GET", HOST_URL, b"", &[], TS).unwrap();
        // client_headers 只读不写签名：再次签名结果必须完全一致
        let _ = id.client_headers();
        let signed2 = sign_with_date(&id, "GET", HOST_URL, b"", &[], TS).unwrap();
        assert_eq!(signed.authorization, signed2.authorization);
        assert!(!signed.authorization.contains("ide-name"));
        assert!(!signed.authorization.contains("user-agent"));
    }
}
