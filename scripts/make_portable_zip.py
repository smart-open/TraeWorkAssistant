#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""（已废弃的一次性脚本）重新打包 release 目录为 portable zip。
现在统一使用 package_portable.py（自动读取 tauri.conf.json 命名为
AI Work 助手_<version>_x64_portable.zip）。本脚本仅保留兼容入口。
"""
import runpy
import os
import sys

if __name__ == "__main__":
    print("提示：make_portable_zip.py 已由 package_portable.py 取代，转交执行...\n")
    pkg = os.path.join(os.path.dirname(os.path.abspath(__file__)), "package_portable.py")
    sys.argv = [pkg]
    runpy.run_path(pkg, run_name="__main__")
