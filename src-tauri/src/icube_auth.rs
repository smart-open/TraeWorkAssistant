//! Trae 客户端 icube 设备凭证（F-70 情报落地 / F-78 DeviceProof 签名）。
//!
//! Trae CN/SOLO 客户端把 OAuth 设备的 EC P-256 密钥对存于
//! `User/globalStorage/storage.json` 的 `iCubeAuthInfo://icube-dc:<deviceId>` 键，
//! 值为项目自研「tc」信封加密（out/vs/base/common/byteCrypto.js 逆向，BlueChonk
//! 交叉验证）：tc 信封的 pepper 是随安装包分发的公开常量表（混淆非加密），
//! 解密属读本机自有凭证，符合零凭证外泄红线——私钥只在内存流转，不落盘不进日志。
//!
//! DeviceProof（ExchangeToken 20405 实测要求）：ECDSA P-256 + SHA-256，签名原文
//! 5 个 `\n` 连接：`POST\n<path>\n<ClientID>\n<AuthCode>\n<Ts>\n<Nonce>`，
//! 字段必须 PascalCase（Signature/Timestamp/Nonce），Timestamp 必须为 JSON int；
//! 签名编码 P1363 r||s 64B（WebCrypto 标准输出）优先，DER 实测被拒保留对照。

use base64::Engine as _;
use sha2::Digest as _;

/// byteCrypto 四常量表（Trae CN resources/app/out/main.js 2026-09-16 实测提取；
/// Woe^Voe=AES 模式 pepper / joe^Hoe=AES_PRIVATE 模式 pepper，经本机真实信封
/// 解密验证通过。上游版本更新表变化时重新提取即可）
const WOE_T: [u8; 64] = [82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130, 155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61, 238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109, 139, 209, 37];
const VOE_T: [u8; 64] = [31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25, 181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200, 235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33, 12, 125];
const JOE_T: [u8; 64] = [191, 192, 216, 250, 122, 246, 220, 97, 31, 254, 98, 27, 8, 72, 71, 176, 135, 99, 96, 18, 127, 101, 203, 104, 211, 102, 191, 125, 37, 72, 150, 156, 51, 229, 121, 35, 17, 153, 141, 177, 110, 131, 150, 128, 172, 255, 254, 6, 18, 140, 55, 62, 236, 249, 135, 64, 135, 12, 117, 4, 89, 149, 168, 209];
const HOE_T: [u8; 64] = [246, 204, 26, 232, 232, 70, 129, 109, 223, 146, 169, 242, 23, 241, 105, 145, 50, 196, 165, 42, 254, 120, 3, 54, 244, 207, 209, 85, 53, 6, 138, 106, 175, 148, 31, 204, 186, 186, 165, 182, 87, 142, 49, 10, 39, 110, 26, 154, 86, 56, 173, 125, 18, 64, 198, 225, 99, 99, 83, 82, 191, 134, 76, 170];

/// tc 信封常量（对齐 byteCrypto：magic [116,99,5,16,0,0]，random 32B，header 6B）
const HEADER: [u8; 6] = [116, 99, 5, 16, 0, 0];
const RANDOM_LEN: usize = 32;
const HEADER_LEN: usize = 6;
const SHA512_LEN: usize = 64;

fn sha512(buf: &[u8]) -> [u8; SHA512_LEN] {
    let mut h = sha2::Sha512::new();
    h.update(buf);
    h.finalize().into()
}

/// 密钥派生（byteCrypto deriveKeys）：SHA512(random) || pepper → SHA512 → 前 32B
/// 切分为 aesKey[0..16] / iv[16..32]
fn derive_keys(random: &[u8], pepper: &[u8; 64]) -> ([u8; 16], [u8; 16]) {
    let o = sha512(random);
    let mut n = [0u8; 128];
    n[..SHA512_LEN].copy_from_slice(&o);
    n[SHA512_LEN..].copy_from_slice(pepper);
    let c = sha512(&n);
    let mut key = [0u8; 16];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&c[..16]);
    iv.copy_from_slice(&c[16..32]);
    (key, iv)
}

/// tc 信封解密（AES 模式；AES_PRIVATE 模式本机数据未见使用，保留支持）
pub fn tc_decrypt(b64: &str, private_mode: bool) -> Result<String, String> {
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("tc 信封 base64 解码失败: {e}"))?;
    if raw.len() < HEADER_LEN + RANDOM_LEN + 16 {
        return Err(format!("tc 信封长度异常: {}", raw.len()));
    }
    if raw[..HEADER_LEN] != HEADER {
        return Err(format!(
            "tc 信封 magic 不匹配: {:02x?}（期望 746305100000）",
            &raw[..HEADER_LEN]
        ));
    }
    let random = &raw[HEADER_LEN..HEADER_LEN + RANDOM_LEN];
    let cipher = &raw[HEADER_LEN + RANDOM_LEN..];
    let pepper = if private_mode {
        let mut p = [0u8; 64];
        for i in 0..64 {
            p[i] = JOE_T[i] ^ HOE_T[i];
        }
        p
    } else {
        let mut p = [0u8; 64];
        for i in 0..64 {
            p[i] = WOE_T[i] ^ VOE_T[i];
        }
        p
    };
    let (key, iv) = derive_keys(random, &pepper);
    let padded = Aes128CbcDec::new((&key).into(), (&iv).into())
        .decrypt_padded_vec_mut::<Pkcs7>(cipher)
        .map_err(|e| format!("AES-128-CBC 解密失败: {e}"))?;
    if padded.len() <= SHA512_LEN {
        return Err("tc 解密后明文过短".into());
    }
    let (tag, body) = padded.split_at(SHA512_LEN);
    if sha512(body)[..] != tag[..] {
        return Err("tc 信封完整性校验失败（SHA512 不匹配）".into());
    }
    String::from_utf8(body.to_vec()).map_err(|e| format!("tc 明文非 UTF-8: {e}"))
}

/// 设备凭证：deviceId（数字串，与 storage.json 键内嵌一致）+ EC P-256 私钥 PEM
/// + machineId（同 storage.json telemetry.machineId，DeviceInfo 构造用）
/// + appVersion（安装目录 package.json version，DeviceInfo.ClientVersion 用）
/// Serialize/Deserialize 供 per-uid synthetic 身份持久化（kv oauth_synthetic_device:{uid}）
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DeviceCredential {
    pub device_id: String,
    pub private_key_pem: String,
    pub machine_id: String,
    pub app_version: String,
    /// 来源客户端（恒 Trae CN，双应用合并后唯一客户端），诊断日志用
    #[allow(dead_code)]
    pub source_app: String,
}

/// 宽容解析持久化的凭证 JSON（kv oauth_synthetic_device:{uid} / :pending；
/// 字段缺失/类型不符/空串返回 None）
pub fn device_credential_from_json(v: &serde_json::Value) -> Option<DeviceCredential> {
    let obj = v.as_object()?;
    let s = |k: &str| -> Option<&str> { obj.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) };
    Some(DeviceCredential {
        device_id: s("device_id")?.to_string(),
        private_key_pem: s("private_key_pem")?.to_string(),
        machine_id: s("machine_id")?.to_string(),
        app_version: obj
            .get("app_version")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        source_app: obj
            .get("source_app")
            .and_then(|x| x.as_str())
            .unwrap_or("synthetic")
            .to_string(),
    })
}

/// 生成合成设备凭证（本机无 Trae 客户端凭证，或全部真实凭证已绑定账号时的
/// 登录身份，issue #110）：随机 EC P-256 密钥对 + 15 位数字 deviceId + 32 hex
/// machineId——与全新安装 IDE 的自生成身份等价（IDE 首启同样本地生成密钥对并经
/// DeviceInfo.DevicePublicKey 自声明，无预注册 API）。私钥由调用方按 uid 持久化
///（服务端自有身份，与 token 同信任域；不进日志不落事件）。
/// 私钥种子走 CSPRNG（random_hex_result→BCryptGenRandom）：不可用上抛 Err，不静默降级
pub fn generate_synthetic_device_credential(app_version: &str) -> Result<DeviceCredential, String> {
    use p256::pkcs8::EncodePrivateKey;

    let seed_hex = crate::commands::oauth::random_hex_result(64)?;
    let mut secret = [0u8; 32];
    for (i, b) in secret.iter_mut().enumerate() {
        *b = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("合成设备随机种子解码失败: {e}"))?;
    }
    let signing = p256::ecdsa::SigningKey::from_bytes(&secret.into())
        .map_err(|e| format!("合成设备密钥生成失败: {e}"))?;
    let pem = signing
        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
        .map_err(|e| format!("合成设备私钥 PEM 编码失败: {e}"))?;
    // deviceId 形态对齐真实客户端：15 位数字串（与 load_or_create_oauth_device
    // 原随机逻辑同口径，仅需唯一性非密码学均匀）
    let device_id: String = (0..15)
        .map(|_| {
            let n = (crate::commands::oauth::random_hex(2).chars().next().unwrap_or('0') as u8)
                .wrapping_rem(10);
            (b'0' + n) as char
        })
        .collect();
    Ok(DeviceCredential {
        device_id,
        private_key_pem: pem.to_string(),
        machine_id: crate::commands::oauth::random_hex(32),
        app_version: app_version.to_string(),
        source_app: "synthetic".to_string(),
    })
}

/// 扫描本机 Trae 客户端（双应用合并后唯一客户端）storage.json 提取设备凭证。
/// 找不到/解密失败返回空表——DeviceProof 不可用时调用方回落无 proof 变体。
pub fn extract_device_credentials() -> Vec<DeviceCredential> {
    let Some(appdata) = std::env::var("APPDATA").ok() else {
        return Vec::new();
    };
    let Some(localappdata) = std::env::var("LOCALAPPDATA").ok() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for app in ["Trae CN"] {
        let path = std::path::PathBuf::from(&appdata)
            .join(app)
            .join("User")
            .join("globalStorage")
            .join("storage.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(obj) = v.as_object() else { continue };
        // DeviceInfo.ClientVersion 用：安装目录 package.json 的 version（真实客户端
        // 上报的是 appVersion，与服务端对设备注册记录的校验相关）
        let app_version = std::fs::read_to_string(
            std::path::PathBuf::from(&localappdata)
                .join("Programs")
                .join(app)
                .join("resources")
                .join("app")
                .join("package.json"),
        )
        .ok()
        .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
        .and_then(|p| p.get("version").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
        let machine_id = obj
            .get("telemetry.machineId")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        for (k, val) in obj {
            let Some(id) = k.strip_prefix("iCubeAuthInfo://icube-dc:") else {
                continue;
            };
            let Some(b64) = val.as_str() else { continue };
            match tc_decrypt(b64, false) {
                Ok(plain) => {
                    let parsed: serde_json::Value = match serde_json::from_str(&plain) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    if let Some(pem) = parsed.get("privateKeyPEM").and_then(|x| x.as_str()) {
                        if pem.contains("BEGIN") {
                            out.push(DeviceCredential {
                                device_id: id.to_string(),
                                private_key_pem: pem.to_string(),
                                machine_id: machine_id.clone(),
                                app_version: app_version.clone(),
                                source_app: app.to_string(),
                            });
                        }
                    }
                }
                Err(_) => continue,
            }
        }
    }
    out
}

/// F-70 余项：storage.json 凭证批量直读的探针结果（账号发现消费）
#[derive(Debug, Clone)]
pub struct CredentialProbe {
    /// 键内嵌的账户中心（dc）uid（纯数字，与 extract 侧同口径）
    pub dc_uid: String,
    /// tc 解密是否成功（解密失败 = 回退既有证据链/抓包路径）
    pub decrypted: bool,
    /// 明文中宽容发现的 JWT 形态字符串（eyJ… 三段式，按出现顺序去重；
    /// 是否可用由调用方 crate::jwt::parse 裁决，本侧不校验签名）
    pub jwt_candidates: Vec<String>,
}

/// 宽容挖掘 JSON 中的 JWT 形态字符串：eyJ 开头 + 恰好两个 '.' 分隔 + 长度 ≥ 100。
/// 只作候选收集，不校验签名与 exp（调用方裁决）。
fn dig_jwt_candidates(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.starts_with("eyJ")
                && t.split('.').count() == 3
                && t.len() >= 100
                && !out.iter().any(|x| x == t)
            {
                out.push(t.to_string());
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|x| dig_jwt_candidates(x, out)),
        serde_json::Value::Object(obj) => obj.values().for_each(|x| dig_jwt_candidates(x, out)),
        _ => {}
    }
}

/// F-70 余项：对 storage.json 的 `iCubeAuthInfo://icube-dc:*` 键做 tc 批量解密探针。
/// 「解密成功优先、失败回退抓包」双路径的直读侧：解密成功且明文含 JWT 形态凭证时，
/// 账号发现可不经 MITM 抓包直接推导 Cloud-IDE uid（JWT data.id 与账号池同体系）；
/// 解密失败/无 JWT 时回退既有证据链（客户端日志 / hybrid / 抓包回填），行为不变。
/// 仅读本机自有凭证；私钥与 JWT 不落盘、不进日志（调用方只用派生结果）。
pub fn probe_credential_entries(storage_json: &serde_json::Value) -> Vec<CredentialProbe> {
    let Some(obj) = storage_json.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, val) in obj {
        let Some(id) = k.strip_prefix("iCubeAuthInfo://icube-dc:") else {
            continue;
        };
        let Some(b64) = val.as_str() else { continue };
        let dc_uid = id.trim().to_string();
        if dc_uid.is_empty() || !dc_uid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let probe = match tc_decrypt(b64, false) {
            Ok(plain) => match serde_json::from_str::<serde_json::Value>(&plain) {
                Ok(parsed) => {
                    let mut jwt_candidates = Vec::new();
                    dig_jwt_candidates(&parsed, &mut jwt_candidates);
                    CredentialProbe {
                        dc_uid,
                        decrypted: true,
                        jwt_candidates,
                    }
                }
                Err(_) => CredentialProbe {
                    dc_uid,
                    decrypted: false,
                    jwt_candidates: Vec::new(),
                },
            },
            Err(_) => CredentialProbe {
                dc_uid,
                decrypted: false,
                jwt_candidates: Vec::new(),
            },
        };
        out.push(probe);
    }
    out
}

/// 从设备私钥推导 SPKI PEM 公钥（DeviceInfo.DevicePublicKey 用，不落盘）
pub fn device_public_key_pem(cred: &DeviceCredential) -> Result<String, String> {
    use p256::pkcs8::{DecodePrivateKey, EncodePublicKey};
    let signing = p256::ecdsa::SigningKey::from_pkcs8_pem(&cred.private_key_pem)
        .map_err(|e| format!("设备私钥解析失败: {e}"))?;
    let verifying = signing.verifying_key();
    let pub_der = verifying
        .to_public_key_der()
        .map_err(|e| format!("公钥 SPKI 编码失败: {e}"))?;
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    let b64 = base64::engine::general_purpose::STANDARD.encode(pub_der.as_bytes());
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----");
    Ok(pem)
}

/// 生成 DeviceProof JSON（PascalCase 字段，20405 实测小写会被拒；
/// Timestamp 必须为 JSON int，字符串会被服务端 schema 拒绝）：
/// 签名原文 = POST\n<path>\n<ClientID>\n<AuthCode>\n<Timestamp>\n<Nonce>
///
/// 签名编码格式（2026-09-16 实测排查）：
/// - P1363：raw r||s 固定 64 字节——WebCrypto（Electron 客户端 JS 侧）标准输出，
///   ring 参照实现（cockpit-tools）同为此格式，为首选
/// - Der：ASN.1 SEQUENCE（~70-72B）——早期逆向结论，实测报 20405 被拒，保留作对照探测
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofSigFormat {
    P1363,
    Der,
}

impl ProofSigFormat {
    pub fn suffix(self) -> &'static str {
        match self {
            ProofSigFormat::P1363 => "/P1363",
            ProofSigFormat::Der => "/DER",
        }
    }
}

pub fn device_proof(
    cred: &DeviceCredential,
    sign_path: &str,
    client_id: &str,
    auth_code: &str,
    format: ProofSigFormat,
) -> Result<serde_json::Value, String> {
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let signing = SigningKey::from_pkcs8_pem(&cred.private_key_pem)
        .map_err(|e| format!("设备私钥解析失败: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 签名 nonce 属安全场景：CSPRNG 不可用上抛 Err（审查修复，不再 panic 中止任务）
    let nonce = crate::commands::oauth::random_hex_result(32)?;
    let msg = format!("POST\n{sign_path}\n{client_id}\n{auth_code}\n{ts}\n{nonce}");
    let sig: Signature = signing.sign(msg.as_bytes());
    let sig_b64 = match format {
        // r||s 固定 64 字节（P1363/WebCrypto/ring 兼容）
        ProofSigFormat::P1363 => base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        ProofSigFormat::Der => base64::engine::general_purpose::STANDARD.encode(sig.to_der()),
    };
    Ok(serde_json::json!({
        "Signature": sig_b64,
        "Timestamp": ts,
        "Nonce": nonce,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表数据完整性：AES 模式 pepper 前六字节 = Woe^Voe（82^31=73, 9^221=212, ...）
    #[test]
    fn pepper_tables_xor_matches_known_head() {
        let expect = [77u8, 212, 194, 230, 184, 49];
        for i in 0..6 {
            assert_eq!(WOE_T[i] ^ VOE_T[i], expect[i]);
        }
    }

    #[test]
    fn tc_decrypt_rejects_bad_magic() {
        let bad = base64::engine::general_purpose::STANDARD.encode([1u8; 64]);
        assert!(tc_decrypt(&bad, false).is_err());
    }

    /// 合成设备生成：deviceId 15 位数字、私钥可解析、machineId 32 hex
    #[test]
    fn generate_synthetic_device_produces_valid_credential() {
        let cred = generate_synthetic_device_credential("9.9.9-test").unwrap();
        assert_eq!(cred.device_id.len(), 15);
        assert!(cred.device_id.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(cred.machine_id.len(), 32);
        assert!(cred.machine_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(cred.private_key_pem.contains("BEGIN PRIVATE KEY"));
        // 私钥可被 DeviceProof 签名链路解析（与真实凭证同一消费口径）
        use p256::pkcs8::DecodePrivateKey;
        p256::ecdsa::SigningKey::from_pkcs8_pem(&cred.private_key_pem)
            .expect("合成私钥必须可解析");
        assert_eq!(cred.source_app, "synthetic");
    }

    /// 持久化往返：serialize → from_json 还原一致；公钥推导可用（刷新链路前置）
    #[test]
    fn synthetic_credential_json_roundtrip() {
        let cred = generate_synthetic_device_credential("9.9.9-test").unwrap();
        let json = serde_json::to_value(&cred).unwrap();
        let parsed = device_credential_from_json(&json).unwrap();
        assert_eq!(parsed.device_id, cred.device_id);
        assert_eq!(parsed.private_key_pem, cred.private_key_pem);
        assert_eq!(parsed.machine_id, cred.machine_id);
        assert!(device_public_key_pem(&parsed).is_ok());
    }

    /// 宽容解析：空对象 / 缺关键字段 / 空串字段均返回 None
    #[test]
    fn device_credential_from_json_rejects_incomplete() {
        assert!(device_credential_from_json(&serde_json::json!({})).is_none());
        assert!(device_credential_from_json(&serde_json::json!({"device_id": "123"})).is_none());
        assert!(device_credential_from_json(&serde_json::json!(
            {"device_id": "", "private_key_pem": "x", "machine_id": "m"}
        ))
        .is_none());
        assert!(device_credential_from_json(&serde_json::json!(null)).is_none());
    }

    /// 构造合成 tc 信封（header + random + AES-CBC(sha512_tag || body)，与真实信封
    /// 同格式），供 F-70 探针测试用——无需真实凭证样本
    fn synthetic_envelope(plaintext: &str) -> String {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        let random = [7u8; RANDOM_LEN];
        let mut pepper = [0u8; 64];
        for i in 0..64 {
            pepper[i] = WOE_T[i] ^ VOE_T[i];
        }
        let (key, iv) = derive_keys(&random, &pepper);
        let body = plaintext.as_bytes();
        let mut padded_input = sha512(body).to_vec();
        padded_input.extend_from_slice(body);
        let ct = Aes128CbcEnc::new((&key).into(), (&iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(&padded_input);
        let mut raw = HEADER.to_vec();
        raw.extend_from_slice(&random);
        raw.extend_from_slice(&ct);
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    /// F-70 探针：解密成功 + 明文 JWT 宽容挖掘；非 icube-dc 键/非数字 uid/解密失败均排除
    #[test]
    fn probe_credential_entries_digs_jwt_from_synthetic_envelope() {
        let jwt = format!("eyJ{}.{}.{}", "e".repeat(57), "f".repeat(40), "g".repeat(60));
        let env = synthetic_envelope(&serde_json::json!({
            "privateKeyPEM": "-----BEGIN PRIVATE KEY-----\nabc\n",
            "accessToken": jwt,
        })
        .to_string());
        let storage = serde_json::json!({
            "iCubeAuthInfo://icube-dc:123456789012345": env,
            "iCubeAuthInfo://icube-dc:not-a-number": "x",
            "someOtherKey": "y",
        });
        let probes = probe_credential_entries(&storage);
        assert_eq!(probes.len(), 1, "仅纯数字 dc uid 键进入探针");
        assert_eq!(probes[0].dc_uid, "123456789012345");
        assert!(probes[0].decrypted);
        assert_eq!(probes[0].jwt_candidates, vec![jwt]);
    }

    /// F-70 探针：解密失败（坏信封）→ decrypted=false、无候选（回退抓包路径的信号）
    #[test]
    fn probe_credential_entries_marks_undecrypted_on_bad_envelope() {
        let storage = serde_json::json!({
            "iCubeAuthInfo://icube-dc:123456789012345":
                base64::engine::general_purpose::STANDARD.encode([9u8; 64]),
        });
        let probes = probe_credential_entries(&storage);
        assert_eq!(probes.len(), 1);
        assert!(!probes[0].decrypted);
        assert!(probes[0].jwt_candidates.is_empty());
    }
}
