#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 release 构建产物打包为 portable zip：
  Trae Work 助手_<version>_x64_portable.zip
内容布局（与 Tauri 安装包一致，exe 直接读取同目录 resources/）：
  Trae Work 助手/               ← 顶层目录用产品名（APP 显示名称）
    trae-work-assistant.exe     ← 主程序名（未配置 mainBinaryName 时用 cargo 包名）
    resources/python/            (来自 src-python/)
    resources/ps/                (来自 src-ps/)
"""
import json
import os
import re
import shutil
import sys
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_TAURI = os.path.join(ROOT, "src-tauri")
CONF = os.path.join(SRC_TAURI, "tauri.conf.json")
OUT_DIR = os.path.join(ROOT, "release")

# 主程序候选：优先 mainBinaryName 命名的产物，兼容旧的 cargo 包名产物
EXE_CANDIDATES = [
    os.path.join(SRC_TAURI, "target", "release", "trae-work-assistant.exe"),
    os.path.join(SRC_TAURI, "target", "release", "ai-work-assistant.exe"),
]


def load_conf():
    with open(CONF, "r", encoding="utf-8") as f:
        return json.load(f)


def walk_copy(src, dst, skip_dirs=(".git",)):
    # __pycache__ 必须保留（与 NSIS 安装包一致）：portable 同样部署在只读目录，
    # 字节码缓存无法运行时重建，剔除会拖慢首次 import（Issue #6 打包审计结论）
    os.makedirs(dst, exist_ok=True)
    for root, dirs, files in os.walk(src):
        dirs[:] = [d for d in dirs if d not in skip_dirs]
        for name in files:
            s = os.path.join(root, name)
            rel = os.path.relpath(s, src)
            t = os.path.join(dst, rel)
            os.makedirs(os.path.dirname(t), exist_ok=True)
            shutil.copy2(s, t)


def main():
    conf = load_conf()
    product = conf["productName"]
    # 版本号单源化：tauri.conf.json 不再写 version，回退到 Cargo.toml
    version = conf.get("version")
    if not version:
        with open(os.path.join(SRC_TAURI, "Cargo.toml"), "r", encoding="utf-8") as f:
            m = re.search(r'^version\s*=\s*"([^"]+)"', f.read(), re.M)
        if not m:
            raise SystemExit("无法从 tauri.conf.json / Cargo.toml 读取版本号")
        version = m.group(1)
    binary_name = conf.get("mainBinaryName") or "trae-work-assistant"
    resources = conf["bundle"]["resources"]
    # resources 形如 {"../src-python/": "python/", "../src-ps/": "ps/"}
    abs_res = {}
    for src_rel, dest in resources.items():
        src_abs = os.path.normpath(os.path.join(SRC_TAURI, src_rel))
        abs_res[src_abs] = dest.strip("/\\")

    release_exe = next((p for p in EXE_CANDIDATES if os.path.isfile(p)), None)
    if release_exe is None:
        print("ERROR: release exe 不存在，已尝试:", EXE_CANDIDATES, file=sys.stderr)
        sys.exit(1)

    os.makedirs(OUT_DIR, exist_ok=True)
    # 产物文件名使用中文产品名（如 Trae Work 助手_2.6.0_x64_portable.zip）
    zip_name = f"{product}_{version}_x64_portable.zip"
    zip_path = os.path.join(OUT_DIR, zip_name)

    tmp_root = os.path.join(OUT_DIR, "_portable_stage")
    if os.path.exists(tmp_root):
        shutil.rmtree(tmp_root)
    stage_app = os.path.join(tmp_root, product)
    os.makedirs(stage_app, exist_ok=True)

    # 1) 主程序按 mainBinaryName 命名放入产品目录
    shutil.copy2(release_exe, os.path.join(stage_app, binary_name + ".exe"))

    # 2) 资源按 Tauri 布局放入 resources/
    # 注意：内嵌 Python 运行时改造后，tauri.conf.json 的 python/ 资源来自
    # build/python-bundle/（由 scripts/prepare_python_runtime.py 白名单装配），
    # 该目录缺失时 portable 包将不含内嵌解释器（安装后回退系统 Python）。
    res_dir = os.path.join(stage_app, "resources")
    for src_abs, dest in abs_res.items():
        if not os.path.isdir(src_abs):
            hint = ""
            if "python-bundle" in os.path.basename(src_abs):
                hint = "（内嵌 Python 运行时未装配，请先运行: python scripts/prepare_python_runtime.py；" \
                       "否则 portable 包将回退系统 Python）"
            print("WARN: 资源目录缺失:", src_abs, hint, file=sys.stderr)
            continue
        target = os.path.join(res_dir, dest)
        walk_copy(src_abs, target)

    # 3) 打包（保留内部目录结构，顶层为产品名文件夹）
    print("正在打包:", zip_path)
    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as z:
        for root, dirs, files in os.walk(tmp_root):
            for name in files:
                fp = os.path.join(root, name)
                arc = os.path.relpath(fp, tmp_root)
                z.write(fp, arc)

    shutil.rmtree(tmp_root)
    size = os.path.getsize(zip_path)
    print(f"OK: {zip_path}  ({size/1024/1024:.2f} MB)")


if __name__ == "__main__":
    main()
