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

/// 跨平台安全随机 hex（原 commands/oauth.rs random_hex：Windows BCrypt 实现，
/// core 侧改 rand OsRng，语义等价——用于 DeviceProof nonce 等安全场景）
pub(crate) fn random_hex(len: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; len.div_ceil(2)];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex.truncate(len);
    hex
}

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
#[derive(Clone, Debug)]
pub struct DeviceCredential {
    pub device_id: String,
    pub private_key_pem: String,
    pub machine_id: String,
    pub app_version: String,
    /// 来源客户端（Trae CN / TRAE SOLO CN 等），诊断日志用
    #[allow(dead_code)]
    pub source_app: String,
}

/// 扫描本机各 Trae 客户端 storage.json 提取设备凭证（多个客户端各有一套）。
/// 找不到/解密失败返回空表——DeviceProof 不可用时调用方回落无 proof 变体。
pub fn extract_device_credentials() -> Vec<DeviceCredential> {
    let Some(appdata) = std::env::var("APPDATA").ok() else {
        return Vec::new();
    };
    let Some(localappdata) = std::env::var("LOCALAPPDATA").ok() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for app in ["Trae CN", "TRAE SOLO CN", "Trae", "Trae Work"] {
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
    let nonce = random_hex(32);
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
}
