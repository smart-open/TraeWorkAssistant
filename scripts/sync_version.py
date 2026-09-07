#!/usr/bin/env python3
"""版本号单源同步工具（单一来源：src-tauri/Cargo.toml）

用法：
  python scripts/sync_version.py              # 读取 Cargo.toml 版本，同步其余各处
  python scripts/sync_version.py 3.2.0        # 先把 Cargo.toml 改为目标版本，再同步其余各处

同步点（历史遗留的 6 处已收敛为 1 处 + 本脚本自动同步）：
  - src-tauri/Cargo.toml           单一来源（手动/参数指定）
  - src-tauri/Cargo.lock           自动（cargo update -p）
  - src-tauri/tauri.conf.json      已移除 version 字段（自动回读 Cargo.toml）
  - src/lib/about.ts               已移除 APP_VERSION 硬编码（运行时 getVersion()）
  - package.json                   本脚本写入
  - AGENT.md 标题                  本脚本写入
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "src-tauri" / "Cargo.toml"
CARGO_LOCK = ROOT / "src-tauri" / "Cargo.lock"
PACKAGE_JSON = ROOT / "package.json"
AGENT_MD = ROOT / "AGENT.md"


def read_cargo_version() -> str:
    text = CARGO_TOML.read_text(encoding="utf-8")
    # 只匹配 [package] 段内的 version（首个 version = "x.y.z"）
    m = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', text, re.M)
    if not m:
        sys.exit(f"[sync_version] {CARGO_TOML} 中未找到 version")
    return m.group(1)


def write_cargo_version(version: str) -> None:
    text = CARGO_TOML.read_text(encoding="utf-8")
    new, n = re.subn(
        r'^(version\s*=\s*)"\d+\.\d+\.\d+"',
        rf'\g<1>"{version}"',
        text,
        count=1,
        flags=re.M,
    )
    if n != 1:
        sys.exit("[sync_version] Cargo.toml version 行替换失败")
    CARGO_TOML.write_text(new, encoding="utf-8", newline="\n")


def sync_package_json(version: str) -> None:
    data = json.loads(PACKAGE_JSON.read_text(encoding="utf-8"))
    data["version"] = version
    PACKAGE_JSON.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def sync_agent_md(version: str) -> None:
    text = AGENT_MD.read_text(encoding="utf-8")
    new, n = re.subn(r"(^# AGENT\.md — .* v)\d+\.\d+\.\d+", rf"\g<1>{version}", text, count=1, flags=re.M)
    if n == 1:
        AGENT_MD.write_text(new, encoding="utf-8", newline="\n")
    else:
        print("[sync_version] 警告：AGENT.md 标题未匹配到版本号（跳过）")


def sync_cargo_lock() -> None:
    """让 Cargo.lock 与 Cargo.toml 对齐（只更新本 crate 条目，不动其他依赖）。"""
    r = subprocess.run(["cargo", "update", "-p", "ai-work-assistant"], cwd=ROOT / "src-tauri")
    if r.returncode != 0:
        sys.exit("[sync_version] cargo update -p 失败，请手动运行 cargo check 对齐 Cargo.lock")


def main() -> None:
    if len(sys.argv) > 1:
        target = sys.argv[1]
        if not re.fullmatch(r"\d+\.\d+\.\d+", target):
            sys.exit(f"[sync_version] 非法版本号: {target}（需 x.y.z）")
        write_cargo_version(target)
        sync_cargo_lock()

    version = read_cargo_version()
    sync_package_json(version)
    sync_agent_md(version)
    print(f"[sync_version] 版本已同步为 {version}：package.json / AGENT.md / Cargo.lock 已对齐")
    print("[sync_version] tauri.conf.json 与 about.ts 无需改动（自动跟随 Cargo.toml）")


if __name__ == "__main__":
    main()
