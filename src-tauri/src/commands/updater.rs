// ---------------- 应用自更新（检查 / 下载 / 静默安装） ----------------
//
// 数据源：GitHub Releases（api.github.com，本仓库 smart-open/TraeWorkAssistant）。
// 注意：仓库中 v3.x.x 是另一产品线的 release，本应用只关注 2.x.x 系列，
//       因此不能用 releases/latest（会被 3.x 遮蔽），必须拉取完整列表过滤取最大 2.x.x。
// 资产命名约定（Tauri NSIS 默认产物）：
//   Trae Work 助手_<ver>_x64-setup.exe   ← NSIS 安装包（首选，支持 /P 被动原地升级）
//   Trae Work 助手_<ver>_x64_zh-CN.msi   ← MSI（备选）
//
// 流程：update_check 拉取 releases 列表 → 过滤 2.x.x → 取最大版本与 CARGO_PKG_VERSION 比较；
//       同时下载该 release 的校验清单 latest.json（发布脚本 rename_release.py 生成并随 Release 上传），
//       取出安装包的发布方 SHA-256（fail-closed：清单缺失/损坏即阻止自动更新，引导手动下载）；
//       update_download 下载资产到临时目录（emit update-download-progress），
//       下载完成后与发布方 SHA-256 比对（不匹配即删除并报错），
//       update_run_installer 启动 NSIS 被动安装（/P /UPDATE /R），应用退出由安装器接管。

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const RELEASES_API: &str =
    "https://api.github.com/repos/smart-open/TraeWorkAssistant/releases?per_page=100";
const RELEASES_PAGE: &str = "https://github.com/smart-open/TraeWorkAssistant/releases";

/// 发布校验清单资产名（scripts/rename_release.py 生成，随 Release 上传）
const MANIFEST_ASSET: &str = "latest.json";

/// 下载临时目录（安装命令只允许执行此目录内的更新包，防止任意路径执行）
const UPDATE_TEMP_DIR: &str = "trae-work-assistant-update";

/// 最近一次成功下载的记录（路径 + 摘要），update_run_installer 用来校验
/// 前端回传的 path 确实来自本次下载且内容未被替换。
struct DownloadedUpdate {
    file_path: String,
    sha256_hex: String,
}
static LAST_DOWNLOAD: Mutex<Option<DownloadedUpdate>> = Mutex::new(None);

/// 计算文件 SHA-256（十六进制小写）
fn file_sha256(path: &std::path::Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开文件计算摘要失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读取文件计算摘要失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Serialize, Clone)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    /// 资产文件名，如 "Trae Work 助手_2.5.1_x64-setup.exe"
    pub asset_name: String,
    /// 资产下载直链（browser_download_url）
    pub download_url: String,
    /// 资产字节数
    pub size: u64,
    /// 发布方 SHA-256（取自 release 校验清单 latest.json，下载完成后强制比对）
    pub sha256: String,
    pub release_page: String,
}

#[derive(Serialize, Clone)]
struct DownloadProgress {
    received: u64,
    total: u64,
    percent: u64,
}

/// 仅允许 2.x.x 系列自更新（不跨大版本升级）
const SUPPORTED_MAJOR: u64 = 2;

/// 下载前置 TEMP 清理：移除本应用临时目录内的过期残留
/// （更新中断遗留的不完整安装包，避免长期占用磁盘）
fn cleanup_temp_dir() {
    let dir = std::env::temp_dir().join(UPDATE_TEMP_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|age| age > Duration::from_secs(7 * 24 * 3600))
            .unwrap_or(false);
        if stale {
            let p = entry.path();
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// 解析 "v2.5.1" / "2.5.1" → (2,5,1)。必须是严格的三段纯数字（2.x.x）。
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let t = s.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split('.');
    let a: u64 = it.next()?.trim().parse().ok()?;
    let b: u64 = it.next()?.trim().parse().ok()?;
    let c: u64 = it.next()?.trim().parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// 从资产文件名提取版本："Trae Work 助手_2.5.1_x64-setup.exe" → (2,5,1)。
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
/// 背景：ureq 只认环境变量代理、不认系统代理；下载 302 到 objects.githubusercontent.com
/// 在国内网络直连不通（os error 10060），必须借用户 VPN 通道。
#[cfg(target_os = "windows")]
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

#[cfg(not(target_os = "windows"))]
fn system_proxy_url() -> Option<String> {
    None
}

/// 按优先级构建尝试序列：系统代理（用户 VPN）→ 环境变量代理 → 直连。
/// 每项带标签，用于日志与报错文案；`finish` 为各场景的收尾超时配置。
fn attempt_agents(
    finish: impl Fn(ureq::AgentBuilder) -> ureq::Agent,
) -> Vec<(&'static str, ureq::Agent)> {
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
    out.push(("直连", finish(ureq::AgentBuilder::new())));
    out
}

fn fetch_releases() -> Result<Vec<serde_json::Value>, String> {
    // 检查是小请求：连接 10s + 整体 20s，逐通道尝试（系统代理 → 环境变量代理 → 直连）
    let mut last_err = String::new();
    for (label, agent) in attempt_agents(|b| {
        b.timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
    }) {
        match agent
            .get(RELEASES_API)
            .set("User-Agent", "trae-work-assistant-updater")
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
    // [(name, url, size, is_nsis)]
    let mut parsed: Vec<(String, String, u64, bool)> = Vec::new();
    for a in assets {
        let Ok(name) = a.get("name").and_then(|v| v.as_str()).ok_or(()) else {
            continue;
        };
        let Ok(url) = a
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .ok_or(())
        else {
            continue;
        };
        let size = a.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let is_nsis = name.ends_with("_x64-setup.exe");
        let is_msi = name.ends_with("_x64_zh-CN.msi");
        if is_nsis || is_msi {
            parsed.push((name.to_string(), url.to_string(), size, is_nsis));
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

/// 发布校验清单结构（scripts/rename_release.py 生成并随 Release 上传）：
/// `{ "version": "2.9.2", "assets": { "<资产文件名>": "<sha256hex>" } }`
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
/// 如 "Trae Work 助手_2.9.2_x64-setup.exe" → "Trae.Work._2.9.2_x64-setup.exe"），
/// 清单键保存的是本地文件名，用宽松键对两侧归一后即可匹配。
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

/// 下载并解析 release 的校验清单，返回目标资产的发布方 SHA-256。
/// fail-closed：清单缺失、版本不符、损坏或未收录该资产时返回 Err 阻止自动更新（引导手动下载）。
fn fetch_manifest_hash(
    assets: &[serde_json::Value],
    asset_name: &str,
    expected_ver: (u64, u64, u64),
) -> Result<String, String> {
    let url = assets
        .iter()
        .filter_map(|a| {
            let name = a.get("name").and_then(|v| v.as_str())?;
            (name == MANIFEST_ASSET)
                .then(|| a.get("browser_download_url").and_then(|v| v.as_str()))
                .flatten()
        })
        .next()
        .ok_or_else(|| {
            format!("新版本缺少校验清单（{MANIFEST_ASSET}），为防安装包被篡改已阻止自动更新。请手动下载：{RELEASES_PAGE}")
        })?;

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
            .set("User-Agent", "trae-work-assistant-updater")
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
    let text = text
        .ok_or_else(|| format!("下载校验清单失败（{last_err}），请重试或手动下载：{RELEASES_PAGE}"))?;

    let manifest: UpdateManifest = serde_json::from_str(&text).map_err(|e| {
        format!("校验清单损坏（{MANIFEST_ASSET} 解析失败: {e}），已阻止自动更新。请手动下载：{RELEASES_PAGE}")
    })?;
    if parse_version(&manifest.version) != Some(expected_ver) {
        return Err(format!(
            "校验清单版本（{}）与目标版本不一致，已阻止自动更新。请手动下载：{RELEASES_PAGE}",
            manifest.version
        ));
    }
    lookup_manifest_hash(&manifest, asset_name)
        .ok_or_else(|| format!("校验清单中未收录该安装包的 SHA-256，已阻止自动更新。请手动下载：{RELEASES_PAGE}"))
}

/// 检查 GitHub Releases 上的最新 2.x.x 版本，与当前应用版本比较。
/// 仓库中 v3.x.x 是另一产品线，直接忽略（不能用 releases/latest，会被 3.x 遮蔽）。
/// async 派发：网络重试最坏 90s（3 通道 × 30s），同步命令默认跑主线程会冻住 UI。
#[tauri::command(async)]
pub fn update_check() -> Result<UpdateCheckResult, String> {
    let current = parse_version(env!("CARGO_PKG_VERSION")).ok_or("内置版本号解析失败")?;
    let releases = fetch_releases()?;

    // 遍历全部 release，保留 2.x.x 系列，取版本最大者。
    // 版本回填：release 资产名版本 < tag 版本时，说明打包时产品版本未跟上 tag
    // （如 tag v2.8.2 而资产仍为 2.8.1），此时以 tag 为准重新命名资产，
    // 避免下载/安装校验因「资产版本 <= 当前版本」被拒
    let mut best: Option<((u64, u64, u64), String, &serde_json::Value)> = None;
    for rel in &releases {
        let tag = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        let assets = rel
            .get("assets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // 版本号优先取 tag（v2.5.1）；tag 不合法时从资产名推导
        let Some(tag_ver) = parse_version(tag) else {
            continue;
        };
        if tag_ver.0 != SUPPORTED_MAJOR {
            continue;
        }
        // 资产名中解析出的最大版本（tag 不合法时无法进入此处，仅作回填依据）
        let asset_ver = assets
            .iter()
            .filter_map(|a| a.get("name").and_then(|v| v.as_str()))
            .filter_map(version_from_asset)
            .max()
            .unwrap_or(tag_ver);
        let eff_ver = asset_ver.max(tag_ver);
        if best
            .as_ref()
            .map_or(true, |(v, _, _)| cmp_version(eff_ver, *v) == std::cmp::Ordering::Greater)
        {
            best = Some((eff_ver, tag.to_string(), rel));
        }
    }
    let (latest, tag, rel) = best.ok_or_else(|| {
        format!("Releases 列表中没有 2.x.x 版本。可手动查看：{RELEASES_PAGE}")
    })?;

    let assets = rel
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let (mut asset_name, download_url, mut size) = pick_asset(&assets).ok_or_else(|| {
        format!(
            "最新 2.x.x release（{tag}）中没有可用的安装包资产。可手动查看：{RELEASES_PAGE}"
        )
    })?;

    // 发布方完整性校验（S1 insecure_update 纵深防御）：
    // 下载校验清单 latest.json 取该资产的发布方 SHA-256，fail-closed——
    // 清单缺失/损坏/未收录时直接报错阻止自动更新，引导手动下载。
    // 必须在回填重命名之前查找：哈希绑定的是 release 里的原始资产文件名。
    let expected_sha = fetch_manifest_hash(&assets, &asset_name, latest)?;

    // 版本回填：资产名版本低于 tag 版本 → 按目标版本重命名资产名，
    // 下载时写入临时目录的文件名随之更新，版本前置校验才能通过。
    // 注意必须「替换」倒数第二段版本段（而非追加后缀）：
    // version_from_asset 固定按下划线倒数第二段解析，
    // 追加成 `X_2.8.1_x64-setup_2.8.2.exe` 会使解析段变为 x64-setup 而失败
    if let Some(asset_ver) = version_from_asset(&asset_name) {
        if cmp_version(asset_ver, latest) == std::cmp::Ordering::Less {
            let mut parts: Vec<String> =
                asset_name.split('_').map(|s| s.to_string()).collect();
            // version_from_asset 已保证至少 2 段
            let n = parts.len();
            parts[n - 2] = format!("{}.{}.{}", latest.0, latest.1, latest.2);
            let renamed = parts.join("_");
            // 大小不可靠（资产是旧版本产物），置 0 让前端以未知大小处理
            size = 0;
            log::warn!(
                "release {tag} 资产版本低于 tag 版本，已回填资产名: {asset_name} -> {renamed}（size 置 0）"
            );
            asset_name = renamed;
        }
    }

    // 仅在 2.x.x 系列内自更新（best 已过滤大版本，此处双保险）
    let has_update = cmp_version(latest, current) == std::cmp::Ordering::Greater;
    Ok(UpdateCheckResult {
        has_update,
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        latest_version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        asset_name,
        download_url,
        size,
        sha256: expected_sha,
        release_page: RELEASES_PAGE.to_string(),
    })
}

/// 下载更新包到临时目录（仅下载，不安装），返回落盘路径。
/// 进度经 update-download-progress 事件回推；安装由前端确认后调用 update_run_installer。
/// `expected_sha256` 为发布方清单（latest.json）给出的安装包摘要，下载完成后强制比对，
/// 不匹配即删除并报错（S1 insecure_update：更新包完整性校验，fail-closed）。
/// async 派发：下载耗时不可控（最坏 3 通道各连+读超时），同步命令跑主线程会冻住 UI。
#[tauri::command(async)]
pub fn update_download(
    app: AppHandle,
    download_url: String,
    asset_name: String,
    expected_version: String,
    expected_sha256: String,
) -> Result<String, String> {
    // 防御：发布方摘要必须为合法 SHA-256 格式（update_check 已 fail-closed 保证存在，此处双保险）
    let expected_sha = expected_sha256.trim().to_ascii_lowercase();
    if !is_sha256_hex(&expected_sha) {
        return Err("发布方 SHA-256 缺失或格式非法，已中止下载。请重新检查更新，或手动下载安装".to_string());
    }

    // 防御：资产名里的版本必须与检查结果一致，且大于当前版本
    let asset_ver = version_from_asset(&asset_name)
        .ok_or_else(|| format!("资产名无法解析版本号: {asset_name}"))?;
    let expected = parse_version(&expected_version).ok_or("目标版本号解析失败")?;
    if asset_ver != expected {
        return Err(format!(
            "资产版本 {asset_ver:?} 与检查到的目标版本 {expected:?} 不一致，已中止"
        ));
    }
    let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap();
    if cmp_version(asset_ver, current) != std::cmp::Ordering::Greater {
        return Err("目标版本不大于当前版本，无需更新".to_string());
    }

    // 防御：资产名只允许纯文件名（禁止路径分隔符/父目录分量，防 join 逃逸临时目录）
    if asset_name.contains(['/', '\\']) || asset_name == ".." || asset_name.contains("..") {
        return Err(format!("非法的资产文件名: {asset_name}"));
    }

    // 下载目录：%TEMP%\trae-work-assistant-update\
    let dir = std::env::temp_dir().join(UPDATE_TEMP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    // 顺手清理过期残留（更新中断遗留的不完整安装包，7 天以上才删）
    cleanup_temp_dir();
    let dest = dir.join(&asset_name);
    // 清理同名旧文件（可能是不完整下载）
    let _ = std::fs::remove_file(&dest);

    // 逐通道尝试下载（系统代理 → 环境变量代理 → 直连）。
    // 超时策略：连接 10s + 读 60s，不设整体超时（大文件慢速下载不能被整体超时掐断）。
    // 每次尝试都从 0 重新流式写盘并重发进度事件（进度条回跳属预期）。
    let mut last_err = String::new();
    for (label, agent) in attempt_agents(|b| {
        b.timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build()
    }) {
        match download_via(&app, &agent, &download_url, &dest) {
            Ok(received) => {
                // 完整性校验：先与发布方清单（latest.json）比对——不匹配即删除并中止；
                // 通过后计算摘要记录，安装时再二次校验（防下载后被替换）
                let sha = file_sha256(&dest).map_err(|e| {
                    let _ = std::fs::remove_file(&dest);
                    e
                })?;
                if sha != expected_sha {
                    let _ = std::fs::remove_file(&dest);
                    return Err(format!(
                        "更新包校验失败（SHA-256 与发布清单不一致），已拒绝安装并删除下载文件。\n请重试或手动下载：{RELEASES_PAGE}"
                    ));
                }
                if let Ok(mut last) = LAST_DOWNLOAD.lock() {
                    *last = Some(DownloadedUpdate {
                        file_path: dest.to_string_lossy().to_string(),
                        sha256_hex: sha,
                    });
                }
                let _ = app.emit(
                    "update-download-progress",
                    DownloadProgress {
                        received,
                        total: received,
                        percent: 100,
                    },
                );
                return Ok(dest.to_string_lossy().to_string());
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
        .set("User-Agent", "trae-work-assistant-updater")
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
                DownloadProgress {
                    received,
                    total,
                    percent,
                },
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

/// 启动更新安装器并退出应用。
/// 安装器以被动模式运行（/P：仅显示进度条、不弹任何询问），/UPDATE 覆盖安装不卸载，
/// /R 安装成功后自动重启应用（见 NSIS 模板 .onInstSuccess）。
/// 安全约束：path 必须指向下载临时目录内的文件，且摘要与最近一次下载记录一致
/// （防止 webview 被注入后借本命令执行任意路径的程序）。
#[tauri::command]
pub fn update_run_installer(app: AppHandle, path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if !p.is_file() {
        return Err(format!("更新包不存在，请重新下载：{path}"));
    }
    // 路径绑定：必须是本应用的下载临时目录
    let allowed_dir = std::env::temp_dir().join(UPDATE_TEMP_DIR);
    let parent_ok = p
        .parent()
        .map(|d| d == allowed_dir)
        .unwrap_or(false);
    if !parent_ok {
        return Err("更新包位置异常（不在下载临时目录内），已拒绝安装".to_string());
    }
    // 完整性绑定：与最近一次下载的 SHA-256 比对，防下载后被替换
    {
        let last = LAST_DOWNLOAD
            .lock()
            .map_err(|_| "内部状态异常，请重启应用后重试".to_string())?;
        let Some(rec) = last.as_ref() else {
            return Err("未找到本次会话的下载记录，请先在「检查更新」中下载更新包".to_string());
        };
        if rec.file_path != path {
            return Err("更新包与最近下载记录不一致，请重新下载".to_string());
        }
        let now_sha = file_sha256(p)?;
        if now_sha != rec.sha256_hex {
            return Err("更新包校验失败（内容与下载时不一致），已拒绝安装".to_string());
        }
    }

    std::process::Command::new(&path)
        .args(["/P", "/UPDATE", "/R"])
        .spawn()
        .map_err(|e| format!("启动安装程序失败: {e}（可手动运行：{path}）"))?;

    // 提示前端后退出，让安装器接管
    let _ = app.emit(
        "update-installing",
        std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
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
            "version": "2.9.2",
            "assets": {
                "Trae Work 助手_2.9.2_x64-setup.exe": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "Trae Work 助手_2.9.2_x64_portable.zip": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
            }
        }"#;
        let m: UpdateManifest = serde_json::from_str(text).unwrap();
        assert_eq!(parse_version(&m.version), Some((2, 9, 2)));
        // 清单键为本地文件名（含空格），按原样可精确命中
        let h = lookup_manifest_hash(&m, "Trae Work 助手_2.9.2_x64-setup.exe").unwrap();
        assert_eq!(h, "a".repeat(64));
        // 未收录 / 哈希格式非法 → None
        assert!(lookup_manifest_hash(&m, "Trae Work 助手_2.9.2_x64_zh-CN.msi").is_none());
        assert!(lookup_manifest_hash(&m, "不存在.exe").is_none());
    }

    #[test]
    fn manifest_lookup_normalizes_github_asset_name() {
        // GitHub 上传会重写资产名：空格 → '.'、非 ASCII（助手）→ '_'
        let text = r#"{
            "version": "2.9.2",
            "assets": {
                "Trae Work 助手_2.9.2_x64-setup.exe": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            }
        }"#;
        let m: UpdateManifest = serde_json::from_str(text).unwrap();
        let gh_name = "Trae.Work._2.9.2_x64-setup.exe";
        let h = lookup_manifest_hash(&m, gh_name).unwrap();
        assert_eq!(h, "a".repeat(64));
        // 大小写归一：GitHub 侧哈希为大写时同样命中
        let text2 = r#"{
            "version": "2.9.2",
            "assets": {
                "Trae Work 助手_2.9.2_x64-setup.exe": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }"#;
        let m2: UpdateManifest = serde_json::from_str(text2).unwrap();
        assert_eq!(lookup_manifest_hash(&m2, gh_name).unwrap(), "a".repeat(64));
        // 无论清单侧大小写，输出统一小写
        assert_eq!(
            lookup_manifest_hash(&m, gh_name).unwrap(),
            lookup_manifest_hash(&m2, gh_name).unwrap()
        );
    }

    #[test]
    fn manifest_version_mismatch_detected() {
        let text = r#"{ "version": "2.8.2", "assets": {} }"#;
        let m: UpdateManifest = serde_json::from_str(text).unwrap();
        assert_ne!(parse_version(&m.version), Some((2, 9, 2)));
    }
}
