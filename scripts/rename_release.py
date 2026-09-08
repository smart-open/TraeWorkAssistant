#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
打包产物统一命名：把 `npm run tauri build` 产出的安装包复制到 release/，
统一使用中文产品名命名。

用法：python scripts/rename_release.py
  src-tauri/target/release/bundle/nsis/Trae Work 助手_<ver>_x64-setup.exe
      → release/Trae Work 助手_<ver>_x64-setup.exe
  src-tauri/target/release/bundle/msi/Trae Work 助手_<ver>_x64_zh-CN.msi
      → release/Trae Work 助手_<ver>_x64_zh-CN.msi
  portable zip 由 package_portable.py 直接生成同名（无需重命名）。
"""
import json
import os
import re
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_TAURI = os.path.join(ROOT, "src-tauri")


def read_version():
    # 版本号单源化：tauri.conf.json 不再写 version，回退到 Cargo.toml
    with open(os.path.join(SRC_TAURI, "tauri.conf.json"), "r", encoding="utf-8") as f:
        conf = json.load(f)
    if conf.get("version"):
        return conf["version"]
    with open(os.path.join(SRC_TAURI, "Cargo.toml"), "r", encoding="utf-8") as f:
        m = re.search(r'^version\s*=\s*"([^"]+)"', f.read(), re.M)
    if not m:
        raise SystemExit("无法从 tauri.conf.json / Cargo.toml 读取版本号")
    return m.group(1)


def main():
    version = read_version()
    with open(os.path.join(SRC_TAURI, "tauri.conf.json"), "r", encoding="utf-8") as f:
        conf = json.load(f)
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
    missing = []
    for src, dst in jobs:
        if os.path.isfile(src):
            shutil.copy2(src, dst)
            print("OK:", dst)
            moved += 1
        else:
            missing.append(src)
            print("SKIP（不存在）:", src, file=sys.stderr)
    # 严格退出码：bundle.targets 同时声明 msi + nsis，产物必须齐全。
    # 部分缺失（只迁一个）说明构建不完整，发布脚本必须失败而非静默通过。
    if moved < len(jobs):
        print(
            f"安装包产物不完整：仅找到 {moved}/{len(jobs)} 个，缺失：",
            file=sys.stderr,
        )
        for src in missing:
            print("  ", src, file=sys.stderr)
        print("请先完整执行 npm run tauri build", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
