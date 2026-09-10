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
import hashlib
import os
import re
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_TAURI = os.path.join(ROOT, "src-tauri")

# 发布校验清单文件名（作为 Release 第 4 个资产上传，更新器下载后校验安装包完整性）
MANIFEST_NAME = "latest.json"


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

    # 生成发布校验清单 latest.json：版本号 + 各资产 SHA-256。
    # 更新器（updater.rs）下载安装包后与清单比对，不匹配即拒绝安装（S1 insecure_update 纵深防御）。
    assets = {}
    for name in (
        f"{product}_{version}_x64-setup.exe",
        f"{product}_{version}_x64_zh-CN.msi",
        f"{product}_{version}_x64_portable.zip",
    ):
        p = os.path.join(out_dir, name)
        if not os.path.isfile(p):
            # portable zip 未打包时警告但不阻塞（更新器只会安装 setup/msi）
            if name.endswith("_portable.zip"):
                print("WARN（清单跳过，文件不存在）:", name, file=sys.stderr)
                continue
            sys.exit(f"清单生成失败：产物缺失 {name}")
        h = hashlib.sha256()
        with open(p, "rb") as f:
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                h.update(chunk)
        assets[name] = h.hexdigest()
        print(f"SHA256 {h.hexdigest()}  {name}")
    manifest = {"version": version, "assets": assets}
    man_path = os.path.join(out_dir, MANIFEST_NAME)
    with open(man_path, "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
    print("OK:", man_path)


if __name__ == "__main__":
    main()
