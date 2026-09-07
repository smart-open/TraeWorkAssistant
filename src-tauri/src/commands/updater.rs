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
//       update_download 下载资产到临时目录（emit update-download-progress），
//       update_run_installer 启动 NSIS 被动安装（/P /UPDATE /R），应用退出由安装器接管。

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
    /// 资产文件名，如 "Trae Work 助手_2.5.1_x64-setup.exe"
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

/// 仅允许 2.x.x 系列自更新（不跨大版本升级）
const SUPPORTED_MAJOR: u64 = 2;

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

/// 构建带超时的 ureq Agent（默认直连；若系统设置了 HTTPS_PROXY/HTTP_PROXY 环境变量则走代理）。
fn build_agent(timeout: Duration) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new().timeout(timeout);
    if let Ok(p) = std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .or_else(|_| std::env::var("http_proxy"))
    {
        if let Ok(proxy) = ureq::Proxy::new(&p) {
            builder = builder.proxy(proxy);
        }
    }
    builder.build()
}

fn fetch_releases() -> Result<Vec<serde_json::Value>, String> {
    let agent = build_agent(Duration::from_secs(20));
    let resp = agent
        .get(RELEASES_API)
        .set("User-Agent", "trae-work-assistant-updater")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| {
            format!(
                "无法访问 GitHub Releases（{}）。\n请检查网络或代理后重试；也可手动打开发布页下载：{}",
                e, RELEASES_PAGE
            )
        })?;
    resp.into_json::<Vec<serde_json::Value>>()
        .map_err(|e| format!("解析 release 响应失败: {e}"))
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

/// 检查 GitHub Releases 上的最新 2.x.x 版本，与当前应用版本比较。
/// 仓库中 v3.x.x 是另一产品线，直接忽略（不能用 releases/latest，会被 3.x 遮蔽）。
#[tauri::command]
pub fn update_check() -> Result<UpdateCheckResult, String> {
    let current = parse_version(env!("CARGO_PKG_VERSION")).ok_or("内置版本号解析失败")?;
    let releases = fetch_releases()?;

    // 遍历全部 release，保留 2.x.x 系列，取版本最大者
    let mut best: Option<((u64, u64, u64), &serde_json::Value)> = None;
    for rel in &releases {
        let tag = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        let assets = rel
            .get("assets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // 版本号优先取 tag（v2.5.1）；tag 不合法时从资产名推导
        let ver_from_assets = || {
            assets
                .iter()
                .filter_map(|a| a.get("name").and_then(|v| v.as_str()))
                .find_map(version_from_asset)
        };
        let Some(ver) = parse_version(tag).or_else(ver_from_assets) else {
            continue;
        };
        if ver.0 != SUPPORTED_MAJOR {
            continue;
        }
        if best.map_or(true, |(v, _)| cmp_version(ver, v) == std::cmp::Ordering::Greater) {
            best = Some((ver, rel));
        }
    }
    let (latest, rel) = best.ok_or_else(|| {
        format!("Releases 列表中没有 2.x.x 版本。可手动查看：{RELEASES_PAGE}")
    })?;

    let tag = rel
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let assets = rel
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let (asset_name, download_url, size) = pick_asset(&assets).ok_or_else(|| {
        format!(
            "最新 2.x.x release（{tag}）中没有可用的安装包资产。可手动查看：{RELEASES_PAGE}"
        )
    })?;

    // 仅在 2.x.x 系列内自更新（best 已过滤大版本，此处双保险）
    let has_update = cmp_version(latest, current) == std::cmp::Ordering::Greater;
    Ok(UpdateCheckResult {
        has_update,
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        latest_version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        asset_name,
        download_url,
        size,
        release_page: RELEASES_PAGE.to_string(),
    })
}

/// 下载更新包到临时目录（仅下载，不安装），返回落盘路径。
/// 进度经 update-download-progress 事件回推；安装由前端确认后调用 update_run_installer。
#[tauri::command]
pub fn update_download(
    app: AppHandle,
    download_url: String,
    asset_name: String,
    expected_version: String,
) -> Result<String, String> {
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

    // 下载目录：%TEMP%\trae-work-assistant-update\
    let dir = std::env::temp_dir().join("trae-work-assistant-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let dest = dir.join(&asset_name);
    // 清理同名旧文件（可能是不完整下载）
    let _ = std::fs::remove_file(&dest);

    // 下载（不做整体超时，只限制连接超时；流式写盘 + 进度事件）
    let agent = build_agent(Duration::from_secs(30));
    let resp = agent
        .get(&download_url)
        .set("User-Agent", "trae-work-assistant-updater")
        .call()
        .map_err(|e| format!("下载安装包失败（{}）。可手动下载：{}", e, RELEASES_PAGE))?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let file = std::fs::File::create(&dest).map_err(|e| format!("创建安装包文件失败: {e}"))?;
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
    let _ = app.emit(
        "update-download-progress",
        DownloadProgress {
            received,
            total,
            percent: 100,
        },
    );
    Ok(dest.to_string_lossy().to_string())
}

/// 启动更新安装器并退出应用。
/// 安装器以被动模式运行（/P：仅显示进度条、不弹任何询问），/UPDATE 覆盖安装不卸载，
/// /R 安装成功后自动重启应用（见 NSIS 模板 .onInstSuccess）。
#[tauri::command]
pub fn update_run_installer(app: AppHandle, path: String) -> Result<(), String> {
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("更新包不存在，请重新下载：{path}"));
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
