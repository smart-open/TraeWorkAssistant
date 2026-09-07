#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
打包产物统一命名：把 `npm run tauri build` 产出的安装包复制到 release/，
统一使用中文产品名命名（APP 名称标识）。

用法：python scripts/rename_release.py
  src-tauri/target/release/bundle/nsis/AI Work 助手_<ver>_x64-setup.exe
      → release/AI Work 助手_<ver>_x64-setup.exe
  src-tauri/target/release/bundle/msi/AI Work 助手_<ver>_x64_zh-CN.msi
      → release/AI Work 助手_<ver>_x64_zh-CN.msi
  portable zip 由 package_portable.py 直接生成同名（无需重命名）。
"""
import json
import os
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_TAURI = os.path.join(ROOT, "src-tauri")


def main():
    with open(os.path.join(SRC_TAURI, "tauri.conf.json"), "r", encoding="utf-8") as f:
        conf = json.load(f)
    version = conf["version"]
    product = conf["productName"]
    out_dir = os.path.join(ROOT, "release")
    os.makedirs(out_dir, exist_ok=True)

    jobs = [
        (os.path.join(SRC_TAURI, "target", "release", "bundle", "nsis",
                      f"{product}_{version}_x64-setup.exe"),
         os.path.join(out_dir, f"{product}_{version}_x64-setup.exe")),
        (os.path.join(SRC_TAURI, "target", "release", "bundle", "msi",
                      f"{product}_{version}_x64_zh-CN.msi"),
         os.path.join(out_dir, f"{product}_{version}_x64_zh-CN.msi")),
    ]
    moved = 0
    for src, dst in jobs:
        if os.path.isfile(src):
            shutil.copy2(src, dst)
            print("OK:", dst)
            moved += 1
        else:
            print("SKIP（不存在）:", src, file=sys.stderr)
    if moved == 0:
        print("未找到任何安装包产物，请先执行 npm run tauri build", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
