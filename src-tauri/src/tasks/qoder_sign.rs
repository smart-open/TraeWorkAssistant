//! Qoder 推理网关（`/algo/...`）COSY 自签名请求层（p3-3）。
//!
//! 移植蓝本：aimod-cc/agent2api（MIT）`cosy.rs`。与 openapi 域的
//! `qoder_common::build_auth_headers`（COSY_CLIENT_TYPE="10"）不同，本模块面向
//! `gateway.qoder.com.cn`（`Cosy-Clienttype`="5"），故命名 `build_cosy_headers` 避免撞车。
//!
//! ── 为什么不能只发 Bearer ─────────────────────────────────
//! Qoder 的推理网关（`/algo/...` 这一族）不认普通 Bearer 令牌，而是一套
//! **自签名**鉴权：
//!   1. 组装用户身份 JSON（uid / security_oauth_token / name / email，固定键序手写）；
//!   2. 生成一把一次性 AES 密钥（16 ASCII 字符），用它把身份 JSON 加密成 `info`；
//!   3. 用内置 RSA 公钥把该 AES 密钥加密成 `Cosy-Key`（服务端用私钥解开）；
//!   4. 把 {version, requestId, info, cosyVersion, ideVersion} 序列化后 base64 得 `payload`；
//!   5. 对 `payload \n key \n 时间戳 \n 请求体 \n 签名路径` 求 MD5，得到签名；
//!   6. 拼成 `Authorization: Bearer COSY.<payload>.<签名>`，并附一组 `Cosy-*` 头（共 19 头）。
//!
//! 签名路径要**去掉 `/algo` 前缀、不含查询串**——服务端按同一规则校验。
//! **请求体参与签名**，所以调用顺序不能颠倒：先 `encode_body` 再调本函数
//!（顺序颠倒会得到「Signature invalid」且无法从错误里看出原因）。
//!
//! ── 依赖取舍 ──────────────────────────────────────────────
//! - AES-CBC：复用已有 `cbc` crate（`Encryptor<Aes128>` + `Pkcs7`），不手写、不升级 aes；
//! - RSA：只需「按 PKCS#1 v1.5 打包后做一次模幂」，指数固定 65537（`modpow` 约 17 次
//!   平方），`num-bigint` 一个依赖就够——引入完整 `rsa` crate 会拖进
//!   num-bigint-dig / pkcs1 / pkcs8 / der 等一长串依赖；
//! - 随机源：全部从 uuid v4 派生（uuid 已在依赖树，避免再引 getrandom 双版本）——
//!   AES 密钥取 simple 格式前 16 hex，RSA PS 填充用多个 uuid 字节拼接、抽到 0 映射 1。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 错误约定与 qoder_common 一致：`Result<_, String>`；release 是 `panic=abort`：
//! 本文件零 unwrap/expect/panic（`json_text` 的 `unwrap_or_else` 只在分配失败时
//! 兜底为空引号，不会 panic；测试代码不受此约束）。

use base64::Engine as _;
use cbc::cipher::generic_array::GenericArray;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use md5::{Digest, Md5};
use num_bigint::BigUint;

/// 推理网关（`/algo/...`）使用的 COSY 协议版本（蓝本对拍值，探针联调确认）
pub const GATEWAY_COSY_VERSION: &str = "1.1.38";
/// COSY 网关客户端类型（桌面 CLI 形态；与 openapi 域的 COSY_CLIENT_TYPE="10" 不同）
pub const CLIENT_TYPE: &str = "5";
/// 数据策略：不同意用于训练
pub const DATA_POLICY: &str = "disagree";
/// 登录版本标识
pub const LOGIN_VERSION: &str = "v2";

/// 网关用它区分客户端形态（COSY 头 `Cosy-Machinetype`）
const MACHINE_TYPE: &str = "5";
/// `Cosy-Clientip`：本机回环（源实现写死该值）
const CLIENT_IP: &str = "127.0.0.1";

/// COSY 身份加密用的 RSA 公钥模数（1024 位，hex，无前导零）。
///
/// 这是**客户端内置的固定公钥**（源实现 `RSA_PUBLIC_KEY` 的 PEM 正文），
/// 只用于加密一次性 AES 密钥，不是机密材料。指数固定 65537。
///
/// 导出方式：对 PEM 正文做 base64 解码 → DER 里 `02 81 81` 之后的 128 字节即模数，
/// 紧随其后的 `02 03 01 00 01` 即指数。
const RSA_MODULUS_HEX: &str = "c0f22307e5cd362e296bb04470f6de8fbf935ce24e8fcf511a0e2701329769c4a76e499bb938036a52af1eaf818cf79a2600620e3ce87e371d2ca6d85803606a1b3fa5e874643c9ed2db7e85673ef7227fca56e2e7c08f0927609bb896a9f24be1782099a66016a5bfdc3f1ff756bfc9e88d7b5dc5be30bf45a0223a00ebcecf";

/// RSA 公钥指数（源 PEM 里的 `02 03 01 00 01`）
const RSA_EXPONENT: u32 = 65537;

/// 自定义替换字母表（与标准 base64 字母表逐字符对应，`encode_body` 第三步用）
const CUSTOM_ALPHABET: &[u8] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
/// 标准 base64 字母表
const STD_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 一次签名所需的身份素材（对应源实现 `buildAuthHeaders` 的 creds 参数）。
pub struct CosyIdentity<'a> {
    /// 账号在 Qoder 侧的 uid
    pub user_id: &'a str,
    /// access token（身份 JSON 里的 `security_oauth_token`）
    pub auth_token: &'a str,
    /// 昵称（可为空）
    pub name: &'a str,
    /// 邮箱（可为空）
    pub email: &'a str,
    /// 机器标识（COSY 头里要带，且参与「同一账号不同设备」的判定）
    pub machine_id: &'a str,
}

/// 构造一次请求所需的全部 COSY 头（19 头）。
///
/// `body` 是**已编码**的请求体（GET 类请求传 `None`）。签名覆盖它，
/// 所以调用方必须先 `encode_body` 再调本函数。
pub fn build_cosy_headers(
    body: Option<&[u8]>,
    request_url: &str,
    identity: &CosyIdentity<'_>,
) -> Result<Vec<(String, String)>, String> {
    if identity.user_id.is_empty() {
        return Err("Qoder 账号缺少用户标识，无法签名".to_string());
    }
    if identity.auth_token.is_empty() {
        return Err("Qoder 账号缺少访问令牌，请重新登录".to_string());
    }

    // 一次性 AES 密钥：16 个 ASCII 字符（源实现取 UUID 去连字符后前 16 位，同语义）
    let aes_key = random_aes_key16();

    // 身份 JSON：键顺序照抄源实现（uid / security_oauth_token / name / aid / email）。
    // ── 为什么手写而不是 `json!` ── 本项目 serde_json 未开 preserve_order，
    // `Value::Object` 是按键排序的 BTreeMap：`json!` 出来的键序是
    // aid / email / name / security_oauth_token / uid。这段 JSON 会被 AES 加密进
    // info、再 base64 进 payload，整条字节串参与签名——与参考实现逐字节一致是
    // 一条能靠对拍证伪的硬保证；键序漂移会让这条保证消失。各字段用 to_string
    // 单独转义，拼出来仍是合法 JSON（值里含引号/中文时也不会拼坏）。
    let user_info = identity_json(identity);
    let info = aes128_cbc_base64(&aes_key, user_info.as_bytes())?;
    let cosy_key = rsa_encrypt_base64(&aes_key)?;

    let request_id = uuid::Uuid::new_v4().to_string();
    let timestamp = now_secs().to_string();

    // payload 同样按源实现的键序手写：version / requestId / info / cosyVersion /
    // ideVersion。info 是 base64、requestId 是 UUID、版本是常量，都不需要转义。
    let payload_src = format!(
        "{{\"version\":\"v1\",\"requestId\":\"{request_id}\",\"info\":\"{info}\",\
         \"cosyVersion\":\"{GATEWAY_COSY_VERSION}\",\"ideVersion\":\"\"}}"
    );
    let payload = base64::engine::general_purpose::STANDARD.encode(payload_src.as_bytes());

    let sig_path = signature_path(request_url)?;
    let body_bytes = body.unwrap_or(&[]);

    // MD5(payload \n key \n 时间戳 \n 请求体 \n 签名路径)
    let mut hasher = Md5::new();
    hasher.update(payload.as_bytes());
    hasher.update(b"\n");
    hasher.update(cosy_key.as_bytes());
    hasher.update(b"\n");
    hasher.update(timestamp.as_bytes());
    hasher.update(b"\n");
    hasher.update(body_bytes);
    hasher.update(b"\n");
    hasher.update(sig_path.as_bytes());
    let signature = format!("{:x}", hasher.finalize());

    let mut body_hasher = Md5::new();
    body_hasher.update(body_bytes);
    let body_hash = format!("{:x}", body_hasher.finalize());

    let headers: Vec<(String, String)> = vec![
        (
            "Authorization".to_string(),
            format!("Bearer COSY.{payload}.{signature}"),
        ),
        ("Cosy-Key".to_string(), cosy_key),
        ("Cosy-User".to_string(), identity.user_id.to_string()),
        ("Cosy-Date".to_string(), timestamp),
        ("Cosy-Version".to_string(), GATEWAY_COSY_VERSION.to_string()),
        ("Cosy-Machineid".to_string(), identity.machine_id.to_string()),
        ("Cosy-Machinetoken".to_string(), identity.machine_id.to_string()),
        ("Cosy-Machinetype".to_string(), MACHINE_TYPE.to_string()),
        ("Cosy-Machineos".to_string(), machine_os()),
        ("Cosy-Clienttype".to_string(), CLIENT_TYPE.to_string()),
        ("Cosy-Clientip".to_string(), CLIENT_IP.to_string()),
        ("Cosy-Bodyhash".to_string(), body_hash),
        ("Cosy-Bodylength".to_string(), body_bytes.len().to_string()),
        ("Cosy-Sigpath".to_string(), sig_path),
        ("Cosy-Data-Policy".to_string(), DATA_POLICY.to_string()),
        ("Cosy-Organization-Id".to_string(), String::new()),
        ("Cosy-Organization-Tags".to_string(), String::new()),
        ("Login-Version".to_string(), LOGIN_VERSION.to_string()),
        ("X-Request-Id".to_string(), request_id),
    ];
    Ok(headers)
}

/// 请求体编码（源实现 `encodeBody`）：上游带 `Encode=1` 时请求体不是裸 JSON。
///
/// 三步都是**纯字节变换**，服务端按同一规则还原：
///   1. 对 JSON 字节做标准 base64，得到一段文本；
///   2. 把该文本按「尾段 / 中段 / 首段」重排（各段长度都是 ⌊n/3⌋，
///      余数留在中段，所以三段拼回来仍是原长）；
///   3. 逐字符做一次字母表替换（含把 `=` 换成 `$`）。
///
/// 签名算在**编码之后**的字节上（见 [`build_cosy_headers`]）。
pub fn encode_body(payload: &[u8]) -> Vec<u8> {
    let standard = base64::engine::general_purpose::STANDARD.encode(payload);
    let bytes = standard.as_bytes();
    let length = bytes.len();
    let third = length / 3;

    let mut table = [0u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    for (index, ch) in STD_ALPHABET.iter().enumerate() {
        table[*ch as usize] = CUSTOM_ALPHABET[index];
    }
    // base64 的填充符也要换掉
    table[b'=' as usize] = b'$';

    let mut out = Vec::with_capacity(length);
    // 尾段 → 中段 → 首段
    for index in (length - third)..length {
        out.push(table[bytes[index] as usize]);
    }
    for index in third..(length - third) {
        out.push(table[bytes[index] as usize]);
    }
    for index in 0..third {
        out.push(table[bytes[index] as usize]);
    }
    out
}

/// 身份 JSON 固定键序手写（`uid / security_oauth_token / name / aid / email`）。
///
/// 独立成函数便于单测断言精确串；转义见 [`build_cosy_headers`] 内注释。
fn identity_json(identity: &CosyIdentity<'_>) -> String {
    format!(
        "{{\"uid\":{},\"security_oauth_token\":{},\"name\":{},\"aid\":\"\",\"email\":{}}}",
        json_text(identity.user_id),
        json_text(identity.auth_token),
        json_text(identity.name),
        json_text(identity.email),
    )
}

/// 一个字符串的 JSON 文本形态（带引号与转义）。
///
/// `serde_json::to_string` 对 `&str` 就是把该串转义成 JSON 字符串，
/// 失败只可能是分配失败——那时给一对空引号，签名会失败但不会 panic。
fn json_text(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// 参与签名的路径：去掉 `/algo` 前缀、不含查询串（源实现 `sigPathOf`）。
pub fn signature_path(request_url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(request_url).map_err(|_| "Qoder 请求地址无效".to_string())?;
    let path = parsed.path();
    Ok(path
        .strip_prefix("/algo")
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string()))
}

/// 机器操作系统标识（源实现 `machineOs`）：`{arch}_{platform}` 形态。
fn machine_os() -> String {
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let platform = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    format!("{arch}_{platform}")
}

/// 当前 Unix 时间（秒）。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一次性 AES 密钥：uuid v4 simple（32 hex 字符）取前 16 ASCII 字符。
fn random_aes_key16() -> String {
    uuid::Uuid::new_v4().simple().to_string().chars().take(16).collect()
}

/// 用 uuid v4 字节流填满 `out`（随机源派生：uuid 已在依赖树，不再引 getrandom）。
fn random_fill(out: &mut [u8]) {
    let mut filled = 0usize;
    while filled < out.len() {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let take = bytes.len().min(out.len() - filled);
        out[filled..filled + take].copy_from_slice(&bytes[..take]);
        filled += take;
    }
}

/// AES-128-CBC + PKCS#7（`cbc` crate），返回 base64 密文。
///
/// 密钥与 IV **都取同一段 16 字节**（源实现 `aesEncrypt` 的既有做法，
/// 不是这里的设计选择），所以只需给一个 16 字符的 key。
fn aes128_cbc_base64(key16: &str, plaintext: &[u8]) -> Result<String, String> {
    if key16.len() != 16 {
        return Err("Qoder 签名密钥长度无效".to_string());
    }
    let key = GenericArray::from_slice(key16.as_bytes());
    let iv = GenericArray::from_slice(key16.as_bytes());
    let ciphertext = cbc::Encryptor::<aes::Aes128>::new(key, iv)
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext);
    Ok(base64::engine::general_purpose::STANDARD.encode(&ciphertext))
}

/// RSA 公钥加密（PKCS#1 v1.5），返回 base64 密文。
///
/// 布局：`00 || 02 || PS || 00 || M`，PS 是**非零随机**字节
///（源实现走 Node 的 `RSA_PKCS1_PADDING`，OpenSSL 用随机 PS）。
/// PS 固定会让 RSA 退化成确定性加密——那是可被指纹化的特征，不要改。
fn rsa_encrypt_base64(plaintext: &str) -> Result<String, String> {
    let modulus = BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16)
        .ok_or_else(|| "Qoder 签名公钥无效".to_string())?;
    let key_len = ((modulus.bits() + 7) / 8) as usize;
    let message = plaintext.as_bytes();
    if message.len() + 11 > key_len {
        return Err("Qoder 签名内容过长".to_string());
    }

    let encoded = pkcs1_v15_encode(message, key_len);
    let cipher = BigUint::from_bytes_be(&encoded).modpow(&BigUint::from(RSA_EXPONENT), &modulus);
    // 定长输出（模数 1024 位 → 128 字节），不足时左侧补零
    let raw = cipher.to_bytes_be();
    if raw.len() > key_len {
        return Err("Qoder 签名结果长度异常".to_string());
    }
    let mut out = vec![0u8; key_len];
    out[key_len - raw.len()..].copy_from_slice(&raw);
    Ok(base64::engine::general_purpose::STANDARD.encode(&out))
}

/// PKCS#1 v1.5 编码（加密前的消息结构，独立成纯函数便于单测）：
/// `00 || 02 || PS(非零随机) || 00 || M`。
fn pkcs1_v15_encode(message: &[u8], key_len: usize) -> Vec<u8> {
    let ps_len = key_len - 3 - message.len();
    let mut ps = vec![0u8; ps_len];
    random_fill(&mut ps);
    // PKCS#1 v1.5 要求 PS 全为非零字节：把抽到的 0 映射成 1
    for byte in ps.iter_mut() {
        if *byte == 0 {
            *byte = 1;
        }
    }
    let mut encoded = Vec::with_capacity(key_len);
    encoded.push(0x00);
    encoded.push(0x02);
    encoded.extend_from_slice(&ps);
    encoded.push(0x00);
    encoded.extend_from_slice(message);
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// encode_body 三段轮转 + 字母替换的手算样例：
    /// b"hi" → 标准 base64 "aGk="（4 字符，third=1）→ 尾/中/首重排 "=Gka"
    /// → 替换：'='→'$'，'G'=STD[6]→CUSTOM[6]='H'，'k'=STD[36]→CUSTOM[36]='z'，
    /// 'a'=STD[26]→CUSTOM[26]='P'（小写 a 在标准表第 26 位）。
    #[test]
    fn encode_body_rotates_and_remaps() {
        assert_eq!(encode_body(b"hi"), b"$HzP");
    }

    /// 空体（GET 类请求的 body=None 形态）应返回空输出（third=0 三段循环全空，无下溢）
    #[test]
    fn encode_body_empty() {
        assert!(encode_body(b"").is_empty());
    }

    /// 逆变换能还原标准 base64 并解回原文（验证三段轮转 + 替换表自洽）
    #[test]
    fn encode_body_roundtrip() {
        // 字节串字面量只能 ASCII；中文场景由 aes_cbc_roundtrip 覆盖
        let payload =
            b"{\"messages\":[{\"role\":\"user\",\"content\":\"nihao world!@#\"}]}";
        let encoded = encode_body(payload);
        // 三段轮转不改变长度
        let standard_len = base64::engine::general_purpose::STANDARD.encode(payload).len();
        assert_eq!(encoded.len(), standard_len);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(inverse_encode(&encoded))
            .unwrap_or_default();
        assert_eq!(decoded, payload.to_vec());
    }

    /// encode_body 的逆变换（仅测试用）：替换回标准表 + 按 首/中/尾 还原三段
    fn inverse_encode(encoded: &[u8]) -> Vec<u8> {
        let mut table = [0u8; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            *slot = index as u8;
        }
        for (index, ch) in CUSTOM_ALPHABET.iter().enumerate() {
            table[*ch as usize] = STD_ALPHABET[index];
        }
        table[b'$' as usize] = b'=';
        let remapped: Vec<u8> = encoded.iter().map(|b| table[*b as usize]).collect();
        let len = remapped.len();
        let third = len / 3;
        let m_len = len - 2 * third;
        // out 布局是 [尾段, 中段， 首段]，还原回 [首段, 中段， 尾段]
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&remapped[len - third..]);
        out.extend_from_slice(&remapped[third..third + m_len]);
        out.extend_from_slice(&remapped[..third]);
        out
    }

    #[test]
    fn signature_path_strips_algo_and_query() {
        assert_eq!(
            signature_path("https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1")
                .unwrap_or_default(),
            "/api/v2/model/list"
        );
        assert_eq!(
            signature_path(
                "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"
            )
            .unwrap_or_default(),
            "/api/v2/service/pro/sse/agent_chat_generation"
        );
        // 无 /algo 前缀原样返回
        assert_eq!(
            signature_path("https://gateway.qoder.com.cn/api/other").unwrap_or_default(),
            "/api/other"
        );
        assert!(signature_path("not a url").is_err());
    }

    #[test]
    fn identity_json_has_source_key_order() {
        let id = CosyIdentity {
            user_id: "u1",
            auth_token: "tk\"x",
            name: "昵称",
            email: "a@b.c",
            machine_id: "m",
        };
        assert_eq!(
            identity_json(&id),
            "{\"uid\":\"u1\",\"security_oauth_token\":\"tk\\\"x\",\"name\":\"昵称\",\"aid\":\"\",\"email\":\"a@b.c\"}"
        );
    }

    #[test]
    fn build_cosy_headers_produces_all_19() {
        let id = CosyIdentity {
            user_id: "10001",
            auth_token: "tok",
            name: "n",
            email: "e@x.com",
            machine_id: "mid",
        };
        let url = "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1";
        let headers = build_cosy_headers(Some(b"body"), url, &id).unwrap_or_default();
        assert_eq!(headers.len(), 19);
        let get = |name: &str| {
            headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        let auth = get("Authorization").unwrap_or_default();
        assert!(auth.starts_with("Bearer COSY."));
        // payload 段可 base64 解出固定键序信封
        let parts: Vec<&str> = auth.splitn(3, '.').collect();
        assert_eq!(parts.len(), 3);
        let payload = String::from_utf8(
            base64::engine::general_purpose::STANDARD.decode(parts[1]).unwrap_or_default(),
        )
        .unwrap_or_default();
        assert!(payload.starts_with("{\"version\":\"v1\",\"requestId\":\""));
        assert!(payload.contains("\"cosyVersion\":\"1.1.38\""));
        assert!(payload.ends_with("\"ideVersion\":\"\"}"));

        assert_eq!(get("Cosy-User"), Some("10001"));
        assert_eq!(get("Cosy-Sigpath"), Some("/api/v2/model/list"));
        assert_eq!(get("Cosy-Bodylength"), Some("4"));
        assert!(!get("Cosy-Bodyhash").unwrap_or_default().is_empty());
        assert_eq!(get("Cosy-Clienttype"), Some("5"));
        assert_eq!(get("Cosy-Version"), Some("1.1.38"));
        assert_eq!(get("Cosy-Data-Policy"), Some("disagree"));
        assert_eq!(get("Login-Version"), Some("v2"));
        assert_eq!(get("Cosy-Machinetoken"), Some("mid"));
        // machine_os 随编译平台变化（{arch}_{platform}），断言与生产逻辑同源——
        // arch 段同样必须随 target_arch 分支：CI arm64 runner 上测试二进制按
        // aarch64 编译，machine_os() 返回 aarch64_darwin（原断言硬编码 x86_64_darwin，
        // 致 aarch64/universal job 的 cargo test 恒挂、x64 job 恒过的架构偏差，run #21）
        let expect_arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
        let expect_platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        let expect_os = format!("{expect_arch}_{expect_platform}");
        assert_eq!(get("Cosy-Machineos"), Some(expect_os.as_str()));
        assert!(!get("X-Request-Id").unwrap_or_default().is_empty());
        // Cosy-Key 是 128 字节 RSA 密文的 base64
        let key = base64::engine::general_purpose::STANDARD
            .decode(get("Cosy-Key").unwrap_or_default())
            .unwrap_or_default();
        assert_eq!(key.len(), 128);
        // 签名是 32 位 hex MD5
        let sig = get("Authorization").unwrap_or_default().rsplit('.').next().unwrap_or("");
        assert_eq!(sig.len(), 32);
        assert!(sig.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn build_cosy_headers_rejects_missing_identity() {
        let empty_user = CosyIdentity {
            user_id: "",
            auth_token: "tok",
            name: "",
            email: "",
            machine_id: "m",
        };
        assert!(build_cosy_headers(None, "https://gateway.qoder.com.cn/algo/x", &empty_user).is_err());
        let empty_token = CosyIdentity {
            user_id: "u",
            auth_token: "",
            name: "",
            email: "",
            machine_id: "m",
        };
        assert!(build_cosy_headers(None, "https://gateway.qoder.com.cn/algo/x", &empty_token).is_err());
    }

    /// AES-CBC 密文可被同 key/iv 解回原文（PKCS7 对齐）
    #[test]
    fn aes_cbc_roundtrip() {
        let key = "0123456789abcdef";
        let msg = b"{\"uid\":\"u\",\"security_oauth_token\":\"t\"}";
        let ct = aes128_cbc_base64(key, msg).unwrap_or_default();
        let raw = base64::engine::general_purpose::STANDARD.decode(&ct).unwrap_or_default();
        assert_eq!(raw.len() % 16, 0);
        let pad = 16 - (msg.len() % 16);
        assert_eq!(raw.len(), msg.len() + pad);

        use cbc::cipher::{BlockDecryptMut, KeyIvInit as _};
        type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
        let key_arr = GenericArray::from_slice(key.as_bytes());
        let iv = GenericArray::from_slice(key.as_bytes());
        let pt = Aes128CbcDec::new(key_arr, iv)
            .decrypt_padded_vec_mut::<Pkcs7>(&raw)
            .unwrap_or_default();
        assert_eq!(pt, msg.to_vec());
    }

    /// PKCS#1 v1.5 编码结构正确（头两字节 00 02、PS 非零且 ≥8 字节、消息在尾）。
    ///
    /// 结构断言必须做在**加密前的编码消息**上：模幂后的密文字节是随机的
    ///（这正是 RSA 的语义），对密文断言结构必然偶发失败。
    #[test]
    fn pkcs1_encoded_shape() {
        let encoded = pkcs1_v15_encode(b"0123456789abcdef", 128);
        assert_eq!(encoded.len(), 128);
        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], 0x02);
        // 找到 PS 结束的 0x00 分隔符
        let sep = encoded.iter().skip(2).position(|b| *b == 0).unwrap_or(0) + 2;
        assert!(sep >= 2 + 8, "PKCS#1 v1.5 要求 PS 至少 8 字节");
        assert!(encoded[2..sep].iter().all(|b| *b != 0));
        assert_eq!(&encoded[sep + 1..], b"0123456789abcdef");
    }

    /// RSA 密文：定长 128 字节（模幂后字节随机，非确定性由 random_sources_differ 覆盖）
    #[test]
    fn rsa_output_shape() {
        let ct = rsa_encrypt_base64("0123456789abcdef").unwrap_or_default();
        let raw = base64::engine::general_purpose::STANDARD.decode(&ct).unwrap_or_default();
        assert_eq!(raw.len(), 128);
    }

    /// 随机源性质：两次 AES 密钥互异、PS 每次不同（RSA 密文非确定）
    #[test]
    fn random_sources_differ() {
        assert_ne!(random_aes_key16(), random_aes_key16());
        let ct1 = rsa_encrypt_base64("same").unwrap_or_default();
        let ct2 = rsa_encrypt_base64("same").unwrap_or_default();
        assert_ne!(ct1, ct2, "PS 随机时应为非确定性加密");
    }

    /// 联调探针（`cargo test probe_dump -- --ignored --nocapture`）：
    /// 打印一组真实形态的 COSY 头与编码体，供与 agent2api 蓝本 / 抓包样本
    /// 逐头对拍（随机源每次不同，比对结构与可解码性）。
    #[test]
    #[ignore]
    fn probe_dump_cosy_headers() {
        let id = CosyIdentity {
            user_id: "10001",
            auth_token: "probe-token",
            name: "probe",
            email: "",
            machine_id: "probe-machine",
        };
        let body = encode_body(b"{\"probe\":true}");
        let headers = build_cosy_headers(
            Some(&body),
            "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1",
            &id,
        )
        .unwrap_or_default();
        println!("body(encoded): {}", String::from_utf8_lossy(&body));
        for (k, v) in &headers {
            println!("{k}: {v}");
        }
    }
}
