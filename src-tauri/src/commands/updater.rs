// ---------------- 应用自更新（检查 / 下载 / 安装，两步确认制） ----------------
//
// 数据源：GitHub Releases（api.github.com）。资产命名约定（见 scripts/rename_release.py）：
//   AI Work 助手_<ver>_x64-setup.exe   ← NSIS 安装包（首选，支持原地升级 + 老版迁移钩子）
//   AI Work 助手_<ver>_x64_zh-CN.msi   ← MSI（备选；仅同 identifier 的 3.x 间可原地升级）
//
// 流程（下载与安装拆分，UI 两处确认）：
//   update_check       解析最新 release 并与 CARGO_PKG_VERSION 比较；同时解析发布校验清单
//                      latest.json（scripts/rename_release.py 生成随 Release 上传）取安装包 SHA256，
//                      清单存在即 fail-closed（缺失/损坏/版本不符均阻止自动更新），无清单回退正文约定行；
//   update_download    下载资产到临时目录（emit update-download-progress），完成后与发布方
//                      SHA256 比对（不匹配即删除并报错），返回文件路径由前端确认后再安装；
//   update_run_installer 以 /P /UPDATE /R 启动 NSIS 安装器：
//                      /P 被动模式（仅显示进度条）+ /UPDATE 跳过卸载直接覆盖 + /R 安装完成后自动重启应用
//                      （自定义模板 build-assets/installer.nsi 支持上述标志），随后应用退出。

use serde::Serialize;
use std::io::{Read, Write};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const RELEASES_API: &str =
    "https://api.github.com/repos/smart-open/TraeWorkAssistant/releases?per_page=100";
const RELEASES_PAGE: &str = "https://github.com/smart-open/TraeWorkAssistant/releases";

/// 发布校验清单资产名（scripts/rename_release.py 生成，随 Release 上传）：
/// `{ "version": "x.y.z", "assets": { "<资产文件名>": "<sha256hex>" } }`
const MANIFEST_ASSET: &str = "latest.json";

#[derive(Serialize, Clone)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    /// 资产文件名，如 "AI Work 助手_3.0.1_x64-setup.exe"
    pub asset_name: String,
    /// 资产下载直链（browser_download_url）
    pub download_url: String,
    /// 资产字节数
    pub size: u64,
    pub release_page: String,
    /// 发布方提供的安装包 SHA256（优先取发布校验清单 latest.json，回退 release 正文约定行；
    /// 存量旧 release 两者皆无时为 None，跳过校验）
    pub sha256: Option<String>,
}

#[derive(Serialize, Clone)]
struct DownloadProgress {
    received: u64,
    total: u64,
    percent: u64,
}

/// 解析 "v3.0.1" / "3.0.1" → (3,0,1)。不合法返回 None。
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let t = s.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split('.');
    let a: u64 = it.next()?.trim().parse().ok()?;
    let b: u64 = it.next()?.trim().parse().ok()?;
    let c: u64 = it.next().unwrap_or("0").trim().parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// 从资产文件名提取版本："AI Work 助手_3.0.1_x64-setup.exe" → (3,0,1)。
/// 规则：取倒数第二个下划线段（倒数第一段是 "x64-setup.exe" / "x64_zh-CN.msi" 的尾部）。
fn version_from_asset(name: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() < 2 {
        return None;
    }
    parse_version(parts[parts.len() - 2])
}

fn cmp_version(a: (u64, u64, u64), b: (u64, u64, u64)) -> std::cmp::Ordering {
    a.cmp(&b)
}

/// 读当前 Windows 系统代理（即用户 VPN）为 ureq 可用的代理 URL；未启用或格式异常返回 None。
/// 注册表 ProxyServer 有两种形态："host:port" 或 "http=..;https=..;ftp=.."（按协议区分）。
fn system_proxy_url() -> Option<String> {
    let (_, server, _) = crate::commands::proxy::get_existing_win_proxy()?;
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    let addr = if server.contains('=') {
        server
            .split(';')
            .find_map(|s| {
                let s = s.trim();
                s.strip_prefix("https=")
                    .or_else(|| s.strip_prefix("http="))
                    .map(|v| v.trim().to_string())
            })
            .unwrap_or_else(|| server.to_string())
    } else {
        server.to_string()
    };
    let url = if addr.contains("://") {
        addr
    } else {
        format!("http://{addr}")
    };
    ureq::Proxy::new(&url).ok().map(|_| url)
}

/// 按优先级构建尝试序列：系统代理（用户 VPN）→ 环境变量代理 → 直连。
/// 每项带标签，用于日志与报错文案；`finish` 为各场景的收尾超时配置。
fn attempt_agents(finish: impl Fn(ureq::AgentBuilder) -> ureq::Agent) -> Vec<(&'static str, ureq::Agent)> {
    let mut out: Vec<(&'static str, ureq::Agent)> = Vec::new();
    if let Some(url) = system_proxy_url() {
        if let Ok(p) = ureq::Proxy::new(&url) {
            out.push(("系统代理", finish(ureq::AgentBuilder::new().proxy(p))));
        }
    }
    let env_proxy = std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .or_else(|_| std::env::var("http_proxy"))
        .ok();
    if let Some(p) = env_proxy {
        if let Ok(proxy) = ureq::Proxy::new(&p) {
            out.push(("环境变量代理", finish(ureq::AgentBuilder::new().proxy(proxy))));
        }
    }
    // 「直连」通道无需显式禁用代理：项目未启用 ureq 的 proxy-from-env feature，
    // AgentBuilder::new() 默认不读环境变量代理，天然直连
    out.push(("直连", finish(ureq::AgentBuilder::new())));
    out
}

fn fetch_releases() -> Result<Vec<serde_json::Value>, String> {
    // 检查是小请求：连接 10s + 整体 20s，逐通道尝试（系统代理 → 环境变量代理 → 直连）
    let mut last_err = String::new();
    for (label, agent) in
        attempt_agents(|b| b.timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(20)).build())
    {
        match agent
            .get(RELEASES_API)
            .set("User-Agent", "ai-work-assistant-updater")
            .set("Accept", "application/vnd.github+json")
            .call()
        {
            Ok(resp) => {
                return resp
                    .into_json::<Vec<serde_json::Value>>()
                    .map_err(|e| format!("解析 release 响应失败: {e}"));
            }
            Err(e) => last_err = format!("[{label}] {e}"),
        }
    }
    Err(format!(
        "无法访问 GitHub Releases（{last_err}）。\n请检查网络或代理后重试；也可手动打开发布页下载：{}",
        RELEASES_PAGE
    ))
}

/// 在 release 资产中挑选安装包：优先 NSIS（x64-setup.exe），退而求其次 MSI。
fn pick_asset(assets: &[serde_json::Value]) -> Option<(String, String, u64)> {
    // [(name, url, size)] 两轮：先 NSIS 后 MSI
    let mut parsed: Vec<(String, String, u64, bool)> = Vec::new(); // bool=is_nsis
    for a in assets {
        let name = a.get("name")?.as_str()?.to_string();
        let url = a.get("browser_download_url")?.as_str()?.to_string();
        let size = a.get("size")?.as_u64().unwrap_or(0);
        let is_nsis = name.ends_with("_x64-setup.exe");
        let is_msi = name.ends_with("_x64_zh-CN.msi");
        if is_nsis || is_msi {
            parsed.push((name, url, size, is_nsis));
        }
    }
    // NSIS 优先；同类型取文件名版本号最大者（release 内一般只有一个，防御性处理）
    parsed.sort_by(|a, b| {
        let ka = (a.3, version_from_asset(&a.0).unwrap_or((0, 0, 0)));
        let kb = (b.3, version_from_asset(&b.0).unwrap_or((0, 0, 0)));
        kb.cmp(&ka)
    });
    parsed
        .into_iter()
        .next()
        .map(|(name, url, size, _)| (name, url, size))
}

// ---------------- 发布校验清单（latest.json） ----------------

/// 发布校验清单结构（scripts/rename_release.py 生成并随 Release 上传）
#[derive(serde::Deserialize)]
struct UpdateManifest {
    version: String,
    assets: std::collections::BTreeMap<String, String>,
}

/// 64 位十六进制 SHA-256 格式校验
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 宽松键：仅保留 ASCII 字母数字（小写），其余字符一律折叠为单个 '_'。
/// GitHub 上传会重写资产名（空格 → '.'、非 ASCII 字符 → '_'，
/// 如 "AI Work 助手_3.3.3_x64-setup.exe" → "AI.Work._3.3.3_x64-setup.exe"），
/// 清单键保存的是本地原始文件名，用宽松键对两侧归一后即可匹配。
fn relaxed_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_underscore = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    out
}

/// 从清单中查找资产的 SHA-256（不存在或格式非法返回 None）：
/// 先按原始文件名精确匹配，再用宽松键归一匹配（兼容 GitHub 的资产名重写）。
fn lookup_manifest_hash(m: &UpdateManifest, asset_name: &str) -> Option<String> {
    if let Some(h) = m.assets.get(asset_name).filter(|h| is_sha256_hex(h)) {
        return Some(h.to_ascii_lowercase());
    }
    let key = relaxed_key(asset_name);
    m.assets
        .iter()
        .find(|(k, _)| relaxed_key(k) == key)
        .map(|(_, h)| h.to_ascii_lowercase())
        .filter(|h| is_sha256_hex(h))
}

/// 下载并解析 release 的校验清单，返回目标资产的发布方 SHA-256：
/// - Ok(Some(hash))：清单命中
/// - Ok(None)：release 没有 latest.json 资产（旧版发布方式），调用方回退正文提取
/// - Err(msg)：清单存在但不可用（下载失败/损坏/版本不符/未收录资产）——
///   fail-closed 直接报错阻止自动更新，引导手动下载（S1 insecure_update 纵深防御）
fn fetch_manifest_hash(
    assets: &[serde_json::Value],
    asset_name: &str,
    expected_ver: (u64, u64, u64),
) -> Result<Option<String>, String> {
    let url = assets
        .iter()
        .filter_map(|a| {
            let name = a.get("name").and_then(|v| v.as_str())?;
            (name == MANIFEST_ASSET)
                .then(|| a.get("browser_download_url").and_then(|v| v.as_str()))
                .flatten()
        })
        .next();
    let Some(url) = url else {
        return Ok(None);
    };

    // 小文件：连接 10s + 整体 20s，逐通道尝试（系统代理 → 环境变量代理 → 直连）
    let mut text: Option<String> = None;
    let mut last_err = String::new();
    for (label, agent) in attempt_agents(|b| {
        b.timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
    }) {
        match agent
            .get(url)
            .set("User-Agent", "ai-work-assistant-updater")
            .call()
        {
            Ok(resp) => {
                let mut s = String::new();
                match resp.into_reader().read_to_string(&mut s) {
                    Ok(_) => {
                        text = Some(s);
                        break;
                    }
                    Err(e) => last_err = format!("[{label}] 读取失败: {e}"),
                }
            }
            Err(e) => last_err = format!("[{label}] {e}"),
        }
    }
    let text = text.ok_or_else(|| {
        format!("下载校验清单失败（{last_err}），已阻止自动更新。请重试或手动下载：{RELEASES_PAGE}")
    })?;

    let manifest: UpdateManifest = serde_json::from_str(&text).map_err(|e| {
        format!(
            "校验清单损坏（{MANIFEST_ASSET} 解析失败: {e}），已阻止自动更新。请手动下载：{RELEASES_PAGE}"
        )
    })?;
    if parse_version(&manifest.version) != Some(expected_ver) {
        return Err(format!(
            "校验清单版本（{}）与目标版本不一致，已阻止自动更新。请手动下载：{RELEASES_PAGE}",
            manifest.version
        ));
    }
    lookup_manifest_hash(&manifest, asset_name).ok_or_else(|| {
        format!("校验清单中未收录该安装包的 SHA-256，已阻止自动更新。请手动下载：{RELEASES_PAGE}")
    }).map(Some)
}

/// 本产品（AI Work 助手）产品线起点：只认 >= 3.0.0 的 release。
/// 同一仓库还发布 2.x 产品线（Trae Work 助手，另一个产品），必须排除。
const PRODUCT_MIN_VERSION: (u64, u64, u64) = (3, 0, 0);

/// 检查 GitHub Releases 上本产品线（>= 3.0.0）的最新版本，与当前应用版本比较。
/// async 派发：网络请求最坏 90s（3 通道 × 30s），同步命令默认跑主线程会冻住 UI，必须异步执行。
#[tauri::command(async)]
pub fn update_check() -> Result<UpdateCheckResult, String> {
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .ok_or("内置版本号解析失败")?;
    let releases = fetch_releases()?;

    // 收集本产品线候选 release：(版本, tag, html_url, assets, 正文)
    let mut candidates: Vec<((u64, u64, u64), String, String, Vec<serde_json::Value>, String)> =
        Vec::new();
    for rel in &releases {
        if rel.get("draft").and_then(|v| v.as_bool()).unwrap_or(false)
            || rel.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            continue;
        }
        let tag = rel
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let html_url = rel
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or(RELEASES_PAGE)
            .to_string();
        let body = rel
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let assets = rel
            .get("assets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // 版本优先取 tag（v3.0.1）；tag 不合法时从资产名推导
        let version = parse_version(&tag)
            .or_else(|| pick_asset(&assets).and_then(|(name, _, _)| version_from_asset(&name)));
        if let Some(v) = version {
            if v >= PRODUCT_MIN_VERSION {
                candidates.push((v, tag, html_url, assets, body));
            }
        }
    }
    // 本产品线内取版本最高者
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let min_ver =
        format!("{}.{}.{}", PRODUCT_MIN_VERSION.0, PRODUCT_MIN_VERSION.1, PRODUCT_MIN_VERSION.2);
    let (latest, tag, release_page, assets, body) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| format!("发布页上没有找到本产品线（v{min_ver} 起）的 release。可手动查看：{RELEASES_PAGE}"))?;

    let (asset_name, download_url, size) = pick_asset(&assets)
        .ok_or_else(|| format!("最新 release（{tag}）中没有可用的安装包资产。可手动查看：{RELEASES_PAGE}"))?;
    // 防御：资产名版本必须与 release 版本一致，避免误装其他产品线的安装包
    if version_from_asset(&asset_name) != Some(latest) {
        return Err(format!(
            "release（{tag}）的资产版本与 release 版本不一致，已中止。可手动查看：{RELEASES_PAGE}"
        ));
    }
    // 完整性校验（S1 insecure_update 纵深防御），优先级：
    // ① 发布校验清单 latest.json（机器可读 + 版本绑定）：存在即强制走清单且 fail-closed——
    //    下载失败/损坏/版本不符/未收录资产任一情况直接报错阻止自动更新，引导手动下载
    // ② 回退 release 正文约定行（宽松文本匹配，兼容 3.3.2 的发布方式）
    // ③ 两者皆无（存量旧 release）：跳过校验（兼容过渡）
    let sha256 = match fetch_manifest_hash(&assets, &asset_name, latest) {
        Ok(h) => h,
        Err(e) => return Err(e),
    }
    .or_else(|| extract_sha256(&body, &asset_name));

    let has_update = cmp_version(latest, current) == std::cmp::Ordering::Greater;
    Ok(UpdateCheckResult {
        has_update,
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        latest_version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        asset_name,
        download_url,
        size,
        release_page,
        sha256,
    })
}

/// 从 release 正文提取指定资产的 SHA256（64 位 hex）。
/// 发布约定行：正文含同时出现资产名与 64 位 hex 的行（如 `SHA256(<资产名>): <hex>`）。
/// 宽松匹配以兼容 `<hex> <资产名>`（sha256sum 风格）与 `SHA256: <hex> <资产名>` 等写法；
/// 找不到（旧版本 release）返回 None，调用方跳过校验。
fn extract_sha256(body: &str, asset_name: &str) -> Option<String> {
    for line in body.lines() {
        if line.contains(asset_name) {
            if let Some(hex) = find_sha256_hex(line) {
                return Some(hex);
            }
        }
    }
    None
}

/// 在单行内查找连续 64 位 hex（前后不接 hex 字符），避免把版本号数字等混入拼接。
fn find_sha256_hex(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let is_hex = |c: char| c.is_ascii_hexdigit();
    let n = chars.len();
    if n < 64 {
        return None;
    }
    for i in 0..=(n - 64) {
        if chars[i..i + 64].iter().all(|c| is_hex(*c))
            && (i == 0 || !is_hex(chars[i - 1]))
            && (i + 64 == n || !is_hex(chars[i + 64]))
        {
            return Some(chars[i..i + 64].iter().collect::<String>().to_lowercase());
        }
    }
    None
}

/// 流式计算文件 SHA256（64KB 缓冲，支持大安装包）。
fn file_sha256(path: &std::path::Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开安装包失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读取安装包失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 下载结果（供前端「确认二：安装」使用）
#[derive(Serialize, Clone)]
pub struct UpdateDownloaded {
    /// 安装包在本地磁盘的完整路径
    pub file_path: String,
    pub asset_name: String,
    /// 安装包字节数（实际下载大小）
    pub size: u64,
    /// 目标版本
    pub version: String,
}

/// 校验下载目标：资产名版本必须与检查结果一致，且大于当前版本。
/// 同时防御路径注入：资产名只允许安全文件名字符（来自 GitHub API，纵深防御）。
fn validate_target(asset_name: &str, expected_version: &str) -> Result<(u64, u64, u64), String> {
    if asset_name.contains(['/', '\\', ':'])
        || asset_name.split(['.', ' ']).any(|seg| seg == "..")
        || asset_name.contains("..")
    {
        return Err(format!("资产名不合法，已中止: {asset_name}"));
    }
    let asset_ver = version_from_asset(asset_name)
        .ok_or_else(|| format!("资产名无法解析版本号: {asset_name}"))?;
    let expected = parse_version(expected_version).ok_or("目标版本号解析失败")?;
    if asset_ver != expected {
        return Err(format!(
            "资产版本 {asset_ver:?} 与检查到的目标版本 {expected:?} 不一致，已中止"
        ));
    }
    let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap();
    if cmp_version(asset_ver, current) != std::cmp::Ordering::Greater {
        return Err("目标版本不大于当前版本，无需更新".to_string());
    }
    Ok(asset_ver)
}

/// 第一步：下载安装包到临时目录（不安装）。完成后前端确认，再调 update_run_installer。
/// async 派发：下载耗时不可控（最坏 3 通道各连+读超时），同步命令跑主线程会冻住 UI。
#[tauri::command(async)]
pub fn update_download(
    app: AppHandle,
    download_url: String,
    asset_name: String,
    expected_version: String,
    expected_sha256: Option<String>,
) -> Result<UpdateDownloaded, String> {
    validate_target(&asset_name, &expected_version)?;

    // 防御：发布方摘要必须为合法 SHA-256 格式（update_check 已 fail-closed 保证存在，此处双保险）
    let expected = match expected_sha256.as_deref() {
        Some(s) => {
            let s = s.trim().to_ascii_lowercase();
            if !is_sha256_hex(&s) {
                return Err(
                    "发布方 SHA-256 缺失或格式非法，已中止下载。请重新检查更新，或手动下载安装"
                        .to_string(),
                );
            }
            Some(s)
        }
        None => None,
    };

    // 下载目录：%TEMP%\ai-work-assistant-update\
    let dir = std::env::temp_dir().join("ai-work-assistant-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    // 清理历史版本残留（只删本目录下的安装包文件，保留当前目标文件）
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.as_deref() != Some(asset_name.as_str())
                && p.is_file()
                && p.extension().map(|e| e == "exe" || e == "msi").unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let dest = dir.join(&asset_name);
    // 清理同名旧文件（可能是不完整下载）
    let _ = std::fs::remove_file(&dest);

    // 逐通道尝试下载（系统代理 → 环境变量代理 → 直连）。
    // 超时策略：连接 10s + 读 60s，不设整体超时（大文件慢速下载不能被整体超时掐断）。
    // 每次尝试都从 0 重新流式写盘并重发进度事件（进度条回跳属预期）。
    let mut last_err = String::new();
    for (label, agent) in
        attempt_agents(|b| b.timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(60)).build())
    {
        match download_via(&app, &agent, &download_url, &dest) {
            Ok(received) => {
                // 完整性校验：发布方提供 SHA256 时必须匹配，不匹配删除文件并中止（防供应链篡改）
                if let Some(expected) = expected.as_deref() {
                    let actual = file_sha256(&dest).unwrap_or_default();
                    if actual != expected {
                        let _ = std::fs::remove_file(&dest);
                        return Err(format!(
                            "安装包完整性校验失败（SHA256 不匹配），已删除下载文件。\
                             可能为网络劫持或下载损坏，请重试或手动下载：{RELEASES_PAGE}"
                        ));
                    }
                }
                let _ = app.emit(
                    "update-download-progress",
                    DownloadProgress { received, total: received, percent: 100 },
                );
                return Ok(UpdateDownloaded {
                    file_path: dest.to_string_lossy().into_owned(),
                    asset_name: asset_name.clone(),
                    size: received,
                    version: expected_version,
                });
            }
            Err(e) => {
                last_err = format!("[{label}] {e}");
                let _ = std::fs::remove_file(&dest);
            }
        }
    }
    Err(format!(
        "下载安装包失败（{last_err}）。\n若你开启了 VPN/代理仍失败，请确认代理可用后重试；也可手动下载：{}",
        RELEASES_PAGE
    ))
}

/// 单通道完整下载：请求 → 流式写盘 → 进度事件 → 完整性校验，返回实际接收字节数。
fn download_via(
    app: &AppHandle,
    agent: &ureq::Agent,
    download_url: &str,
    dest: &std::path::Path,
) -> Result<u64, String> {
    let resp = agent
        .get(download_url)
        .set("User-Agent", "ai-work-assistant-updater")
        .call()
        .map_err(|e| format!("连接失败: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let file = std::fs::File::create(dest).map_err(|e| format!("创建安装包文件失败: {e}"))?;
    let mut writer = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载中断: {e}（可删除临时文件后重试：{:?}）", dest))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("写入安装包失败: {e}"))?;
        received += n as u64;
        // 每 1 MiB 或完成时发一次进度
        if total > 0 && (received - last_emit >= 1024 * 1024 || received >= total) {
            last_emit = received;
            let percent = (received.min(total)) * 100 / total;
            let _ = app.emit(
                "update-download-progress",
                DownloadProgress { received, total, percent },
            );
        }
    }
    writer.flush().ok();
    drop(writer);
    if total > 0 && received < total {
        return Err(format!(
            "下载不完整（{received}/{total} 字节），请重试或手动下载：{RELEASES_PAGE}"
        ));
    }
    Ok(received)
}

/// 第二步：启动 NSIS 安装器（/P /UPDATE /R）——被动模式显示进度条，
/// /UPDATE 跳过卸载直接覆盖，/R 安装完成后自动重启本应用；随后当前进程退出。
#[tauri::command]
pub fn update_run_installer(
    app: AppHandle,
    file_path: String,
    asset_name: String,
) -> Result<(), String> {
    // 防御：只允许运行本应用临时更新目录内的安装包，且版本必须大于当前版本
    let path = std::path::Path::new(&file_path);
    let expected_dir = std::env::temp_dir().join("ai-work-assistant-update");
    if !path.is_file()
        || path.parent() != Some(expected_dir.as_path())
    {
        return Err(format!("非法的安装包路径，已中止：{file_path}"));
    }
    let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap();
    match version_from_asset(&asset_name) {
        Some(v) if cmp_version(v, current) == std::cmp::Ordering::Greater => {}
        _ => return Err("安装包版本不大于当前版本，已中止".to_string()),
    }

    // /P 进度条可见 + /UPDATE 跳过卸载直接覆盖 + /R 完成后自动重启应用
    std::process::Command::new(path)
        .args(["/P", "/UPDATE", "/R"])
        .spawn()
        .map_err(|e| format!("启动安装程序失败: {e}（可手动运行：{file_path}）"))?;

    // 提示前端后退出，让安装器接管（安装钩子会兜底结束本进程解锁文件占用）
    let _ = app.emit("update-installing", asset_name);
    std::thread::sleep(Duration::from_millis(800));
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_format() {
        let h = "a".repeat(64);
        assert!(is_sha256_hex(&h));
        assert!(is_sha256_hex(&"0123456789ABCDEF".repeat(4)));
        assert!(!is_sha256_hex(&"a".repeat(63))); // 不足 64 位
        assert!(!is_sha256_hex(&"g".repeat(64))); // 非十六进制字符
        assert!(!is_sha256_hex(""));
    }

    #[test]
    fn manifest_parse_and_exact_lookup() {
        let text = r#"{
            "version": "3.3.3",
            "assets": {
                "AI Work 助手_3.3.3_x64-setup.exe": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "AI Work 助手_3.3.3_x64_portable.zip": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
            }
        }"#;
        let m: UpdateManifest = serde_json::from_str(text).unwrap();
        assert_eq!(parse_version(&m.version), Some((3, 3, 3)));
        // 清单键为本地原始文件名（含空格），按原样可精确命中
        let h = lookup_manifest_hash(&m, "AI Work 助手_3.3.3_x64-setup.exe").unwrap();
        assert_eq!(h, "a".repeat(64));
        // 未收录 / 哈希格式非法 → None
        assert!(lookup_manifest_hash(&m, "AI Work 助手_3.3.3_x64_zh-CN.msi").is_none());
        assert!(lookup_manifest_hash(&m, "不存在.exe").is_none());
    }

    #[test]
    fn manifest_lookup_normalizes_github_asset_name() {
        // GitHub 上传会重写资产名：空格 → '.'、非 ASCII（助手）→ '_'
        let text = r#"{
            "version": "3.3.3",
            "assets": {
                "AI Work 助手_3.3.3_x64-setup.exe": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            }
        }"#;
        let m: UpdateManifest = serde_json::from_str(text).unwrap();
        let gh_name = "AI.Work._3.3.3_x64-setup.exe";
        let h = lookup_manifest_hash(&m, gh_name).unwrap();
        assert_eq!(h, "a".repeat(64));
        // 大小写归一：清单侧哈希大小写任意，输出统一小写
        let text2 = r#"{
            "version": "3.3.3",
            "assets": {
                "AI Work 助手_3.3.3_x64-setup.exe": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }"#;
        let m2: UpdateManifest = serde_json::from_str(text2).unwrap();
        assert_eq!(lookup_manifest_hash(&m2, gh_name).unwrap(), h);
        // 格式非法（长度不足）的哈希不命中
        let text3 = r#"{
            "version": "3.3.3",
            "assets": { "AI Work 助手_3.3.3_x64-setup.exe": "zz" }
        }"#;
        let m3: UpdateManifest = serde_json::from_str(text3).unwrap();
        assert!(lookup_manifest_hash(&m3, gh_name).is_none());
    }

    #[test]
    fn manifest_version_mismatch_detected() {
        let text = r#"{ "version": "3.2.9", "assets": {} }"#;
        let m: UpdateManifest = serde_json::from_str(text).unwrap();
        assert_ne!(parse_version(&m.version), Some((3, 3, 3)));
    }
}
