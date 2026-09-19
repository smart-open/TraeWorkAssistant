//! CA / 叶子证书管理（原 device_proxy.py ensure_ca / leaf_cert 的 Rust 版）。
//!
//! 兼容性红线：老版本 Python 生成的 CA（RSA 2048、CN=TraeDeviceProxyCA、PKCS#1 私钥）
//! 必须原样加载——用户已将其安装进 Windows 受信任根存储，换 CA 等于强制所有用户重装证书。
//! 注意：rcgen 0.14 默认 ring 后端的 KeyPair 只接受 PKCS#8；必须启用 aws_lc_rs feature
//! （Cargo.toml）其 TryFrom 才支持 PKCS#1/SEC1，否则 from_pem 报「Could not parse key pair」。
//! 另一陷阱：rcgen 对非 PKCS#8 加载的密钥 `serialize_der` **原样返回原格式 DER**
//! （PKCS#1 进 PKCS#1 出），不能硬包 PrivatePkcs8KeyDer——必须按原始格式传给
//! rustls（PrivateKeyDer 按 variant 分派解析），否则 ServerConfig 构建报
//! 「failed to parse private key as RSA, ECDSA, or EdDSA」。
//! 新生成的 CA 使用 rcgen 默认 ECDSA P-256（根证书算法不影响链校验）。
//!
//! 叶子证书不再像 Python 版那样写临时文件：rcgen 在内存内签名 + LRU 缓存 ServerConfig，
//! 私钥永不落盘（Python 版的 atexit 清理 / 残留清扫随之简化为「启动清扫一次历史残留」）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use time::{Duration, OffsetDateTime};
use tokio_rustls::rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use tokio_rustls::rustls::pki_types::pem::PemObject;

/// 叶子证书 ServerConfig 缓存上限（Python 版为 50，内存内缓存无临时文件可放宽）
const LEAF_CACHE_MAX: usize = 512;

/// 证书序列号：纳秒时间戳 + 原子计数器（只需进程内唯一，无需密码学随机）
static SERIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_serial() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (SERIAL_COUNTER.fetch_add(1, Ordering::Relaxed) << 24)
}

/// MITM 证书颁发机构：持有 CA 签发器，按域名在内存内签发叶子证书并缓存 ServerConfig。
pub struct CaAuthority {
    issuer: Issuer<'static, KeyPair>,
    /// 叶子证书独立密钥（RSA-2048，启动时生成一次、全部叶子复用，与 CA 密钥严格
    /// 分离——对齐 Python 版「每叶子独立密钥」结构）。差异修复 2026-09-15：此前
    /// 叶子复用 CA 密钥（SPKI == 签发 CA SPKI 的畸形结构），豆包客户端 ttnet/cronet
    /// 原生栈（BoringSSL 定制校验）一律拒之门外（握手中止 os error 10053 → 页面
    /// 空白）；Python 版独立密钥叶子同 CA 同域被正常接受（leaf_*.key 文件为证）
    leaf_key: KeyPair,
    provider: Arc<CryptoProvider>,
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl CaAuthority {
    /// 按域名生成（或取缓存）TLS 服务端配置。叶子证书 CN/SAN=域名，有效期 10 年。
    pub fn gen_server_config(&self, host: &str) -> Arc<ServerConfig> {
        if let Some(cfg) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(host) {
            return Arc::clone(cfg);
        }
        let cfg = Arc::new(self.build_server_config(host));
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= LEAF_CACHE_MAX {
            cache.clear();
        }
        cache.insert(host.to_string(), Arc::clone(&cfg));
        cfg
    }

    fn build_server_config(&self, host: &str) -> ServerConfig {
        let not_before = OffsetDateTime::now_utc() - Duration::days(1);
        // rcgen 0.14：new() 接受字符串 SAN，内部自动识别 IP 地址
        let mut params = CertificateParams::new(vec![host.to_string()])
            .expect("leaf certificate params");
        params.distinguished_name.push(DnType::CommonName, host);
        params.not_before = not_before;
        params.not_after = not_before + Duration::days(3650);
        params.is_ca = IsCa::NoCa;
        params.serial_number = Some(next_serial().into());
        // AKI：OpenSSL 3.2+ 严格校验非自签证书必须带 Authority Key Identifier
        params.use_authority_key_identifier_extension = true;

        let cert = params
            .signed_by(&self.leaf_key, &self.issuer)
            .expect("failed to sign leaf certificate");

        let cfg = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert)],
                PrivatePkcs8KeyDer::from(self.leaf_key.serialize_der()).into(),
            )
            .expect("failed to build server config");
        // 不广播 ALPN（对齐 Python 版 ssl.SSLContext 默认行为——ServerHello 不带
        // ALPN 扩展，客户端一律回落 HTTP/1.1）。差异修复 2026-09-15：此前显式广播
        // ALPN=[http/1.1]，对只提供 h2 的客户端（豆包 cronet/ttnet 原生栈对其 API
        // 域常见）服务端选不出共同协议直接握手告警中止（实测 proxy.log 大量
        // 「TLS 握手失败 os error 10053」，Python 版同域无此现象）。
        cfg
    }
}

/// 查询 Windows 受信任根存储是否含本代理 CA（TraeDeviceProxyCA）。
/// HKLM 与 HKCU Root 任一命中即视为已安装（与 commands::cert::cert_status 同语义）；
/// 查询失败一律视为未安装。供代理启动日志输出真实安装状态（此前为无条件提示，误导）。
/// 跨平台统一入口是 platform::cert_ctl::cert_query（mac 走 security find-certificate），
/// 本函数仅保留 Windows 实现。
#[cfg(target_os = "windows")]
pub fn installed_in_windows_root() -> bool {
    let run = |args: &[&str]| -> bool {
        crate::platform::cmd::sys_command("certutil")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("TraeDeviceProxyCA"))
            .unwrap_or(false)
    };
    run(&["-store", "Root"]) || run(&["-user", "-store", "Root"])
}

/// 确保数据目录下存在可用 CA：已有则加载（兼容 Python 版 RSA CA），缺失则生成并落盘。
/// 文件布局与 Python 版完全一致：data/certs/{ca.crt, ca.key, ca.cer}（ca.cer 为 DER，
/// 供 certutil 安装；cert_status 靠 CN 字符串 TraeDeviceProxyCA 匹配，不可改名）。
/// 返回供代理使用的签发器；cert_install 只需文件存在性，可忽略返回值。
pub fn ensure_ca(certs_dir: &std::path::Path) -> Result<CaAuthority, String> {
    sweep_legacy_leaf_files(certs_dir);
    let cert_pem_path = certs_dir.join("ca.crt");
    let key_pem_path = certs_dir.join("ca.key");
    let cer_der_path = certs_dir.join("ca.cer");

    std::fs::create_dir_all(certs_dir).map_err(|e| format!("创建证书目录失败: {e}"))?;

    let issuer = if cert_pem_path.exists() && key_pem_path.exists() {
        // 读取前 ACL 自愈（issue #14）：历史版本 harden_ca_dir 的 grant 无 (OI)(CI)
        // 继承标志，目录收紧时已有子文件的继承 ACE 被动态清空成空 DACL——任何进程
        // （含提权 certutil、Windows 证书 UI）都读不了文件。此前的自愈挂在 certutil
        // 失败之后，读取阶段就报「读取 CA 证书失败: 拒绝访问 (os error 5)」走不到那里
        self_heal_acl_if_needed(certs_dir, &[&cert_pem_path, &key_pem_path])?;
        let cert_pem = std::fs::read_to_string(&cert_pem_path)
            .map_err(|e| format!("读取 CA 证书失败: {e}"))?;
        let key_pem = std::fs::read_to_string(&key_pem_path)
            .map_err(|e| format!("读取 CA 私钥失败: {e}"))?;
        let issuer = load_issuer(&cert_pem, &key_pem)?;
        // ca.cer 缺失则从 ca.crt(PEM) 补导出 DER：老版本/异常过程可能只留下
        // ca.crt+ca.key，certutil 安装依赖 ca.cer，缺失会在 UAC 后立即失败（闪退）
        if !cer_der_path.exists() {
            let der = pem_to_der(&cert_pem)?;
            std::fs::write(&cer_der_path, der).map_err(|e| format!("补写 ca.cer 失败: {e}"))?;
        }
        issuer
    } else {
        let generated = generate_ca()?;
        // 先落盘再使用：ca.cer(DER) 供 certutil 安装，ca.crt/ca.key 供下次启动加载
        std::fs::write(&cert_pem_path, generated.cert_pem.as_bytes())
            .map_err(|e| format!("写入 ca.crt 失败: {e}"))?;
        std::fs::write(&key_pem_path, generated.key_pem.as_bytes())
            .map_err(|e| format!("写入 ca.key 失败: {e}"))?;
        std::fs::write(&cer_der_path, generated.cert_der)
            .map_err(|e| format!("写入 ca.cer 失败: {e}"))?;
        harden_ca_dir(certs_dir);
        generated.issuer
    };

    // 叶子独立密钥：RSA-2048（与 Python 版叶子同算法），启动时生成一次（约百毫秒）
    let leaf_key = KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
        .map_err(|e| format!("生成叶子证书密钥失败: {e}"))?;

    let provider = Arc::new(load_crypto_provider());
    Ok(CaAuthority { issuer, leaf_key, provider, cache: Mutex::new(HashMap::new()) })
}

struct GeneratedCa {
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
    key_pem: String,
    cert_der: Vec<u8>,
}

/// 生成自签 CA（CN=TraeDeviceProxyCA，10 年有效期，ECDSA P-256）
fn generate_ca() -> Result<GeneratedCa, String> {
    let key_pair = KeyPair::generate().map_err(|e| format!("生成 CA 密钥失败: {e}"))?;
    // 私钥 PEM 必须在 key_pair 移交 Issuer 之前序列化
    let key_pem = key_pair.serialize_pem();
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "TraeDeviceProxyCA");
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(3650);
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(next_serial().into());

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("自签 CA 失败: {e}"))?;
    let cert_pem = cert.pem();
    let cert_der = cert.der().to_vec();
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, key_pair)
        .map_err(|e| format!("构建 CA 签发器失败: {e}"))?;
    Ok(GeneratedCa { issuer, cert_pem, key_pem, cert_der })
}

/// 从 PEM 加载已有 CA（Python cryptography 生成的 RSA PKCS#1 私钥可直接解析）。
/// 私钥按 PEM 标签解析为原始格式的 PrivateKeyDer 并原样保留（rustls 按 variant
/// 分派解析）——rcgen 对 PKCS#1 进的密钥 serialize_der 原样返回 PKCS#1，硬包
/// PrivatePkcs8KeyDer 会在构建 ServerConfig 时报「failed to parse private key」。
fn load_issuer(cert_pem: &str, key_pem: &str) -> Result<Issuer<'static, KeyPair>, String> {
    let key_der = PrivateKeyDer::from_pem_reader(&mut key_pem.as_bytes())
        .map_err(|e| format!("解析 CA 私钥 PEM 失败: {e}"))?;
    let key_pair = KeyPair::try_from(&key_der)
        .map_err(|_| "解析 CA 私钥失败: Could not parse key pair".to_string())?;
    Issuer::from_ca_cert_pem(cert_pem, key_pair).map_err(|e| format!("解析 CA 证书失败: {e}"))
}

/// PEM(CERTIFICATE) → DER：提取 base64 主体并解码（供补写 ca.cer）
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let body = pem
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|s| s.split("-----END CERTIFICATE-----").next())
        .ok_or_else(|| "ca.crt 缺少 CERTIFICATE PEM 块".to_string())?;
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("ca.crt base64 解码失败: {e}"))
}

/// rustls CryptoProvider：tokio-rustls 默认启用 aws-lc-rs，全进程共享单例
fn load_crypto_provider() -> CryptoProvider {
    // install_default 幂等：已被 tungstenite/其他模块安装过则忽略
    let provider = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider();
    let _ = provider.clone().install_default();
    provider
}

/// 启动时清扫历史残留：Python 版叶子证书临时文件（leaf_*.crt/.key）私钥曾落盘，全部删除
fn sweep_legacy_leaf_files(certs_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(certs_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("leaf_") && (name.ends_with(".crt") || name.ends_with(".key")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 读取前 ACL 自愈（仅 Windows，尽力而为 + 明确报错）：探测任一目标文件当前用户
/// 不可读（空 DACL 等损坏）→ icacls /reset /T 恢复继承后重探；仍不可读则报人话
/// 错误指引删除 certs 目录重新生成。cert_install 阶段借此保证不再带着坏文件走到
/// certutil 才失败；/reset 恢复父目录继承属 fail-open 取舍（证书本就要公开分发，
/// 目录收紧仅纵深防御，绝不能因此阻断 CA 自身读写）。
fn self_heal_acl_if_needed(
    certs_dir: &std::path::Path,
    targets: &[&std::path::Path],
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let readable = |p: &std::path::Path| std::fs::File::open(p).is_ok();
        if targets.iter().all(|p| readable(p)) {
            return Ok(());
        }
        let _ = std::process::Command::new("icacls")
            .arg(certs_dir)
            .args(["/reset", "/T"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output();
        if targets.iter().all(|p| readable(p)) {
            return Ok(());
        }
        Err(format!(
            "证书文件 ACL 权限损坏且自动修复失败（拒绝访问）。请完全退出助手后删除 \
             {} 整个文件夹，重新打开助手并再次点击「安装证书」",
            certs_dir.display()
        ))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (certs_dir, targets);
        Ok(())
    }
}

/// 收紧 CA 目录 ACL（仅 Windows，尽力而为）：移除继承、仅当前用户完全控制，
/// 防止同机低权限账户读取 CA 私钥。自验证失败自动 /reset 回滚（fail-open：
/// ACL 仅纵深防御，绝不能因此破坏代理自身的 CA 读写）。
/// F-75：Windows 专属（icacls ACL 模型）；mac Keychain 自管理不适用，空实现。
#[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
fn harden_ca_dir(certs_dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let user = std::env::var("USERNAME").unwrap_or_default();
        if user.is_empty() {
            return;
        }
        let run = |args: &[&str]| {
            crate::platform::cmd::sys_command("icacls")
                .arg(certs_dir)
                .args(args)
                .output()
        };
        // 记录收紧前已有文件：NTFS 动态继承下 /inheritance:r 移除目录可继承 ACE
        // 时，已有子文件的继承 ACE 会被同步清空（DACL 变空 → 连属主都拒绝访问）
        let existing: Vec<std::path::PathBuf> = certs_dir
            .read_dir()
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        // grant 必须带 (OI)(CI) 继承标志：否则目录 DACL 无可继承 ACE，
        // 已有子文件继承 ACE 被动态清空、新建子文件依赖进程默认 DACL——
        // 旧实现（无标志）正是用户「certutil 提权也读不到 ca.cer」的根因。
        // icacls 退出码必须检查：grant 侧失败（如用户名解析失败）而
        // /inheritance:r 已生效时，目录会变成空 DACL（protected + 零 ACE）
        let hardened = run(&["/inheritance:r", "/grant:r", &format!("{user}:(OI)(CI)F")])
            .map(|o| o.status.success())
            .unwrap_or(false);
        // 自验证双探针：① 收紧前已有的文件收紧后必须仍可读（能发现继承 ACE
        // 被动态清空的真实伤害）；② 目录下新建临时文件可读（校验未来子文件的
        // 继承行为）。任一失败立即 /reset 回滚——fail-open：ACL 仅纵深防御，
        // 绝不能因此破坏代理自身的 CA 读写
        let existing_ok = hardened && existing.iter().all(|p| std::fs::File::open(p).is_ok());
        let probe_ok = existing_ok && probe_new_file_readable(certs_dir);
        if !probe_ok {
            let _ = run(&["/reset", "/T"]);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = certs_dir;
    }
}

/// 在目录下新建临时文件并重读，验证子文件继承到的 ACL 允许当前用户读写；
/// 用于发现「目录 DACL 收紧后变空/丢失访问权」的坏状态。尽力而为。
#[cfg(target_os = "windows")]
fn probe_new_file_readable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".acl_probe");
    let ok = std::fs::write(&probe, b"probe")
        .and_then(|_| std::fs::read(&probe).map(|d| d == b"probe"))
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ca_cer_is_regenerated_from_pem() {
        let tmp = std::env::temp_dir().join(format!("aiwork_ca_test2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let _ca = ensure_ca(&tmp).expect("ensure_ca should generate");
        assert!(tmp.join("ca.cer").exists());
        // 模拟老版本/异常过程只留下 ca.crt+ca.key：删除 ca.cer 后重载应自动补写
        std::fs::remove_file(tmp.join("ca.cer")).unwrap();
        let _ca2 = ensure_ca(&tmp).expect("reload");
        let der = std::fs::read(tmp.join("ca.cer")).unwrap();
        assert!(!der.is_empty());
        let pem = std::fs::read_to_string(tmp.join("ca.crt")).unwrap();
        assert_eq!(der, pem_to_der(&pem).unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn generated_ca_roundtrip_and_leaf() {
        let tmp = std::env::temp_dir().join(format!("aiwork_ca_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let ca = ensure_ca(&tmp).expect("ensure_ca should generate");
        // 文件三件套齐全
        assert!(tmp.join("ca.crt").exists());
        assert!(tmp.join("ca.key").exists());
        assert!(tmp.join("ca.cer").exists());
        // 再次加载：兼容自生成的 PEM
        let ca2 = ensure_ca(&tmp).expect("reload");
        let _ = ca2.gen_server_config("api.trae.cn");

        let cfg = ca.gen_server_config("api.trae.cn");
        // 不广播 ALPN：对齐 Python 版行为（客户端回落 HTTP/1.1）
        assert!(cfg.alpn_protocols.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 兼容性红线回归：老版 Python 生成的 CA（RSA 2048、PKCS#1 "BEGIN RSA PRIVATE KEY"）
    /// 必须原样加载。ring 后端 KeyPair 只认 PKCS#8（报「Could not parse key pair」），
    /// 依赖 Cargo.toml 启用 aws_lc_rs feature；本测试固定夹具锁死该行为。
    #[test]
    fn legacy_python_pkcs1_ca_loads() {
        const LEGACY_CA_CERT: &str = include_str!("fixtures/legacy_ca_pkcs1.crt");
        const LEGACY_CA_KEY: &str = include_str!("fixtures/legacy_ca_pkcs1.key");
        let tmp = std::env::temp_dir().join(format!("aiwork_ca_pkcs1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("ca.crt"), LEGACY_CA_CERT).unwrap();
        std::fs::write(tmp.join("ca.key"), LEGACY_CA_KEY).unwrap();

        let ca = ensure_ca(&tmp).expect("legacy PKCS#1 CA must load");
        // 签发器可用：复用 RSA CA 密钥签发叶子证书
        let cfg = ca.gen_server_config("api.trae.cn");
        // 不广播 ALPN：对齐 Python 版行为（客户端回落 HTTP/1.1）
        assert!(cfg.alpn_protocols.is_empty());
        // 加载路径不得重写 CA 文件（换 CA = 强制所有用户重装证书）
        assert_eq!(
            std::fs::read_to_string(tmp.join("ca.crt")).unwrap(),
            LEGACY_CA_CERT
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// issue #14 回归：目标文件可读时 ACL 自愈必须为纯探测（不触发 icacls、
    /// 无副作用）直接 Ok——正常环境每次 ensure_ca 都会走到此路径
    #[test]
    fn acl_self_heal_is_noop_when_readable() {
        let tmp = std::env::temp_dir().join(format!("aiwork_ca_acl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cert = tmp.join("ca.crt");
        let key = tmp.join("ca.key");
        std::fs::write(&cert, b"cert").unwrap();
        std::fs::write(&key, b"key").unwrap();

        self_heal_acl_if_needed(&tmp, &[&cert, &key]).expect("readable files must pass");
        assert_eq!(std::fs::read(&cert).unwrap(), b"cert");
        assert_eq!(std::fs::read(&key).unwrap(), b"key");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
