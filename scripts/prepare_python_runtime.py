#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
准备内置 Python 运行时（issue #6）：embeddable 解释器 + requirements 依赖，
产物输出到 src-tauri/target/python-runtime/，由 tauri.conf.json resources
以 "python/" 名义与 src-python 脚本合并打包，实现安装后离线可用、不依赖系统 Python。

用法（通常由 tauri.conf.json beforeBuildCommand 自动调用）：
  python scripts/prepare_python_runtime.py [--force]

可配置环境变量：
  AIWORK_PY_RUNTIME_VERSION  embeddable 版本（默认 3.12.10）
  AIWORK_PY_MIRROR           embeddable zip 镜像（默认 npmmirror binary python）
  AIWORK_PIP_INDEX           pip 安装源（默认清华 TUNA）
幂等：产物 stamp（版本 + requirements 哈希）一致且自检通过时跳过重建；
--force 强制重建。缓存：embed zip 与 get-pip.py 缓存在 target/python-embed-cache/。
"""
import hashlib
import io
import os
import shutil
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNTIME_DIR = ROOT / "src-tauri" / "target" / "python-runtime"
CACHE_DIR = ROOT / "src-tauri" / "target" / "python-embed-cache"
SRC_PY_DIR = ROOT / "src-python"
REQ_FILE = SRC_PY_DIR / "requirements.txt"

CREATE_NO_WINDOW = 0x08000000 if os.name == "nt" else 0


def log(msg: str) -> None:
    print(f"[python-runtime] {msg}", flush=True)


def fail(msg: str) -> None:
    log(f"ERROR: {msg}")
    sys.exit(1)


def download(url: str, dest: Path) -> Path:
    if dest.exists() and dest.stat().st_size > 0:
        log(f"复用缓存: {dest.name}")
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    log(f"下载: {url}")
    try:
        with urllib.request.urlopen(url, timeout=120) as resp, open(dest, "wb") as f:
            shutil.copyfileobj(resp, f)
    except Exception as e:  # noqa: BLE001
        if dest.exists():
            dest.unlink()
        fail(f"下载失败（可配置代理环境变量 HTTPS_PROXY 或镜像后重试）: {e}")
    return dest


def run(cmd: list, desc: str) -> None:
    log(desc)
    r = subprocess.run(cmd, capture_output=True, text=True, creationflags=CREATE_NO_WINDOW)
    if r.returncode != 0:
        tail = (r.stderr or r.stdout or "").strip().splitlines()[-6:]
        fail(f"{desc} 失败（exit {r.returncode}）:\n" + "\n".join(tail))


def reqs_stamp(version: str) -> str:
    req_hash = hashlib.sha256(REQ_FILE.read_bytes()).hexdigest()[:12]
    return f"{version}+req{req_hash}"


def runtime_ok() -> bool:
    """产物存在且能导入全部关键依赖时视为可用（自检含 cryptography 与 win32crypt）"""
    exe = RUNTIME_DIR / "python.exe"
    if not exe.exists():
        return False
    r = subprocess.run(
        [str(exe), "-c", "import cryptography, win32crypt, sqlite3"],
        capture_output=True, creationflags=CREATE_NO_WINDOW,
    )
    return r.returncode == 0


def sync_scripts() -> None:
    """将 src-python 业务脚本同步进 runtime 根目录（与 python.exe 平铺，匹配 spawn_script
    的 python_dir/脚本名 路径规则）。每次无条件执行，保证 dev/build 用最新脚本。
    resources 仅映射 runtime 目录（Tauri map key 不支持 glob），脚本必须位于其中才会被打包。"""
    RUNTIME_DIR.mkdir(parents=True, exist_ok=True)
    for py in sorted(SRC_PY_DIR.glob("*.py")):
        shutil.copy2(py, RUNTIME_DIR / py.name)


def prepare() -> None:
    if os.name != "nt":
        log("非 Windows 平台，跳过内置 Python 运行时准备（打包仍走系统 Python 回退）")
        return
    if not REQ_FILE.exists():
        fail(f"未找到 {REQ_FILE}")

    version = os.environ.get("AIWORK_PY_RUNTIME_VERSION", "3.12.10")
    mirror = os.environ.get(
        "AIWORK_PY_MIRROR",
        f"https://registry.npmmirror.com/-/binary/python/{version}",
    )
    pip_index = os.environ.get("AIWORK_PIP_INDEX", "https://pypi.tuna.tsinghua.edu.cn/simple")
    get_pip_url = os.environ.get("AIWORK_GETPIP_URL", "https://bootstrap.pypa.io/get-pip.py")

    stamp = reqs_stamp(version)
    stamp_file = RUNTIME_DIR / ".runtime-stamp"
    if not FORCE and stamp_file.exists() and stamp_file.read_text(encoding="utf-8").strip() == stamp:
        if runtime_ok():
            sync_scripts()
            log(f"运行时已是最新（{stamp}），跳过（业务脚本已同步）")
            return
        log("运行时存在但自检失败，将重建")
    if FORCE:
        log("强制重建")

    # 1. 下载并解压 embeddable 包（官方 zip 为平铺布局）
    zip_dest = download(f"{mirror}/python-{version}-embed-amd64.zip",
                        CACHE_DIR / f"python-{version}-embed-amd64.zip")
    if RUNTIME_DIR.exists():
        shutil.rmtree(RUNTIME_DIR)
    RUNTIME_DIR.mkdir(parents=True)
    with zipfile.ZipFile(zip_dest) as zf:
        zf.extractall(RUNTIME_DIR)

    # 2. 修 ._pth：追加 site-packages 并启用 site（否则嵌入式隔离模式找不到已装依赖）
    pth_files = list(RUNTIME_DIR.glob("python*._pth"))
    if len(pth_files) != 1:
        fail(f"embeddable 包中应恰有一个 ._pth 文件，实际 {len(pth_files)} 个")
    pth = pth_files[0]
    body = pth.read_text(encoding="utf-8")
    body = body.replace("#import site", "import site")
    if "Lib/site-packages" not in body:
        body = body.rstrip("\r\n") + "\nLib/site-packages\n"
    pth.write_text(body, encoding="utf-8")
    log(f"已配置 {pth.name}: 启用 site + Lib/site-packages")

    exe = str(RUNTIME_DIR / "python.exe")

    # 3. 引导 pip（embeddable 不自带）并安装 requirements（单一依赖来源）
    get_pip = download(get_pip_url, CACHE_DIR / "get-pip.py")
    run([exe, str(get_pip), "--no-warn-script-location"], "引导 pip")
    run([exe, "-m", "pip", "install", "--no-warn-script-location",
         "--disable-pip-version-check", "--only-binary=:all:",
         "-i", pip_index, "-r", str(REQ_FILE)], "安装 requirements 依赖")

    # 4. pywin32 运行时 DLL 平移到解释器根目录（Windows DLL 搜索含 exe 所在目录）
    sys32 = RUNTIME_DIR / "Lib" / "site-packages" / "pywin32_system32"
    if sys32.exists():
        for dll in sys32.glob("*.dll"):
            shutil.copy2(dll, RUNTIME_DIR / dll.name)
            log(f"DLL 平移: {dll.name}")

    # 5. 清理无用产物，压缩产物体积（实测净减 ~10.7MB 未压缩）：
    #    根级：pywin32 安装器残留 python.cat（Windows 目录签名文件，运行时无用）、
    #          pythonw.exe（无 GUI 场景）
    #    site-packages 级：pywin32 自带的 IDE/COM 扩展/帮助文档等——业务仅用 win32crypt
    #          （DPAPI），依赖链只涉及 win32/ 与 pywin32_system32，其余均可删
    #    必须保留：dist-info（运行时自愈 pip install 依赖其识别已装包，避免无谓重装）、
    #          __pycache__（只读安装目录下无法重生成，预置字节码可加速首次 import）
    for name in ("Scripts", "tests", ".pytest_cache", "logs", "pythonw.exe", "python.cat"):
        p = RUNTIME_DIR / name
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
        elif p.exists():
            p.unlink(missing_ok=True)
    sp = RUNTIME_DIR / "Lib" / "site-packages"
    for name in ("pythonwin", "win32comext", "win32com", "adodbapi", "isapi", "bin", "PyWin32.chm"):
        p = sp / name
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
        elif p.exists():
            p.unlink(missing_ok=True)

    # 6. 终验：关键依赖必须可导入，否则视为失败并清理半成品
    if not runtime_ok():
        shutil.rmtree(RUNTIME_DIR, ignore_errors=True)
        fail("运行时自检失败（import cryptography/win32crypt），已清理产物")
    stamp_file.write_text(stamp, encoding="utf-8")
    sync_scripts()

    total_mb = sum(f.stat().st_size for f in RUNTIME_DIR.rglob("*") if f.is_file()) / 1048576
    log(f"OK: 运行时就绪 {RUNTIME_DIR}（{total_mb:.1f} MB, stamp={stamp}）")


if __name__ == "__main__":
    FORCE = "--force" in sys.argv
    prepare()
