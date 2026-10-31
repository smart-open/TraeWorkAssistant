//! exe 六级发现（原 PS Find-TraeExe 210-342 对译）。
//!
//! 顺序原则（修复「首次切换误用 Trae CN.exe」）：旧逻辑把「运行中进程」作为最高
//! 优先级，导致残留/错误的 Trae 进程（如旧的 Trae CN.exe）被优先采用，从而启动
//! 错误的 exe。现改为：
//!   1) 用户显式配置 > 2) 候选路径 > 3) 开始菜单/桌面 lnk > 4) 注册表
//!   > 5) 运行中进程（最后回退）> 6) 进程缓存（兜底，仅自定义安装且当前未运行时）
//! 这样正常情况下总是解析到用户真实安装的应用，而非被残留进程带偏。
//!
//! F-75 M1-1.2 macOS 定位链（bundle 制，应用发现模型与 Windows 完全不同）：
//!   1) 用户显式配置（settings_path_key，mac 形态为 .app 目录）> 2) bundle 探测
//!   （/Applications、~/Applications，Info.plist CFBundleExecutable 白名单防串台）
//!   > 3) Spotlight mdfind 兜底 > 4) 运行中进程回退（sysinfo exe → .app 根归一）。
//!   启动走 `open`（LaunchServices，见 proc.rs start_app mac 分支）。

use std::path::{Path, PathBuf};

#[cfg(windows)] // Windows 六级发现专用（mac bundle 定位链不消费）
use super::{glob_match_ci, profile};
use super::Session;

pub fn find_exe(sess: &mut Session) -> Option<PathBuf> {
    // F-75 M1-1.2：mac 走 bundle 定位链（与 Windows 六级发现完全分派）
    #[cfg(target_os = "macos")]
    {
        return find_bundle(sess);
    }

    #[cfg(not(target_os = "macos"))]
    {
        // 1) 用户显式配置：kv `app_settings` → settings_path_key（最高优先级；SQLite 化 P2）
        let settings: serde_json::Value = crate::store::db(&sess.data_dir).kv_get("app_settings");
        if let Some(p) = settings.get(sess.prof.settings_path_key).and_then(|v| v.as_str()) {
            let p = PathBuf::from(p);
            if p.is_file() {
                sess.exe_cache = Some(p.clone());
                return Some(p);
            }
        }

        // 2) 多候选路径探测（与 commands/env.rs 探测列表保持一致）
        for c in &sess.prof.exe_candidates {
            if c.is_file() {
                sess.exe_cache = Some(c.clone());
                return Some(c.clone());
            }
        }

        // 3) .lnk 快捷方式（开始菜单×2 / 桌面×2，递归；lnk crate 纯 Rust 解析，
        //    替代 WScript.Shell COM）。文件名 glob 匹配大小写不敏感（PS -like 语义，
        //    如 *Doubao* 须命中 doubao.lnk）
        for dir in lnk_dirs() {
            for lnk in walk_lnk_files(&dir) {
                let Some(name) = lnk.file_name().and_then(|n| n.to_str()) else { continue };
                if !sess.prof.lnk_patterns.iter().any(|p| glob_match_ci(p, name)) {
                    continue;
                }
                if let Some(target) = lnk_target(&lnk) {
                    let p = PathBuf::from(&target);
                    if p.is_file() && profile::exe_matches(&p, &sess.prof) {
                        sess.exe_cache = Some(p.clone());
                        return Some(p);
                    }
                }
            }
        }

        // 4) 注册表回退（HKLM 64/32 + HKCU Uninstall；DisplayName 匹配 →
        //    DisplayIcon / InstallLocation+exe_names 组合探测）
        if let Some(p) = registry_locate(sess) {
            if p.is_file() {
                sess.exe_cache = Some(p.clone());
                return Some(p);
            }
        }

        // 5) 运行中进程（最后回退）：仅当以上都找不到才用，避免残留/错误进程误导启动
        //    路径。匹配用 proc_patterns 通配组，再经 exe_names 白名单防串台——与 Stop
        //    的 proc_names 精确组刻意双轨（PS 同款）
        if let Some(p) = super::proc::running_exe_of(&sess.prof) {
            if p.is_file() && profile::exe_matches(&p, &sess.prof) {
                sess.exe_cache = Some(p.clone());
                return Some(p);
            }
        }

        // 6) 进程缓存兜底（自定义安装、当前未运行、以上均未命中）
        sess.exe_cache.clone().filter(|p| p.is_file())
    }
}

// ── macOS bundle 定位链（F-75 M1-1.2） ──────────────────────────────────────

/// bundle 候选名：显示名 + exe 主干名（剥 .exe 后缀）
#[cfg(target_os = "macos")]
fn bundle_name_candidates(sess: &Session) -> Vec<&str> {
    let mut names = vec![sess.prof.app_name];
    for exe in sess.prof.exe_names {
        if let Some(stem) = exe.strip_suffix(".exe") {
            names.push(stem);
        }
    }
    names
}

#[cfg(target_os = "macos")]
fn find_bundle(sess: &mut Session) -> Option<PathBuf> {
    // 1) 用户显式配置：settings_path_key 在 mac 形态为 .app 目录路径
    let settings: serde_json::Value = crate::store::db(&sess.data_dir).kv_get("app_settings");
    if let Some(p) = settings.get(sess.prof.settings_path_key).and_then(|v| v.as_str()) {
        let p = PathBuf::from(p);
        if is_bundle_dir(&p) {
            sess.exe_cache = Some(p.clone());
            return Some(p);
        }
    }

    // 2) bundle 探测：候选名 = 显示名 + exe 名主干（豆包等中文名应用 mac bundle 名待侦察）
    for name in bundle_name_candidates(sess) {
        if let Some(app) = locate_bundle_dir(name, sess.prof.exe_names, sess.prof.mac_bundle_ids) {
            sess.exe_cache = Some(app.clone());
            return Some(app);
        }
    }

    // 3) Spotlight 兜底（mdfind 秒回；仅上面未命中时）：
    //    候选名与 bundle 探测同源（审查 P2：原只查显示名，漏掉安装目录名与
    //    显示名不一致的应用，如英文名目录 + 中文名显示）
    for name in bundle_name_candidates(sess) {
        if let Some(app) = mdfind_bundle(name, sess.prof.exe_names, sess.prof.mac_bundle_ids) {
            sess.exe_cache = Some(app.clone());
            return Some(app);
        }
    }

    // 4) 运行中进程回退：sysinfo exe() 返回 <App>.app/Contents/MacOS/<exe>，
    //    归一回 .app 根后经白名单防串台。
    //    M-1 已知局限（helper 进程局限）：仅辅助进程在跑（主进程已退）时，
    //    running_exe_of 可能命中 <App> Helper.app 内层 bundle——bundle_root_of
    //    归一到 Helper 包、白名单不匹配被拒 → 进程回退失准；主进程在跑时无此问题
    //    （M-1 ⑥ 后 running_exe_of mac 分支仅枚举主进程，本局限已消除）
    if let Some(exe) = super::proc::running_exe_of(&sess.prof) {
        if let Some(app) = bundle_root_of(&exe) {
            if bundle_exe_matches(&app, sess.prof.exe_names, sess.prof.mac_bundle_ids) {
                sess.exe_cache = Some(app);
                return sess.exe_cache.clone();
            }
        }
    }

    // 5) 进程缓存兜底（2026-10-19 缓存投毒防护：补 bundle 白名单复检——缓存
    //    可能来自 stop_app 停前缓存的进程 exe（Contents/MacOS 二进制路径）或
    //    bundle 定位链的 .app 根，归一到 bundle 根后验身份白名单，防串台 bundle
    //    缓存被直接放行（仅装国际版机器上国内切换 open 拉起国际客户端））
    sess.exe_cache.clone().filter(|p| {
        p.exists() && cached_bundle_ok(p, sess.prof.exe_names, sess.prof.mac_bundle_ids)
    })
}

/// 第 5 级缓存兜底的 bundle 白名单复检：缓存形态可为 .app 根（bundle 定位链）
/// 或主进程二进制路径（stop_app 停前缓存，Contents/MacOS/<exe>），统一归一到
/// bundle 根后验双信号白名单（CFBundleExecutable / CFBundleIdentifier）
#[cfg(target_os = "macos")]
fn cached_bundle_ok(p: &Path, exe_names: &[&str], bundle_ids: &[&str]) -> bool {
    if is_bundle_dir(p) {
        bundle_exe_matches(p, exe_names, bundle_ids)
    } else {
        bundle_root_of(p).map_or(false, |r| bundle_exe_matches(&r, exe_names, bundle_ids))
    }
}

/// 是否为 .app bundle 目录
#[cfg(target_os = "macos")]
pub(crate) fn is_bundle_dir(p: &Path) -> bool {
    p.is_dir() && p.extension().map(|e| e == "app").unwrap_or(false)
}

/// 从 Contents/MacOS/... exe 路径归一回 .app bundle 根
#[cfg(target_os = "macos")]
fn bundle_root_of(exe: &Path) -> Option<PathBuf> {
    // ancestors 语义：nth(0)=exe 自身 → nth(1)=MacOS → nth(2)=Contents → nth(3)=<App>.app
    let app = exe.ancestors().nth(3)?;
    if is_bundle_dir(app) {
        Some(app.to_path_buf())
    } else {
        None
    }
}

/// bundle 防串台：Info.plist 身份 ∈ 白名单。双信号（M-1 侦察 ⑥：四应用
/// CFBundleExecutable 均为 "Electron" 不可区分，CFBundleIdentifier 为主身份信号）：
///   ① CFBundleExecutable ∈ exe_names（剥 .exe 后缀，原语义保留）；
///   ② CFBundleIdentifier ∈ bundle_ids（mac_bundle_ids 档案字段；Helper 内层
///     bundle id 带 ".helper" 后缀，如 cn.trae.app.helper，不会被 ② 误放行）
#[cfg(target_os = "macos")]
pub(crate) fn bundle_exe_matches(app: &Path, exe_names: &[&str], bundle_ids: &[&str]) -> bool {
    if let Some(exec) = read_info_plist_value(app, "CFBundleExecutable") {
        if exe_names
            .iter()
            .any(|e| e.strip_suffix(".exe").unwrap_or(e).eq_ignore_ascii_case(&exec))
        {
            return true;
        }
    }
    !bundle_ids.is_empty()
        && read_info_plist_value(app, "CFBundleIdentifier")
            .map(|id| bundle_ids.iter().any(|b| b.eq_ignore_ascii_case(&id)))
            .unwrap_or(false)
}

/// 解析 Contents/Info.plist 指定 key 的首个 `<string>…</string>` 值（纯文本 XML
/// plist 宽容解析）。F-75 P2-2 起开放给 env.rs（app_locate mac 分派读
/// CFBundleShortVersionString 版本号）。
/// M-1 已知局限（bplist 二进制风险）：部分应用 Info.plist 为 binary1 格式，
/// read_to_string 得到乱码 → 解析 None → 调用方按缺失处理（白名单拒绝/版本为空）；
/// M-1 真机侦察确认后如需支持，改用 plutil -convert xml1 或 plist crate。
#[cfg(target_os = "macos")]
pub(crate) fn read_info_plist_value(app: &Path, key: &str) -> Option<String> {
    let plist = std::fs::read_to_string(app.join("Contents").join("Info.plist")).ok()?;
    let key_tag = format!("<key>{key}</key>");
    let key_pos = plist.find(&key_tag)?;
    let rest = &plist[key_pos + key_tag.len()..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    let v = rest[start..end].trim();
    if v.is_empty() { None } else { Some(v.to_string()) }
}

/// bundle 探测：/Applications/<Name>.app、~/Applications/<Name>.app
#[cfg(target_os = "macos")]
pub(crate) fn locate_bundle_dir(name: &str, exe_names: &[&str], bundle_ids: &[&str]) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let bases = [
        PathBuf::from("/Applications"),
        PathBuf::from(format!("{home}/Applications")),
    ];
    for base in bases {
        let app = base.join(format!("{name}.app"));
        if is_bundle_dir(&app) && bundle_exe_matches(&app, exe_names, bundle_ids) {
            return Some(app);
        }
    }
    None
}

/// Spotlight 兜底：mdfind 查 .app，逐个经白名单防串台
#[cfg(target_os = "macos")]
pub(crate) fn mdfind_bundle(name: &str, exe_names: &[&str], bundle_ids: &[&str]) -> Option<PathBuf> {
    use std::time::Duration;
    // 审查修复（Medium）：Spotlight 索引异常时 mdfind 可长时间挂起，无超时会卡死
    // 切换链——sys_output_timeout 3s 兜底强杀（常态秒回，3s 足够宽裕）
    const MDFIND_TIMEOUT: Duration = Duration::from_secs(3);
    // 逐应用目录查询（用户级 ~/Applications 与系统级 /Applications 同等常见）
    let mut roots = vec!["/Applications".to_string()];
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        roots.push(format!("{home}/Applications"));
    }
    for root in roots {
        let out = match crate::platform::cmd::sys_output_timeout(
            crate::platform::cmd::sys_command("mdfind").args([
                &format!("kMDItemKind == 'Application' && kMDItemDisplayName == '*{name}*'cd"),
                "-onlyin",
                &root,
            ]),
            MDFIND_TIMEOUT,
        ) {
            Ok(s) => s,
            // 超时/启动失败：跳过该 root 继续下一个（原 .output().ok()? 会整体放弃，
            // 连用户级目录都不再尝试）
            Err(_) => continue,
        };
        let text = out;
        for line in text.lines() {
            let p = PathBuf::from(line.trim());
            if is_bundle_dir(&p) && bundle_exe_matches(&p, exe_names, bundle_ids) {
                return Some(p);
            }
        }
    }
    None
}

// ── Windows 六级发现（原实现原样保留，mac 不编译） ──────────────────────────

/// .lnk 搜索目录（PS 241-246：开始菜单×2 / 桌面×2）
#[cfg(windows)]
fn lnk_dirs() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    vec![
        PathBuf::from(format!(
            "{}\\Microsoft\\Windows\\Start Menu\\Programs",
            env("APPDATA")
        )),
        PathBuf::from(format!(
            "{}\\Microsoft\\Windows\\Start Menu\\Programs",
            env("ProgramData")
        )),
        PathBuf::from(format!("{}\\Desktop", env("USERPROFILE"))),
        PathBuf::from(format!("{}\\Desktop", env("PUBLIC"))),
    ]
    .into_iter()
    .filter(|d| d.is_dir())
    .collect()
}

/// 递归收集目录下全部 .lnk（PS Get-ChildItem -Recurse -Filter *.lnk -ErrorAction
/// SilentlyContinue 对译：遍历失败静默跳过）
#[cfg(windows)]
fn walk_lnk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk_lnk_files(&p));
        } else if p.extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            out.push(p);
        }
    }
    out
}

/// .lnk → TargetPath（lnk crate；local_base_path 与 unicode 变体双回退；
/// 解析失败按 PS 同语义静默跳过）
#[cfg(windows)]
fn lnk_target(path: &Path) -> Option<String> {
    let link = lnk::ShellLink::open(path, lnk::encoding::WINDOWS_1252).ok()?;
    let info = link.link_info().as_ref()?;
    if let Some(p) = info.local_base_path() {
        return Some(p.to_string());
    }
    if let Some(p) = info.local_base_path_unicode() {
        return Some(p.clone());
    }
    None
}

/// 注册表定位（windows-registry；替代 PS Get-ItemProperty 三根枚举）：
/// HKLM 64/32 + HKCU Uninstall；DisplayName 匹配 → DisplayIcon（剥 ",N" 图标
/// 索引后缀）/ InstallLocation+exe_names 组合
#[cfg(windows)]
fn registry_locate(sess: &Session) -> Option<PathBuf> {
    use windows_registry::{CURRENT_USER, LOCAL_MACHINE};
    let roots = [
        (LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
        (LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
        (CURRENT_USER, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
    ];
    for (root, sub) in roots {
        let Ok(uninstall) = root.open(sub) else { continue };
        let Ok(key_iter) = uninstall.keys() else { continue };
        for key in key_iter {
            let Ok(item) = uninstall.open(&key) else { continue };
            let Ok(display) = item.get_string("DisplayName") else { continue };
            if !sess.prof.reg_patterns.iter().any(|p| glob_match_ci(p, &display)) {
                continue;
            }
            // DisplayIcon：剥 ",N" 图标索引后缀（2026-10-17 修复：原 replace(',', "")
            // 仿 PS -replace 仅删逗号本身，会把 "…\WorkBuddyAI.exe,0" 变
            // "…\WorkBuddyAI.exe0" 恒 is_file 失败，注册表级形同虚设；改为取首个
            // 逗号段，与 env.rs resolve_reg_profile_candidate 同构）
            if let Ok(icon) = item.get_string("DisplayIcon") {
                let clean = icon.split(',').next().unwrap_or(&icon).trim().to_string();
                let p = PathBuf::from(clean);
                if p.is_file() && profile::exe_matches(&p, &sess.prof) {
                    return Some(p);
                }
            }
            // InstallLocation + exe_names 组合探测
            if let Ok(loc) = item.get_string("InstallLocation") {
                for exe in sess.prof.exe_names {
                    let p = PathBuf::from(loc.trim()).join(exe);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[cfg(windows)] // 仅 lnk_dirs 用例消费（displayicon 用例为纯 std 语义）
    use super::*;

    #[test]
    fn displayicon_索引后缀剥离_取首个逗号段() {
        // 2026-10-17 修复：原 replace(',', "") 仿 PS -replace 仅删逗号本身，
        // 会把 "…\WorkBuddyAI.exe,0" 变 "…\WorkBuddyAI.exe0" 恒 is_file 失败，
        // 注册表级形同虚设；改为取首个逗号段（剥图标索引语义，与 env.rs 同构）
        let icon = "C:\\x\\WorkBuddyAI.exe,0";
        assert_eq!(
            icon.split(',').next().unwrap_or(icon).trim(),
            "C:\\x\\WorkBuddyAI.exe"
        );
        // 无索引后缀原样保留
        let bare = "C:\\x\\Doubao.exe";
        assert_eq!(bare.split(',').next().unwrap_or(bare).trim(), bare);
    }

    #[cfg(windows)]
    #[test]
    fn lnk_dirs_不因缺失目录崩溃() {
        // 任一目录缺失均静默过滤（PS Test-Path continue 语义）
        let dirs = lnk_dirs();
        assert!(dirs.iter().all(|d| d.is_dir()));
    }

    // ── 第 5 级缓存兜底 bundle 白名单复检（2026-10-19 缓存投毒防护）──
    #[cfg(target_os = "macos")]
    mod mac {
        use super::super::cached_bundle_ok;
        use crate::switcher::TargetApp;
        use crate::switcher::profile::profile_for;

        #[test]
        fn cached_bundle_ok_白名单复检() {
            // find_bundle 第 5 级兜底对缓存统一归一 bundle 根后验双信号白名单：
            // 串台 bundle（国内档案缓存了国际 CodeBuddy.app）必须拒绝，防仅装
            // 国际版机器上国内切换 open 拉起国际客户端
            let root =
                std::env::temp_dir().join(format!("sw-locate-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let mk = |name: &str, id: &str| {
                let contents = root.join(name).join("Contents");
                std::fs::create_dir_all(&contents).unwrap();
                std::fs::write(
                    contents.join("Info.plist"),
                    format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                         <plist version=\"1.0\"><dict><key>CFBundleIdentifier</key>\
                         <string>{id}</string></dict></plist>\n"
                    ),
                )
                .unwrap();
                root.join(name)
            };
            let cn = mk("CodeBuddy CN.app", "com.tencent.codebuddycn");
            let intl = mk("CodeBuddy.app", "com.tencent.codebuddy");
            let prof = profile_for(TargetApp::CodeBuddy, &root);
            // .app 根形态（bundle 定位链缓存）
            assert!(cached_bundle_ok(&cn, prof.exe_names, prof.mac_bundle_ids));
            assert!(!cached_bundle_ok(&intl, prof.exe_names, prof.mac_bundle_ids));
            // 主进程二进制形态（stop_app 停前缓存）归一 bundle 根后同样复检
            assert!(cached_bundle_ok(
                &cn.join("Contents/MacOS/Electron"),
                prof.exe_names,
                prof.mac_bundle_ids
            ));
            assert!(!cached_bundle_ok(
                &intl.join("Contents/MacOS/Electron"),
                prof.exe_names,
                prof.mac_bundle_ids
            ));
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
