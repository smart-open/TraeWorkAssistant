#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
打包产物统一命名：把 `npm run tauri build` 产出的安装包复制到 release/，
统一使用中文产品名命名（APP 名称标识）。

用法：python scripts/rename_release.py [--strict]
  --strict  任一产物缺失时以非零码退出（默认仅告警）
  src-tauri/target/release/bundle/nsis/AI Work 助手_<ver>_x64-setup.exe
      → release/AI Work 助手_<ver>_x64-setup.exe
  src-tauri/target/release/bundle/msi/AI Work 助手_<ver>_x64_zh-CN.msi
      → release/AI Work 助手_<ver>_x64_zh-CN.msi
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

# 发布校验清单文件名（作为 Release 资产上传，更新器下载安装包后校验完整性，
# updater.rs fail-closed：清单缺失/损坏/版本不符/未收录资产均阻止自动更新）
MANIFEST_NAME = "latest.json"


def read_version(conf):
    # 版本单源 = Cargo.toml；tauri.conf.json 里的 version 字段已移除（自动回读 Cargo.toml）
    if conf.get("version"):
        return conf["version"]
    with open(os.path.join(SRC_TAURI, "Cargo.toml"), "r", encoding="utf-8") as f:
        m = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', f.read(), re.M)
    if not m:
        sys.exit("ERROR: 无法从 Cargo.toml 读取版本号")
    return m.group(1)


def main():
    with open(os.path.join(SRC_TAURI, "tauri.conf.json"), "r", encoding="utf-8") as f:
        conf = json.load(f)
    version = read_version(conf)
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
            print("SKIP（不存在）:", src, file=sys.stderr)
            missing.append(src)
    if moved == 0:
        print("未找到任何安装包产物，请先执行 npm run tauri build", file=sys.stderr)
        sys.exit(1)

    # 生成发布校验清单 latest.json：版本号 + 各资产 SHA-256。
    # 更新器（updater.rs）下载安装包后与清单比对，不匹配即拒绝安装（更新包完整性校验）。
    assets = {}
    for name in (
        f"{product}_{version}_x64-setup.exe",
        f"{product}_{version}_x64_zh-CN.msi",
        f"{product}_{version}_x64_portable.zip",
    ):
        p = os.path.join(out_dir, name)
        if not os.path.isfile(p):
            # portable zip 未打包时警告但不阻塞（更新器只安装 setup/msi）
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
    print("提示：发布时请将", MANIFEST_NAME, "作为 Release 资产一并上传", file=sys.stderr)

    if missing:
        print(
            "WARNING: 有 %d 个产物缺失，上传 release 前请核对清单：" % len(missing),
            file=sys.stderr,
        )
        for m in missing:
            print("  -", m, file=sys.stderr)
        if "--strict" in sys.argv:
            print("--strict：缺失产物视为失败", file=sys.stderr)
            sys.exit(1)


if __name__ == "__main__":
    main()
