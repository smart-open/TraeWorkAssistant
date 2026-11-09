// ---------------- 应用自更新（检查 / 下载 / 安装，两步确认制） ----------------
//
// 数据源：GitHub Releases（api.github.com）。资产命名约定（见 scripts/rename_release.mjs）：
//   AI Work 助手_<ver>_x64-setup.exe   ← NSIS 安装包（首选，支持原地升级 + 老版迁移钩子）
//   AI Work 助手_<ver>_x64_zh-CN.msi   ← MSI（仅作手动安装备选；应用内更新不使用，见 pick_asset）
//
// 流程（下载与安装拆分，UI 两处确认）：
//   update_check       解析最新 release 并与 CARGO_PKG_VERSION 比较；同时解析发布校验清单
//                      latest.json（scripts/rename_release.mjs 生成随 Release 上传）取安装包 SHA256，
//                      清单存在即 fail-closed（缺失/损坏/版本不符均阻止自动更新），无清单回退正文约定行；
//   update_download    下载资产到临时目录（emit update-download-progress）。完整性三道闸：
//                      ① 拒绝缺失/非法的发布方 SHA256（None 不放行）；② 下载完成与发布方
//                      SHA256 比对（不匹配即删除并报错），通过后落 .sha256 sidecar；
//                      ③ update_run_installer 安装前校验「文件名与资产名绑定 + SHA256 复验」。
//                      返回文件路径由前端确认后再安装；
//   update_run_installer 经 cmd /c start 中转以 /P /UPDATE /R 启动 NSIS 安装器（issue #135）：
//                      /P 被动模式（仅显示进度条）+ /UPDATE 跳过卸载直接覆盖 + /R 安装完成后自动重启应用
//                      （自定义模板 build-assets/installer.nsi 支持上述标志）；中转启动令安装器在应用
//                      退出前即脱离本应用进程树，避免安装器 PREINSTALL 的 taskkill 树杀误伤自身，
//                      随后应用退出。

use serde::Serialize;
use std::io::{Read, Write};
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

const RELEASES_API: &str =
    "https://api.github.com/repos/smart-open/TraeWorkAssistant/releases?per_page=100";
const RELEASES_PAGE: &str = "https://github.com/smart-open/TraeWorkAssistant/releases";

// 审查修复（M-6，下载 URL 单前缀锚）：download_url 经 IPC 由前端传入，被注入的
// webview 可传攻击者 URL + 自供 sha256 绕过域校验走完三道闸（下载并安装恶意包）。
// 下载地址仅接受官方 GitHub Releases 资产直链前缀，校验单点收口于 update_download
// （update_check 返回的 download_url 来自 GitHub API 受信来源，天然命中前缀；
// 自动更新路径 update_auto_tick 内部走同一函数，无需重复校验）
const RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/smart-open/TraeWorkAssistant/releases/download/";

/// 发布校验清单资产名（scripts/rename_release.mjs 生成，随 Release 上传）：
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
    /// 存量旧 release 两者皆无时为 None——update_download 将拒绝下载，fail-closed）
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

/// 在 release 资产中挑选安装包：仅接受 NSIS（x64-setup.exe）。
/// MSI 不参与挑选（P4，#135 审查收尾）：update_run_installer 统一按 NSIS 参数
/// （/P /UPDATE /R）启动安装器，msiexec 语义中 /P 是补丁模式——旧「MSI 兜底」
/// 选出即死路，宁可在此返回 None 走「没有可用的安装包资产」显式报错引导手动下载，
/// 也不静默选出必然失败的资产。
fn pick_asset(assets: &[serde_json::Value]) -> Option<(String, String, u64)> {
    let mut parsed: Vec<(String, String, u64)> = Vec::new();
    for a in assets {
        let name = a.get("name")?.as_str()?.to_string();
        let url = a.get("browser_download_url")?.as_str()?.to_string();
        let size = a.get("size")?.as_u64().unwrap_or(0);
        if name.ends_with("_x64-setup.exe") {
            parsed.push((name, url, size));
        }
    }
    // 同类型取文件名版本号最大者（release 内一般只有一个，防御性处理）
    parsed.sort_by(|a, b| {
        version_from_asset(&b.0)
            .unwrap_or((0, 0, 0))
            .cmp(&version_from_asset(&a.0).unwrap_or((0, 0, 0)))
    });
    parsed.into_iter().next()
}

// ---------------- 发布校验清单（latest.json） ----------------

/// 发布校验清单结构（scripts/rename_release.mjs 生成并随 Release 上传）
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

/// 资产文件名白名单（D1，cmd 元字符注入防御）：update_run_installer 将安装包
/// 路径传入 `cmd /C start` 拼接，且 asset_name 参与临时目录/sidecar 路径 join。
/// Rust Command 对含空格参数自动加引号，但 `"` 可闭合引导，`&`/`^`/`%`/`!` 等
/// 在 cmd 解析层存在注入面。白名单仅放行字母数字（Unicode 语义，含既有资产名
/// 中文）+ 空格 `. _ - ( ) #`——发布流资产名 "AI Work 助手_<ver>_x64-setup.exe"
/// / "_x64_zh-CN.msi" 全命中，其余一律拒绝。
fn ensure_safe_asset_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("资产文件名为空，已中止".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '(' | ')' | '#'))
    {
        return Err(format!("资产文件名包含非法字符，已中止: {name}"));
    }
    Ok(())
}

/// 校验下载目标：资产名版本必须与检查结果一致，且大于当前版本。
/// 同时防御路径注入：资产名只允许安全文件名字符（来自 GitHub API，纵深防御）。
fn validate_target(asset_name: &str, expected_version: &str) -> Result<(u64, u64, u64), String> {
    // D1（cmd 元字符注入防御，先于路径校验）：文件名将参与 `cmd /C start` 拼接
    ensure_safe_asset_name(asset_name)?;
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

/// 审查修复（M-6）：下载地址信任判定——仅接受官方 GitHub Releases 资产直链前缀。
/// 独立成函数便于单测（update_download 依赖 AppHandle/网络，难以直测）。
fn is_trusted_download_url(url: &str) -> bool {
    url.starts_with(RELEASE_DOWNLOAD_PREFIX)
}

/// 审查修复（M-6 收口）：下载地址形态交叉校验——前缀闸只保证「官方 Releases 域名
/// 直链」，未校验 tag 与资产归属：同仓库历史 release 均落受信前缀（含 2.x 产品线
/// 安装包），可被指向旧资产降级。此处解析前缀剩余段 `<tag>/<资产文件名>`：
/// tag 必须与目标版本一致（兼容 "v3.8.2"/"3.8.2" 两形态——GitHub tag 带 v 前缀，
/// expected_version 是 parse_version 剥离后的裸版本）、末段文件名必须与目标资产
/// 一致、不得有多余路径段。独立成函数便于单测（同 is_trusted_download_url）。
fn verify_release_identity(
    url: &str,
    expected_version: &str,
    asset_name: &str,
) -> Result<(), String> {
    let rest = url
        .strip_prefix(RELEASE_DOWNLOAD_PREFIX)
        .ok_or_else(|| "下载地址不受信任（仅接受官方 GitHub Releases 资产）".to_string())?;
    let mut segs = rest.split('/');
    let tag = segs.next().unwrap_or("");
    let file = segs.next().unwrap_or("");
    if tag.is_empty() || file.is_empty() || segs.next().is_some() {
        return Err("下载地址形态非法（应为 /releases/download/<tag>/<资产文件名>）".to_string());
    }
    if tag != expected_version && tag != format!("v{expected_version}") {
        return Err(format!(
            "下载地址版本与目标版本不一致（URL tag={tag}，目标 {expected_version}），已中止"
        ));
    }
    if file != asset_name {
        return Err("下载地址资产名与目标资产不一致，已中止".to_string());
    }
    Ok(())
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
    // 审查修复（M-6）：前置信任校验——拒绝非官方前缀的下载地址（fail-closed），
    // 不信任的地址绝不进入下载流程
    if !is_trusted_download_url(&download_url) {
        return Err("下载地址不受信任（仅接受官方 GitHub Releases 资产）".to_string());
    }
    // 审查修复（M-6 收口）：前缀命中后交叉校验 URL 形态——tag 与目标版本一致、
    // 末段文件名与目标资产一致，闭合「受信前缀 + 任意旧 release 资产」降级面。
    // update_check 返回的 download_url 天然命中（tag/资产同源 GitHub API 响应），
    // 自动更新路径 update_auto_tick 走同一函数，无需重复校验
    verify_release_identity(&download_url, &expected_version, &asset_name)?;

    // 审查修复（H4，闸①）：发布方摘要是强制闸门——None（存量旧 release 无清单无正文行）
    // 与格式非法同样拒绝下载，绝不放行无校验的安装包（原实现 None 直接跳过校验，
    // 篡改/损坏的安装包可直通安装）
    let expected = match expected_sha256.as_deref() {
        Some(s) => {
            let s = s.trim().to_ascii_lowercase();
            if !is_sha256_hex(&s) {
                return Err(
                    "发布方 SHA-256 缺失或格式非法，已中止下载。请重新检查更新，或手动下载安装"
                        .to_string(),
                );
            }
            s
        }
        None => {
            return Err(
                "发布方 SHA-256 缺失（旧版发布方式，无校验信息），已中止下载。请手动下载安装"
                    .to_string(),
            )
        }
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
                && p.extension()
                    .map(|e| e == "exe" || e == "msi" || e == "sha256")
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let dest = dir.join(&asset_name);
    // 清理同名旧文件（可能是不完整下载）
    let _ = std::fs::remove_file(&dest);
    // 审查修复（H4，闸②）：校验文件 sidecar 路径（<资产名>.sha256，下载校验通过后写入，
    // 供 update_run_installer 安装前复验）；先清理同名旧 sidecar，避免跨轮次残留误配
    let sidecar = dir.join(format!("{asset_name}.sha256"));
    let _ = std::fs::remove_file(&sidecar);

    // 逐通道尝试下载（系统代理 → 环境变量代理 → 直连）。
    // 超时策略：连接 10s + 读 60s，不设整体超时（大文件慢速下载不能被整体超时掐断）。
    // 每次尝试都从 0 重新流式写盘并重发进度事件（进度条回跳属预期）。
    let mut last_err = String::new();
    for (label, agent) in
        attempt_agents(|b| b.timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(60)).build())
    {
        match download_via(&app, &agent, &download_url, &dest) {
            Ok(received) => {
                // 完整性校验（闸②前半）：下载完成立即与发布方 SHA256 比对，不匹配删除文件并中止
                // （防供应链篡改；expected 已在入口强制非空，无「跳过校验」路径）
                // 审查修复（L-1）：IO 失败与哈希不匹配区分归因——原 unwrap_or_default
                // 把读取失败折叠为「SHA256 不匹配/网络劫持」误导文案；两种情况均
                // fail-closed 删除下载文件
                let actual = match file_sha256(&dest) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = std::fs::remove_file(&dest);
                        return Err(format!(
                            "安装包完整性校验无法执行（读取失败），已删除下载文件：{e}"
                        ));
                    }
                };
                if actual != expected {
                    let _ = std::fs::remove_file(&dest);
                    return Err(format!(
                        "安装包完整性校验失败（SHA256 不匹配），已删除下载文件。\
                         可能为网络劫持或下载损坏，请重试或手动下载：{RELEASES_PAGE}"
                    ));
                }
                // 审查修复（H4，闸②后半）：校验通过后落 .sha256 sidecar（sha256sum 风格：
                // <hex>  <资产名>），供 update_run_installer 安装前复验，覆盖
                // 「下载后、安装前」窗口期的文件替换/篡改
                std::fs::write(&sidecar, format!("{expected}  {asset_name}\n"))
                    .map_err(|e| format!("写入校验文件失败: {e}"))?;
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
    // D1（cmd 元字符注入防御）：下方经 `cmd /C start` 拼接启动，asset_name 同时
    // 参与 sidecar 路径拼接——白名单拒绝 cmd 元字符（受信前端直调也纵深拦截）
    ensure_safe_asset_name(&asset_name)?;

    // 审查修复（H4，闸③）：安装前两道校验——
    // ① 文件名与资产名绑定：拒绝「A 文件路径 + B 资产名」的组合，防止借合法资产名
    //    通过版本校验后执行任意文件
    // ② SHA256 复验：读取下载时落盘的 .sha256 sidecar，对磁盘文件重新计算并比对，
    //    覆盖「下载后、安装前」窗口期的替换/篡改；sidecar 缺失或损坏同样拒绝
    //    （fail-closed：正常流程必有 sidecar，缺失说明文件不是本次更新流程下载的）
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("非法的安装包路径，已中止：{file_path}"))?;
    if file_name != asset_name {
        return Err(format!(
            "安装包文件名与资产名不一致（{file_name} / {asset_name}），已中止"
        ));
    }
    let sidecar = expected_dir.join(format!("{asset_name}.sha256"));
    let expected = std::fs::read_to_string(&sidecar).map_err(|_| {
        "安装包校验文件缺失，已中止安装。请重新走「检查更新 → 下载」流程，或手动下载安装"
            .to_string()
    })?;
    let expected = expected
        .split_whitespace()
        .next()
        .filter(|h| is_sha256_hex(h))
        .ok_or("安装包校验文件损坏，已中止安装。请重新下载")?
        .to_ascii_lowercase();
    // 审查修复（L-1）：IO 失败与哈希不匹配区分归因（原 unwrap_or_default 把读取
    // 失败折叠为「文件被改动」）；两种情况均 fail-closed 删除安装包（原实现不匹配
    // 时篡改文件仍残留磁盘）
    let actual = match file_sha256(path) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return Err(format!(
                "安装包完整性校验无法执行（读取失败），已删除安装包：{e}"
            ));
        }
    };
    if actual != expected {
        let _ = std::fs::remove_file(path);
        return Err(
            "安装包完整性校验失败（SHA256 与下载时不一致），文件可能在下载后被改动，已中止安装。请重新下载"
                .to_string(),
        );
    }

    // /P 进度条可见 + /UPDATE 跳过卸载直接覆盖 + /R 完成后自动重启应用。
    // Windows 经 `cmd /c start` 中转启动（issue #135）：cmd 启动安装器后立即退出，
    // 安装器随即脱离本应用的进程树——安装器 PREINSTALL 钩子会 taskkill 本产品主程序，
    // 若安装器仍是本应用的子进程，树杀（/T）会连同安装器一起强杀，安装中途静默中断
    // 导致升级失败；脱树后 taskkill 只命中应用自身。CREATE_NO_WINDOW 防止 cmd 控制台
    // 窗口闪现（安装包路径与存在性已在上方校验，start 找不到文件的场景已排除）。
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // .status() 等待 cmd 自身退出（cmd 启动 start 后立即退出，毫秒级，不阻塞脱树）：
        // spawn 成功只代表 cmd 进程创建成功，start 的 ShellExecute 失败（如杀软拦截、
        // 文件关联损坏）时 cmd 返回非零——校验退出码可在此报错提示手动运行安装包，
        // 而不是照常 emit+exit 静默失败（issue #135 加固）
        let status = std::process::Command::new("cmd")
            .args(["/C", "start", "", file_path.as_str(), "/P", "/UPDATE", "/R"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| format!("启动安装程序失败: {e}（可手动运行：{file_path}）"))?;
        if !status.success() {
            return Err(format!(
                "启动安装程序失败（退出码 {}），可手动运行：{file_path}",
                status.code().unwrap_or(-1)
            ));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(path)
            .args(["/P", "/UPDATE", "/R"])
            .spawn()
            .map_err(|e| format!("启动安装程序失败: {e}（可手动运行：{file_path}）"))?;
    }

    // 提示前端后退出，让安装器接管（安装钩子会兜底结束本进程解锁文件占用）
    // 结果型事件：emit 失败落 stderr（issue #44 遗留项；即将 exit，无 data_dir 需求）
    crate::events::emit_logged(&app, "update-installing", serde_json::Value::String(asset_name.clone()), None);
    // D2：800ms 延迟退出与安装器 PREINSTALL 宽限等待（installer-hooks.nsh：1.2s
    // 静默 + 存活探测循环）耦合——正常时序本应用先自行退出，安装器宽限仅兜底
    // 锁占用；两值需同步调整（应用退出耗时应 < 安装器兜底强杀）
    std::thread::sleep(Duration::from_millis(800));
    // 审查修复：走 Tauri 退出链（RunEvent::Exit）而非 std::process::exit——
    // 后者绕过清理链，代理运行中重启更新会残留死系统代理导致全局断网
    let _ = app.exit(0);
    Ok(())
}

// ---------------- F-86：全自动静默更新编排（issue #175，设置默认关） ----------------

/// 防重入闸：上一轮 tick 仍在下载/安装期间时跳过本轮（前端 2h 定时 + 此闸双保险）
static AUTO_TICK_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct AutoTickGuard;
impl Drop for AutoTickGuard {
    fn drop(&mut self) {
        AUTO_TICK_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 无人值守更新编排结果（前端仅用于状态展示与日志，不参与决策）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAutoTickResult {
    /// skipped_cli / skipped_disabled / busy / busy_gateway / no_update / error / installed
    pub action: String,
    pub detail: String,
}

/// F-86：无人值守更新编排——check → 空闲判定 → download → run_installer。
/// 由前端每 2 小时定时触发；设置 `auto_update_silent` 默认关。
/// **安全红线**：三道完整性闸（发布方 SHA256 强制、下载比对、安装前复验）全部
/// 沿用既有 update_download / update_run_installer 路径，静默路径零绕过分支；
/// CLI 任务进程（--task-run）永不参与自动更新（避免双进程竞争安装）。
#[tauri::command(async)]
pub fn update_auto_tick(
    app: AppHandle,
    state: State<'_, crate::state::AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<UpdateAutoTickResult, String> {
    use std::sync::atomic::Ordering;
    let r = |action: &str, detail: String| {
        Ok(UpdateAutoTickResult { action: action.to_string(), detail })
    };
    // CLI 任务进程排除（GUI 前端定时器不会在 CLI 进程运行，双保险）
    if std::env::args().nth(1).as_deref() == Some("--task-run") {
        return r("skipped_cli", "CLI 任务进程不参与自动更新".into());
    }
    // 设置开关（默认关）
    if !state.settings().auto_update_silent {
        return r("skipped_disabled", "静默自动更新未开启".into());
    }
    // 防重入
    if AUTO_TICK_BUSY.swap(true, Ordering::SeqCst) {
        return r("busy", "上一轮自动更新仍在进行".into());
    }
    let _guard = AutoTickGuard;
    // 空闲判定：网关在途请求为 0 才安装（未运行 / 状态锁中毒均视为空闲——
    // 安装会退出本进程，仅在途请求会被中断，故只挡 inflight > 0）
    match runtime.lock() {
        Ok(guard) => {
            if let Some(rt) = guard.as_ref() {
                if rt.shared.inflight.load(Ordering::Relaxed) > 0 {
                    return r("busy_gateway", "网关存在在途请求，已推迟到下一轮".into());
                }
            }
        }
        Err(_) => { /* 状态锁中毒：按空闲继续（安装即退出，无中断面） */ }
    }
    // ① 检查（网络失败归 error，不 Err——避免前端定时器持续报未捕获错误）
    let chk = match update_check() {
        Ok(c) => c,
        Err(e) => return r("error", format!("检查更新失败：{e}")),
    };
    if !chk.has_update {
        return r("no_update", format!("已是最新版本 {}", chk.current_version));
    }
    // ② 下载（三道闸前两道在此路径内）
    let dl = match update_download(
        app.clone(),
        chk.download_url.clone(),
        chk.asset_name.clone(),
        chk.latest_version.clone(),
        chk.sha256.clone(),
    ) {
        Ok(d) => d,
        Err(e) => return r("error", format!("下载 {} 失败：{e}", chk.latest_version)),
    };
    // 审查修复 P3：安装前二次空闲判定——下载耗时分钟级，期间新进的在途请求
    // 会被安装重启中断；把竞态窗口从整个下载时长压缩到判定→安装的毫秒级。
    // 本轮放弃的安装包留在更新缓存，下一轮/手动更新直接覆盖复用
    if let Ok(rt_guard) = runtime.lock() {
        if let Some(rt) = rt_guard.as_ref() {
            if rt.shared.inflight.load(Ordering::Relaxed) > 0 {
                return r("busy_gateway", "下载完成但网关出现新的在途请求，已推迟安装到下一轮".into());
            }
        }
    }
    // ③ 安装（第三道闸 + /R 装完自动重启；成功路径 emit update-installing 后进程退出）
    match update_run_installer(app.clone(), dl.file_path, dl.asset_name) {
        Ok(()) => r("installed", format!("已启动 {} 安装，应用将自动重启", dl.version)),
        Err(e) => r("error", format!("安装失败：{e}")),
    }
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

    /// D1：资产文件名白名单——既有发布流资产名全命中，cmd 元字符一律拒绝
    #[test]
    fn safe_asset_name_whitelist() {
        assert!(ensure_safe_asset_name("AI Work 助手_3.3.3_x64-setup.exe").is_ok());
        assert!(ensure_safe_asset_name("AI Work 助手_3.3.3_x64_zh-CN.msi").is_ok());
        assert!(ensure_safe_asset_name("AI.Work._3.3.3_x64-setup.exe").is_ok());
        assert!(ensure_safe_asset_name("setup(1).exe").is_ok());
        // cmd 元字符 / 引号 / 分隔符 / 控制字符一律拒绝
        for bad in [
            "x&calc.exe",
            "x|ver.exe",
            "x^1.exe",
            "x%PATH%.exe",
            "x!U!.exe",
            "a\"b.exe",
            "a<b.exe",
            "a>b.exe",
            "a;b.exe",
            "a'b.exe",
            "a`b.exe",
            "a\\b.exe",
            "a/b.exe",
            "a:b.exe",
            "a$b.exe",
            "a=b.exe",
            "a,cmd.exe",
            "a@x.exe",
            "x\t.exe",
            "x\n.exe",
        ] {
            assert!(ensure_safe_asset_name(bad).is_err(), "须拒绝: {bad:?}");
        }
        assert!(ensure_safe_asset_name("").is_err(), "空名拒绝");
    }

    /// P4（#135 审查收尾）：pick_asset 仅接受 NSIS——MSI 兜底是死路
    /// （update_run_installer 统一按 NSIS 参数启动，msiexec /P 为补丁模式），
    /// 仅 MSI 的 release 显式报「没有可用的安装包资产」引导手动下载。
    #[test]
    fn pick_asset_nsis_only() {
        let asset = |name: &str| {
            serde_json::json!({
                "name": name,
                "browser_download_url": format!("https://example.com/{name}"),
                "size": 1u64,
            })
        };
        // NSIS 优先：MSI 在场也不影响
        let (name, _, _) = pick_asset(&[
            asset("AI Work 助手_3.8.1_x64_zh-CN.msi"),
            asset("AI Work 助手_3.8.2_x64-setup.exe"),
        ])
        .unwrap();
        assert_eq!(name, "AI Work 助手_3.8.2_x64-setup.exe");
        // 仅 MSI → None（旧逻辑会选中并必然失败于 msiexec）
        assert!(pick_asset(&[asset("AI Work 助手_3.8.2_x64_zh-CN.msi")]).is_none());
        // 同类多资产取版本最高者
        let (name, _, _) = pick_asset(&[
            asset("AI Work 助手_3.8.1_x64-setup.exe"),
            asset("AI Work 助手_3.8.2_x64-setup.exe"),
        ])
        .unwrap();
        assert_eq!(name, "AI Work 助手_3.8.2_x64-setup.exe");
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

    /// M-6：下载 URL 单前缀锚——仅接受官方 GitHub Releases 资产直链
    #[test]
    fn download_url_prefix_gate() {
        assert!(is_trusted_download_url(
            "https://github.com/smart-open/TraeWorkAssistant/releases/download/v3.8.2/AI.Work._3.8.2_x64-setup.exe"
        ));
        // 非 https / 非官方域名 / 错仓库 / 错路径形态 / 空串一律拒绝
        for bad in [
            "http://github.com/smart-open/TraeWorkAssistant/releases/download/v1/x.exe",
            "https://evil.com/smart-open/TraeWorkAssistant/releases/download/v1/x.exe",
            "https://github.com/smart-open/OtherRepo/releases/download/v1/x.exe",
            "https://github.com/smart-open/TraeWorkAssistant/releases/expanded_assets/v1",
            "",
        ] {
            assert!(!is_trusted_download_url(bad), "须拒绝: {bad:?}");
        }
    }

    /// M-6 收口：URL 形态交叉校验——tag 与目标版本一致、末段与目标资产一致
    #[test]
    fn download_url_identity_gate() {
        let base = "https://github.com/smart-open/TraeWorkAssistant/releases/download/";
        // 正常形态：tag 带 v 前缀 / 裸版本号两种均接受
        for tag in ["v3.8.2", "3.8.2"] {
            assert!(verify_release_identity(
                &format!("{base}{tag}/AI Work 助手_3.8.2_x64-setup.exe"),
                "3.8.2",
                "AI Work 助手_3.8.2_x64-setup.exe",
            )
            .is_ok());
        }
        // tag 与目标版本不一致（旧 release 资产降级面）
        assert!(verify_release_identity(
            &format!("{base}v3.0.1/AI Work 助手_3.0.1_x64-setup.exe"),
            "3.8.2",
            "AI Work 助手_3.8.2_x64-setup.exe",
        )
        .is_err());
        // URL 末段与目标资产不一致
        assert!(verify_release_identity(
            &format!("{base}v3.8.2/AI.Work._3.8.2_x64-setup.exe"),
            "3.8.2",
            "AI Work 助手_3.8.2_x64-setup.exe",
        )
        .is_err());
        // 形态非法：缺文件名 / 多余路径段
        assert!(verify_release_identity(&format!("{base}v3.8.2/"), "3.8.2", "x").is_err());
        assert!(verify_release_identity(&format!("{base}v3.8.2/a/b.exe"), "3.8.2", "b.exe").is_err());
        // 前缀不命中（独立调用时的兜底）
        assert!(verify_release_identity("https://evil.com/x", "3.8.2", "x").is_err());
    }
}
