// ---------------- 应用自更新（检查 / 下载 / 安装，两步确认制） ----------------
//
// 数据源：GitHub Releases（api.github.com）。资产命名约定（见 scripts/rename_release.py）：
//   AI Work 助手_<ver>_x64-setup.exe   ← NSIS 安装包（首选，支持原地升级 + 老版迁移钩子）
//   AI Work 助手_<ver>_x64_zh-CN.msi   ← MSI（备选；仅同 identifier 的 3.x 间可原地升级）
//
// 流程（下载与安装拆分，UI 两处确认）：
//   update_check       解析最新 release 并与 CARGO_PKG_VERSION 比较；
//   update_download    下载资产到临时目录（emit update-download-progress），完成后返回文件路径，
//                      由前端确认后再安装（确认一：下载 / 确认二：安装）；
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

    // 收集本产品线候选 release：(版本, tag, html_url, assets)
    let mut candidates: Vec<((u64, u64, u64), String, String, Vec<serde_json::Value>)> =
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
                candidates.push((v, tag, html_url, assets));
            }
        }
    }
    // 本产品线内取版本最高者
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let min_ver =
        format!("{}.{}.{}", PRODUCT_MIN_VERSION.0, PRODUCT_MIN_VERSION.1, PRODUCT_MIN_VERSION.2);
    let (latest, tag, release_page, assets) = candidates
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

    let has_update = cmp_version(latest, current) == std::cmp::Ordering::Greater;
    Ok(UpdateCheckResult {
        has_update,
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        latest_version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        asset_name,
        download_url,
        size,
        release_page,
    })
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
fn validate_target(asset_name: &str, expected_version: &str) -> Result<(u64, u64, u64), String> {
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
) -> Result<UpdateDownloaded, String> {
    validate_target(&asset_name, &expected_version)?;

    // 下载目录：%TEMP%\ai-work-assistant-update\
    let dir = std::env::temp_dir().join("ai-work-assistant-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
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
