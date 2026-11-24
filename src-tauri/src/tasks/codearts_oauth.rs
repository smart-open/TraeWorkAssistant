//! CodeArts（华为云码道）新式 IAM OAuth：PKCE(S256) + ES256 DPoP + STS token 交换。
//!
//! 参考真实 CodeArts 插件与社区实现（yuweihao17/codearts、dsh-codearts-auth）：
//! - 授权：`https://codearts.huaweicloud.com/portal/authorize`，本地 127.0.0.1 回调收 `code`；
//! - 换凭据：`https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens`，
//!   每次 token 请求都用一把 P-256 私钥签 DPoP JWS（htm=POST, htu=token url）；
//! - 响应含临时 AK/SK + security_token + refresh_token，到期前静默续期（同 verifier/DPoP key）。
//!
//! 本模块只做纯逻辑（URL 构造 / JWS 签发 / token 解析 / 续期），网络回调服务器在
//! commands/codearts/oauth.rs。所有安全随机走 `random_hex_result`（CSPRNG，不可用即 Err）。

use base64::Engine as _;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use sha2::Digest as _;
use serde_json::{json, Value};

use super::codearts_common::CodeartsCreds;

/// OAuth client id（等于插件 URI scheme，逆向自真实 IDE 扩展）。
pub const CLIENT_ID: &str = "codearts-agent";
/// 本地回调路径（端口由回环服务器决定，固定 ≥10000）。
pub const REDIRECT_PATH: &str = "/oauth/callback";
/// 授权门户。
pub const PORTAL_AUTHORIZE_BASE: &str = "https://codearts.huaweicloud.com/portal/authorize";
pub const PORTAL_LOGIN_BASE: &str = "https://codearts.huaweicloud.com/portal/login";
/// STS token 端点。
pub const STS_TOKEN_ENDPOINT: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";
/// 换取身份（可选，用于稳定 user_id）。
pub const STS_IDENTITY_ENDPOINT: &str = "https://sts.cn-north-4.myhuaweicloud.com/v5/caller-identity";
/// 插件标识（逆向自真实 IDE 扩展）。
const LOGIN_PLUGIN_NAME: &str = "snap_AIIDE";
const LOGIN_PLUGIN_VERSION: &str = "5.2.0";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn b64url(data: &[u8]) -> String {
    B64.encode(data)
}

/// PKCE 对：verifier = CSPRNG 64 hex；challenge = BASE64URL(SHA256(verifier))。
pub fn pkce_pair() -> Result<(String, String), String> {
    let verifier = crate::commands::oauth::random_hex_result(64)?;
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    Ok((verifier.clone(), b64url(&digest)))
}

/// 生成 DPoP ES256(P-256) 私钥，返回 PKCS#8 PEM（明文唯一驻留 vault）。
pub fn generate_dpop_key() -> Result<String, String> {
    // 32 字节 CSPRNG 种子（64 个 hex 字符）→ SigningKey
    let seed_hex = crate::commands::oauth::random_hex_result(64)?;
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("DPoP 密钥种子解码失败: {e}"))?;
    }
    let signing = SigningKey::from_bytes((&seed).into())
        .map_err(|e| format!("DPoP 密钥生成失败: {e}"))?;
    Ok(signing
        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
        .map_err(|e| format!("DPoP 私钥编码失败: {e}"))?
        .to_string())
}

/// 从 PEM 恢复公钥 JWK（ES256 protected header 用）。
fn public_jwk(signing: &SigningKey) -> Value {
    let point = signing.verifying_key().to_encoded_point(false);
    let bytes = point.as_bytes();
    // 未压缩点：04 || x(32) || y(32)
    let x = if bytes.len() >= 65 { &bytes[1..33] } else { &[] };
    let y = if bytes.len() >= 65 { &bytes[33..65] } else { &[] };
    json!({
        "kty": "EC",
        "crv": "P-256",
        "x": b64url(x),
        "y": b64url(y),
    })
}

/// 签 DPoP JWS（ES256）。header ~ {"alg":"ES256","typ":"dpop+jwt","jwk":pub}，
/// payload = {htm,htu,iat,jti}；签名 = base64url(raw r||s)（JWS ES256 格式）。
pub fn sign_dpop_jws(dpop_key_pem: &str, htm: &str, htu: &str) -> Result<String, String> {
    let signing = SigningKey::from_pkcs8_pem(dpop_key_pem)
        .map_err(|e| format!("DPoP 私钥解析失败: {e}"))?;
    let header = json!({
        "alg": "ES256",
        "typ": "dpop+jwt",
        "jwk": public_jwk(&signing),
    });
    let iat = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let jti = crate::commands::oauth::random_hex_result(32)?;
    let payload = json!({ "htm": htm, "htu": htu, "iat": iat, "jti": jti });
    let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(payload.to_string().as_bytes())
    );
    let sig: Signature = signing.sign(signing_input.as_bytes());
    Ok(format!("{}.{}", signing_input, b64url(&sig.to_bytes())))
}

/// 构造 portal 授权 URL（参数镜像真实 IDE 扩展）。
pub fn build_authorize_url(port: u16, ticket_id: &str, code_challenge: &str) -> String {
    format!(
        "{PORTAL_AUTHORIZE_BASE}?theme=2&locale=zh-cn\
         &uri_scheme={CLIENT_ID}&client_id={CLIENT_ID}&port={port}\
         &code_challenge={code_challenge}&code_challenge_method=SHA-256\
         &ticket_id={ticket_id}&plugin-name={LOGIN_PLUGIN_NAME}&plugin-version={LOGIN_PLUGIN_VERSION}"
    )
}

/// 授权成功后的 portal 结果页 URL（回调服务器重定向到它）。
pub fn portal_result_url(ok: bool) -> String {
    format!(
        "{PORTAL_LOGIN_BASE}?login_succeed={}&uri_scheme={CLIENT_ID}&locale=zh-cn",
        if ok { "true" } else { "false" }
    )
}

/// 解析 STS token 响应为统一凭据。extra 携带换 token 用的 verifier / dpop key。
/// STS 凭据字段：`credentials.{access_key_id,secret_access_key,security_token,expiration}`。
pub fn credential_from_token(token: &Value, verifier: &str, dpop_key_pem: &str) -> Result<CodeartsCreds, String> {
    let c = token.get("credentials").ok_or("STS 响应缺少 credentials")?;
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let ak = s("access_key_id");
    let sk = s("secret_access_key");
    if ak.is_empty() || sk.is_empty() {
        return Err("STS 凭据缺少 access_key_id/secret_access_key".into());
    }
    // expiration 可能是 RFC3339 或毫秒时间戳；统一为 Unix 秒
    let expires_at = parse_expiration(c.get("expiration"))
        .or_else(|| parse_expiration(token.get("expiration")))
        .unwrap_or(0);
    let refresh_token = token
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(CodeartsCreds {
        ak,
        sk,
        security_token: s("security_token"),
        expires_at,
        refresh_token,
        code_verifier: verifier.to_string(),
        dpop_key_pem: dpop_key_pem.to_string(),
        credential_source: "oauth".into(),
    })
}

/// 静默续期：用 refresh_token + 原 code_verifier / DPoP key 换一组新凭据。
/// 复用同一 verifier 与 DPoP 私钥（STS 要求与首次登录一致）。
pub fn refresh_credential(creds: &CodeartsCreds) -> Result<CodeartsCreds, String> {
    if creds.refresh_token.is_empty() || creds.code_verifier.is_empty() || creds.dpop_key_pem.is_empty() {
        return Err("CodeArts 凭据缺少续期材料（refresh_token/verifier/DPoP key）".into());
    }
    let dpop = sign_dpop_jws(&creds.dpop_key_pem, "POST", STS_TOKEN_ENDPOINT)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &creds.refresh_token)
        .append_pair("code_verifier", &creds.code_verifier)
        .finish();
    let agent = crate::tasks::http_agent(30);
    // STS token 端点只认 DPoP 头；发 Authorization 会被当 AK/SK 解析而 401（APIGW.0301）
    let resp = agent
        .post(STS_TOKEN_ENDPOINT)
        .set("DPoP", &dpop)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let text = r.into_string().unwrap_or_default();
                format!("CodeArts 续期失败（HTTP {code}）：{}", text.chars().take(200).collect::<String>())
            }
            other => format!("CodeArts 续期网络错误：{other}"),
        })?;
    let token: Value = resp.into_json().map_err(|e| format!("续期响应解析失败：{e}"))?;
    let mut next = credential_from_token(&token, &creds.code_verifier, &creds.dpop_key_pem)?;
    // STS 可能不返回新 refresh_token，沿用旧的
    if next.refresh_token.is_empty() {
        next.refresh_token = creds.refresh_token.clone();
    }
    Ok(next)
}

fn parse_expiration(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => {
            let x = n.as_i64()?;
            Some(if x > 10_000_000_000 { x / 1000 } else { x })
        }
        Some(Value::String(s)) => {
            if let Ok(x) = s.parse::<i64>() {
                return Some(if x > 10_000_000_000 { x / 1000 } else { x });
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_is_s256_shape() {
        let (v, c) = pkce_pair().unwrap();
        assert_eq!(v.len(), 64);
        assert!(!c.is_empty() && !c.contains('=') && !c.contains('+'));
        assert_eq!(c, b64url(&sha2::Sha256::digest(v.as_bytes())));
    }

    #[test]
    fn dpop_jws_has_three_segments_and_es256_header() {
        let pem = generate_dpop_key().unwrap();
        let jws = sign_dpop_jws(&pem, "POST", STS_TOKEN_ENDPOINT).unwrap();
        let parts: Vec<&str> = jws.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "dpop+jwt");
        let payload: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["htm"], "POST");
        assert_eq!(payload["htu"], STS_TOKEN_ENDPOINT);
    }

    #[test]
    fn token_response_parsed_to_oauth_creds() {
        let token = json!({
            "credentials": {
                "access_key_id": "AK", "secret_access_key": "SK",
                "security_token": "ST", "expiration": "2026-11-18T12:00:00Z"
            },
            "refresh_token": "RT"
        });
        let c = credential_from_token(&token, "verifier", "dpop-pem").unwrap();
        assert_eq!(c.ak, "AK");
        assert_eq!(c.sk, "SK");
        assert_eq!(c.security_token, "ST");
        assert_eq!(c.refresh_token, "RT");
        assert!(c.is_oauth());
        assert!(c.expires_at > 0);
    }
}
