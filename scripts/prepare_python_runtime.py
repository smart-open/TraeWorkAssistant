#!/usr/bin/env python3
"""准备内嵌 Python 运行时（Issue #6 根治：安装包自带解释器与依赖）。

把 Windows embeddable Python 解压到 src-python/，并预装 requirements.txt 依赖；
再按发布白名单把「解释器 + 依赖 + 业务脚本」装配到 build/python-bundle/
（tauri.conf.json 的 resources 实际来源），与源目录解耦——tests/、requirements.txt
等开发文件以及 pywin32 的 IDE/COM/文档附属不会进入安装包。
应用启动时 state.rs 检测到 python/python.exe 存在即优先使用内嵌解释器，
不再回退系统 Python。

分层幂等（重复执行安全、离线友好）：
  1. src-python/python.exe 不存在           -> 全流程（下载/解压/装依赖/自检）
  2. python.exe 在但依赖自检失败            -> 仅补装依赖 + 自检
  3. python.exe 在且依赖自检通过            -> 跳过（--force 强制重建）

用法:
  python scripts/prepare_python_runtime.py [--py-version 3.13.12] [--force]
                                           [--cache-dir DIR] [--skip-download]

约束:
  - 运行本脚本的解释器主次版本必须与 embeddable 版本一致（ABI 匹配），
    否则 cryptography 等带 C 扩展的 wheel 与嵌入解释器不兼容。
  - CI 中请用 actions/setup-python 钉同一版本（见 .github/workflows/build-windows.yml）。

下载源:
  https://www.python.org/ftp/python/{ver}/python-{ver}-embed-amd64.zip
  可用环境变量 PYTHON_EMBED_URL 覆盖；zip 缓存于 %LOCALAPPDATA%/TraeWorkAssistant/build-cache。
"""

import argparse
import glob
import io
import os
import shutil
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_PYTHON = REPO_ROOT / "src-python"
REQUIREMENTS = SRC_PYTHON / "requirements.txt"
BUNDLE_STAGE = REPO_ROOT / "build" / "python-bundle"

DEFAULT_PY_VERSION = "3.13.12"

# 打包暂存白名单：与业务脚本一同发布的顶层文件（pythonw.exe / python.cat /
# requirements.txt / tests/ 等运行时用不到的内容天然不在此列）。
STAGE_TOP_FILES = ("LICENSE.txt", "python.exe", "python313._pth", "python313.zip")
STAGE_TOP_GLOBS = ("*.pyd", "*.dll")
STAGE_SCRIPTS = ("auto_checkin.py", "device_proxy.py")
# site-packages 中 pywin32 的 IDE/COM/文档附属（运行时仅用 win32crypt，
# 其依赖 pywintypes/pythoncom DLL 已复制到顶层；win32/ 本体必须保留）。
STAGE_SITE_PRUNE = (
    "pythonwin", "win32comext", "win32com", "adodbapi", "isapi", "bin", "PyWin32.chm",
)


def info(msg: str) -> None:
    print(f"[prepare-python] {msg}", flush=True)


def fail(msg: str) -> "NoReturn":  # type: ignore[valid-type]
    print(f"[prepare-python][ERROR] {msg}", file=sys.stderr, flush=True)
    sys.exit(1)


def cache_dir(explicit: str | None) -> Path:
    if explicit:
        p = Path(explicit)
    else:
        local = os.environ.get("LOCALAPPDATA")
        base = Path(local) if local else Path.home() / ".cache"
        p = base / "TraeWorkAssistant" / "build-cache"
    p.mkdir(parents=True, exist_ok=True)
    return p


def download_embed_zip(py_version: str, cache: Path) -> Path:
    """下载 embeddable zip（命中缓存则直接复用）。"""
    url = os.environ.get(
        "PYTHON_EMBED_URL",
        f"https://www.python.org/ftp/python/{py_version}/python-{py_version}-embed-amd64.zip",
    )
    dest = cache / f"python-{py_version}-embed-amd64.zip"
    if dest.exists() and dest.stat().st_size > 1_000_000:
        info(f"命中缓存: {dest}")
        return dest
    info(f"下载 embeddable Python {py_version}: {url}")
    try:
        with urllib.request.urlopen(url, timeout=120) as resp, io.open(dest, "wb") as f:
            shutil.copyfileobj(resp, f)
    except Exception as e:  # noqa: BLE001 - 需要把任意网络错误转成可读提示
        fail(
            f"下载失败: {e}\n"
            f"  可手动下载后放到: {dest}\n"
            f"  或用 --cache-dir / 环境变量 PYTHON_EMBED_URL 指定镜像地址"
        )
    info(f"下载完成: {dest} ({dest.stat().st_size / 1024 / 1024:.1f} MB)")
    return dest


def extract_embed(zip_path: Path) -> None:
    """解压 embeddable zip 到 src-python/（平铺结构，直接覆盖）。"""
    SRC_PYTHON.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(zip_path) as z:
        for member in z.infolist():
            # 防路径穿越（zip slip）
            target = (SRC_PYTHON / member.filename).resolve()
            if not str(target).startswith(str(SRC_PYTHON.resolve())):
                fail(f"zip 条目路径异常: {member.filename}")
            z.extract(member, SRC_PYTHON)
    info(f"已解压到 {SRC_PYTHON}")


def enable_site() -> None:
    """修改 python3XX._pth：取消 import site 注释，使 Lib/site-packages 与 .pth 生效。"""
    pth_files = glob.glob(str(SRC_PYTHON / "python*._pth"))
    if not pth_files:
        fail("未找到 python*._pth（embeddable 解压产物异常）")
    for p in pth_files:
        path = Path(p)
        content = path.read_text(encoding="utf-8")
        if "#import site" in content:
            content = content.replace("#import site", "import site")
            path.write_text(content, encoding="utf-8")
            info(f"已启用 site: {path.name}")
        elif "import site" in content:
            info(f"site 已启用: {path.name}")


def check_deps() -> bool:
    """用内嵌解释器自检依赖可导入。"""
    exe = SRC_PYTHON / "python.exe"
    if not exe.exists():
        return False
    r = subprocess.run(
        [str(exe), "-c", "import cryptography, win32crypt"],
        capture_output=True,
        text=True,
        cwd=str(SRC_PYTHON),
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    return r.returncode == 0


def install_deps(py_version: str) -> None:
    """用当前解释器的 pip 把依赖装到内嵌运行时的 Lib/site-packages。

    --only-binary :all: 强制全 wheel（cryptography/cffi/pywin32 均带 C 扩展，
    源码编译在 embeddable 环境既不可行也不必要）。
    """
    host = f"{sys.version_info.major}.{sys.version_info.minor}"
    embed = ".".join(py_version.split(".")[:2])
    if host != embed:
        fail(
            f"ABI 不匹配: 构建机 Python {host} != embeddable {embed}。\n"
            f"  请用 Python {embed}.x 重新运行本脚本（CI 中由 actions/setup-python 钉版本）。"
        )
    target = SRC_PYTHON / "Lib" / "site-packages"
    if target.exists():
        shutil.rmtree(target)  # 清掉上次可能残留的半成品
    cmd = [
        sys.executable, "-m", "pip", "install",
        "-r", str(REQUIREMENTS),
        "--target", str(target),
        "--only-binary", ":all:",
        "--no-warn-script-location",
    ]
    info(f"安装依赖: {' '.join(cmd[2:])}")
    r = subprocess.run(cmd)
    if r.returncode != 0:
        fail(
            "pip 安装依赖失败。\n"
            "  国内网络可设置镜像: set PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple"
        )
    # pywin32 后处理：其运行需要 pywin32_system32 下的 DLL，Windows DLL 搜索
    # 不走 sys.path，把它们复制到 python.exe 同目录（永远在搜索路径上）。
    dll_src = target / "pywin32_system32"
    if dll_src.is_dir():
        for dll in dll_src.glob("*.dll"):
            shutil.copy2(dll, SRC_PYTHON / dll.name)
        info(f"已复制 pywin32 DLL 到运行时根目录: {len(list(dll_src.glob('*.dll')))} 个")


def assemble_bundle() -> None:
    """装配打包暂存目录 build/python-bundle/（tauri.conf.json resources 的实际来源）。

    与源目录 src-python/ 解耦：只按白名单复制发布所需内容，天然排除
    tests/、requirements.txt 等开发文件与 pythonw.exe/python.cat 等运行时
    用不到的附属；site-packages 中 pywin32 的 IDE/COM/文档附属就地裁剪。

    采用增量装配（copy2 覆盖 + 幂等裁剪）而非整体重建：稳态下零删除，
    兼容对批量删除 fail-closed 的构建环境（沙箱/杀软拦截 rmtree）。
    代价是源侧删除/改名的文件会在暂存残留——依赖升级或脚本改名后
    请手动删除 build/python-bundle/ 重建一次。
    """
    if not (SRC_PYTHON / "python.exe").exists():
        fail("内嵌运行时未就绪，无法装配打包暂存")
    BUNDLE_STAGE.mkdir(parents=True, exist_ok=True)

    # 1) 顶层：精确清单 + 通配运行时产物（*.pyd/*.dll 覆盖版本升级的增减）
    for name in STAGE_TOP_FILES:
        src = SRC_PYTHON / name
        if src.exists():
            shutil.copy2(src, BUNDLE_STAGE / name)
    for pat in STAGE_TOP_GLOBS:
        for src in SRC_PYTHON.glob(pat):
            if src.is_file():
                shutil.copy2(src, BUNDLE_STAGE / src.name)

    # 2) 业务脚本（缺失即失败，防止发布空壳包）
    for name in STAGE_SCRIPTS:
        src = SRC_PYTHON / name
        if not src.exists():
            fail(f"业务脚本缺失: {src}")
        shutil.copy2(src, BUNDLE_STAGE / name)

    # 3) Lib 目录：os.walk 跳过式复制（site-packages 顶层裁剪项直接不复制，
    #    稳态零删除；源目录保持完整）
    src_lib = SRC_PYTHON / "Lib"
    dst_lib = BUNDLE_STAGE / "Lib"
    prune_set = set(STAGE_SITE_PRUNE)
    copied = 0
    for root, dirs, files in os.walk(src_lib):
        rel_root = Path(root).relative_to(src_lib)
        if rel_root == Path("site-packages"):
            dirs[:] = [d for d in dirs if d not in prune_set and not d.endswith(".dist-info")]
            files = [f for f in files if f not in prune_set]
        dst_root = dst_lib / rel_root
        dst_root.mkdir(parents=True, exist_ok=True)
        for f in files:
            shutil.copy2(Path(root) / f, dst_root / f)
            copied += 1
    info(f"已装配打包暂存: {BUNDLE_STAGE}（Lib 跳过式复制 {copied} 个文件）")


def verify_staged() -> None:
    """用暂存目录里的内嵌解释器做发布前自检，防止裁剪误伤依赖。"""
    exe = BUNDLE_STAGE / "python.exe"
    r = subprocess.run(
        [str(exe), "-c", "import cryptography, win32crypt, sqlite3"],
        capture_output=True,
        text=True,
        cwd=str(BUNDLE_STAGE),
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    if r.returncode != 0:
        fail(f"打包暂存自检失败（裁剪可能误伤依赖）: {r.stderr.strip()[-400:]}")
    info("打包暂存自检通过（cryptography / win32crypt / sqlite3）")


def main() -> None:
    if os.name != "nt":
        fail("本脚本仅支持 Windows（embeddable Python 为 Windows 专用发行）")
    try:
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")
    except Exception:
        pass

    ap = argparse.ArgumentParser(description="准备内嵌 Python 运行时")
    ap.add_argument("--py-version", default=DEFAULT_PY_VERSION)
    ap.add_argument("--force", action="store_true", help="删除现有运行时后全量重建")
    ap.add_argument("--cache-dir", help="embeddable zip 缓存目录（默认 %%LOCALAPPDATA%%/TraeWorkAssistant/build-cache）")
    ap.add_argument("--skip-download", action="store_true", help="不解压新 zip，只补装依赖/自检")
    args = ap.parse_args()

    exe = SRC_PYTHON / "python.exe"
    if args.force and exe.exists():
        info("--force: 删除现有内嵌运行时文件")
        for pat in ("python.exe", "pythonw.exe", "python*.dll", "python*._pth",
                    "python*.zip", "vcruntime*.dll", "LICENSE.txt", "Lib"):
            for p in SRC_PYTHON.glob(pat):
                shutil.rmtree(p, ignore_errors=True) if p.is_dir() else p.unlink(missing_ok=True)

    # 分层幂等判定
    if exe.exists() and check_deps():
        info("内嵌运行时已就绪（解释器 + 依赖自检通过），跳过重装")
    else:
        if not exe.exists():
            if args.skip_download:
                fail("python.exe 不存在，且 --skip-download 禁止解压新运行时")
            zip_path = download_embed_zip(args.py_version, cache_dir(args.cache_dir))
            extract_embed(zip_path)

        enable_site()

        if not check_deps():
            install_deps(args.py_version)

        if not check_deps():
            fail("依赖自检仍失败，请检查上方 pip 输出")
        info("OK: 内嵌 Python 运行时就绪（含 cryptography / pywin32）")

    # 打包暂存装配：无论运行时是否重装都执行（业务脚本可能已修改），
    # 保证 build/python-bundle/ 始终与当前源码一致，再自检防裁剪误伤。
    assemble_bundle()
    verify_staged()
    info("OK: 打包暂存就绪（build/python-bundle -> 安装包 python/）")


if __name__ == "__main__":
    main()
