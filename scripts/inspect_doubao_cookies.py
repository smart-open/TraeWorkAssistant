# -*- coding: utf-8 -*-
"""临时诊断脚本：读取豆包各 Chromium profile 的 Cookies 库（仅 cookie 名与有效期，不读密文值）。"""
import os
import shutil
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

ud = Path(os.environ["LOCALAPPDATA"]) / "Doubao" / "User Data"
profiles = [p for p in ud.iterdir() if p.is_dir() and (p.name == "Default" or p.name.startswith("Profile"))]

for prof in profiles:
    net = prof / "Network"
    cf = net / "Cookies"
    print(f"=== {prof.name} === cookies_file={cf.is_file()}")
    if not cf.is_file():
        continue
    tmp = Path(tempfile.mkdtemp(prefix="aw_diag_"))
    try:
        for f in net.glob("Cookies*"):
            shutil.copy(f, tmp / f.name)
        con = sqlite3.connect(str(tmp / "Cookies"))
        try:
            rows = con.execute(
                "SELECT name, host_key, expires_utc, is_persistent FROM cookies WHERE host_key LIKE '%doubao.com'"
            ).fetchall()
            print(f"doubao cookies total={len(rows)}")
            for name, host, exp, pers in rows:
                if name in ("sessionid", "sid_guard", "sessionid_ss", "sid_tt", "uid_tt", "multi_sids"):
                    if exp:
                        remain = exp / 1_000_000 - time.time() - 11644473600
                        rem = f"{remain/3600:.1f}h"
                    else:
                        rem = "session-only"
                    print(f"  {name}  host={host}  remain={rem}  persistent={pers}")
        finally:
            con.close()
    except Exception as e:  # noqa: BLE001
        print(f"  read failed: {e}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
print("done", file=sys.stderr)
