//! PE 版本资源直读（替代 `(Get-Item $exe).VersionInfo` 的 powershell 子进程）。
//!
//! 背景：环境检测（env_check / workbuddy_env_check / codebuddy_env_check /
//! qoder_env_check / app_locate）每次都要取客户端 exe 版本号，原实现 spawn 一个
//! powershell.exe 读 VersionInfo——单次实测 1.2s+，而概览页与顶栏在同一轮界面切换里
//! 会重复触发（Buddy 一次切换 4 次），是「切板块后概览数据要等」的主要开销。
//! 这里改为进程内直读 PE 的 VS_VERSION_INFO 资源，微秒级、零子进程。
//!
//! 取值顺序对齐 PowerShell 的 `$v.ProductVersion` / `$v.FileVersion`：
//! StringFileInfo 的 ProductVersion → StringFileInfo 的 FileVersion →
//! VS_FIXEDFILEINFO 的固定信息四段版本（字符串资源缺失时 PowerShell 展示的就是它）。
//!
//! 读取失败（非 PE 文件 / 无版本资源 / 权限不足）返回 None，由调用方回退旧实现。

/// 读取 exe 版本资源，返回「产品版本优先、文件版本兜底」的原始版本串。
/// 未做格式归一（末尾冗余 `.0` 裁剪等由调用方 `env::normalize_version` 统一处理）。
#[cfg(windows)]
pub fn product_or_file_version(path: &str) -> Option<String> {
    win::product_or_file_version(path)
}

/// macOS 适配预留：PE 版本资源是 Windows 专属概念，mac 由调用方回退原实现
///（pgrep/属性读取），此处恒 None。
/// allow：唯一调用点 env::version_of 为 cfg(windows)，mac 构建下本桩零调用方，
/// 不加门控会触发 dead_code 警告（零警告红线）。
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn product_or_file_version(_path: &str) -> Option<String> {
    None
}

#[cfg(windows)]
mod win {
    use core::ffi::c_void;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    /// UTF-16 编码 + 结尾 NUL（Win32 宽字符 API 入参）
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 载入版本资源原始字节块；文件无版本资源 / 读取失败返回 None
    pub(super) fn version_block(path: &str) -> Option<Vec<u8>> {
        let path_w = wide(path);
        let mut handle: u32 = 0;
        // SAFETY: path_w 为以 NUL 结尾的合法宽字符串；handle 为可写出参
        let size = unsafe { GetFileVersionInfoSizeW(path_w.as_ptr(), &mut handle) };
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf 长度即 API 返回的 size；dwHandle 必须传 0（文档规定保留字段）
        let ok = unsafe {
            GetFileVersionInfoW(path_w.as_ptr(), 0, size, buf.as_mut_ptr().cast::<c_void>())
        };
        if ok == 0 {
            return None;
        }
        Some(buf)
    }

    /// 查询子块，返回其在资源块内的字节偏移与长度（puLen 语义随子块类型不同）。
    /// VerQueryValueW 返回的是块内指针，这里换算成偏移，避免跨栈持有裸指针。
    fn query_offset(block: &[u8], sub: &str) -> Option<(usize, u32)> {
        let sub_w = wide(sub);
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len: u32 = 0;
        // SAFETY: block 为 GetFileVersionInfoW 填充的资源块；sub_w 为以 NUL 结尾的合法宽字符串
        let ok = unsafe {
            VerQueryValueW(
                block.as_ptr().cast::<c_void>(),
                sub_w.as_ptr(),
                &mut ptr,
                &mut len,
            )
        };
        if ok == 0 || ptr.is_null() {
            return None;
        }
        let base = block.as_ptr() as usize;
        let off = (ptr as usize).checked_sub(base)?;
        if off > block.len() {
            return None;
        }
        Some((off, len))
    }

    /// 查询字符串子块（首个 NUL 结尾）。字符串子块的 puLen 语义为「字符数（含 NUL）」，
    /// 这里按「从偏移起读到 NUL」解码，对两种语义都安全。
    fn query_string(block: &[u8], sub: &str) -> Option<String> {
        let (off, len) = query_offset(block, sub)?;
        let max_chars = (len as usize).max(1);
        let mut units: Vec<u16> = Vec::new();
        for i in 0..max_chars {
            let p = off + i * 2;
            if p + 2 > block.len() {
                break;
            }
            let u = u16::from_le_bytes([block[p], block[p + 1]]);
            if u == 0 {
                break;
            }
            units.push(u);
        }
        let s = String::from_utf16_lossy(&units).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// StringFileInfo 的语言/代码页组合（`\VarFileInfo\Translation`，每项 4 字节 =
    /// `{ WORD wLanguage; WORD wCodePage }`，小端内存序 → `u32::from_le_bytes` 读出后
    /// **低 16 位是语言 ID、高 16 位是代码页**）；缺失时回退 en-US + Unicode
    ///（PowerShell 同款兜底，常量按同序 = (代码页 << 16) | 语言 ID）。
    /// 评审修正（2026-10-08）：原实现高低位颠倒，对真实资源必然 miss 子块名 → 静默
    /// 落到 VS_FIXEDFILEINFO，Electron 系客户端版本显示退化为构建号（PR #68 评审实测）。
    fn translations(block: &[u8]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if let Some((off, len)) = query_offset(block, "\\VarFileInfo\\Translation") {
            let end = off.saturating_add(len as usize).min(block.len());
            for c in block[off..end].chunks_exact(4) {
                out.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        if out.is_empty() {
            out.push(0x04B0_0409); // en-US(0x0409) + Unicode(0x04B0)
        }
        out
    }

    /// 按语言/代码页逐个尝试取 StringFileInfo 值（多语言资源命中即返回）。
    /// 子块名语言 ID 在前：`\StringFileInfo\{lang:04x}{cp:04x}\{key}`
    pub(super) fn string_file_info(block: &[u8], key: &str) -> Option<String> {
        for t in translations(block) {
            let sub = format!("\\StringFileInfo\\{:04x}{:04x}\\{}", t & 0xFFFF, t >> 16, key);
            if let Some(v) = query_string(block, &sub) {
                return Some(v);
            }
        }
        None
    }

    /// VS_FIXEDFILEINFO 的固定信息文件版本（四段 a.b.c.d）；签名不符视为异常块
    fn fixed_file_version(block: &[u8]) -> Option<String> {
        let (off, len) = query_offset(block, "\\")?;
        if (len as usize) < 28 || off + 28 > block.len() {
            return None;
        }
        let dword = |i: usize| {
            u32::from_le_bytes([block[off + i], block[off + i + 1], block[off + i + 2], block[off + i + 3]])
        };
        if dword(0) != 0xFEEF_04BD {
            return None;
        }
        let ms = dword(8);
        let ls = dword(12);
        Some(format!("{}.{}.{}.{}", ms >> 16, ms & 0xFFFF, ls >> 16, ls & 0xFFFF))
    }

    /// 见模块级文档：产品版本 → 文件版本 → 固定信息版本
    pub(super) fn product_or_file_version(path: &str) -> Option<String> {
        let block = version_block(path)?;
        string_file_info(&block, "ProductVersion")
            .or_else(|| string_file_info(&block, "FileVersion"))
            .or_else(|| fixed_file_version(&block))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非 PE 文件（普通文本）读取失败必须返回 None，不得 panic
    #[test]
    fn 非pe文件返回none() {
        let dir = std::env::temp_dir().join(format!("aiwork_pe_ver_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("not-pe.txt");
        std::fs::write(&f, b"plain text, not a PE").unwrap();
        assert_eq!(product_or_file_version(&f.to_string_lossy()), None);
        // 路径不存在同样返回 None（找不到文件 / 不是 PE 的语义一致）
        assert_eq!(product_or_file_version(&dir.join("missing.exe").to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 系统自带 exe（有版本资源）应能读到形如数字点分的版本串
    #[cfg(windows)]
    #[test]
    fn 系统exe可读到版本() {
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let exe = std::path::Path::new(&sysroot).join("System32").join("notepad.exe");
        if !exe.is_file() {
            return; // 环境缺该文件时跳过（不引入环境依赖）
        }
        let v = product_or_file_version(&exe.to_string_lossy());
        assert!(v.is_some(), "notepad.exe 应能读到版本串: {v:?}");
        // 回归锁定（2026-10-08 评审）：Translation 低 16 位才是语言 ID——若字节序颠倒，
        // 字符串表全 miss、静默落到 VS_FIXEDFILEINFO。notepad 两者恰好相等，
        // product_or_file_version 察觉不到，必须直查字符串表
        let block = win::version_block(&exe.to_string_lossy()).expect("notepad 版本资源块");
        assert!(
            win::string_file_info(&block, "ProductVersion").is_some(),
            "StringFileInfo\\{{lang}}{{cp}}\\ProductVersion 应命中（Translation 字节序回归）"
        );
        let v = v.unwrap();
        assert!(
            v.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "版本串应以数字开头: {v}"
        );
    }
}
