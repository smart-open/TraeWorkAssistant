#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 release 构建产物打包为 portable zip：
  AI Work 助手_<version>_x64_portable.zip
内容布局（与 Tauri 安装包一致，exe 直接读取同目录 resources/）：
  AI Work 助手/               ← 顶层目录用产品名（APP 显示名称）
    ai-work-assistant.exe     ← 主程序名取 tauri.conf.json 的 mainBinaryName
    resources/python/          (来自 src-python/)
    resources/ps/              (来自 src-ps/)
"""
import json
import os
import shutil
import sys
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_TAURI = os.path.join(ROOT, "src-tauri")
CONF = os.path.join(SRC_TAURI, "tauri.conf.json")
OUT_DIR = os.path.join(ROOT, "release")

# 主程序候选：优先 mainBinaryName 命名的产物，兼容旧的 cargo 包名产物
EXE_CANDIDATES = [
    os.path.join(SRC_TAURI, "target", "release", "ai-work-assistant.exe"),
    os.path.join(SRC_TAURI, "target", "release", "trae-work-assistant.exe"),
]


def load_conf():
    with open(CONF, "r", encoding="utf-8") as f:
        return json.load(f)


def walk_copy(src, dst, skip_dirs=("__pycache__", ".git")):
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
    # 版本单源 = Cargo.toml；tauri.conf.json 里的 version 字段已移除（自动回读 Cargo.toml）
    if conf.get("version"):
        version = conf["version"]
    else:
        import re as _re
        with open(os.path.join(SRC_TAURI, "Cargo.toml"), "r", encoding="utf-8") as f:
            m = _re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', f.read(), _re.M)
        if not m:
            sys.exit("ERROR: 无法从 Cargo.toml 读取版本号")
        version = m.group(1)
    binary_name = conf.get("mainBinaryName") or "ai-work-assistant"
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
    # 产物文件名使用中文产品名（如 AI Work 助手_3.0.0_x64_portable.zip）
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
    res_dir = os.path.join(stage_app, "resources")
    for src_abs, dest in abs_res.items():
        if not os.path.isdir(src_abs):
            print("WARN: 资源目录缺失:", src_abs, file=sys.stderr)
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
