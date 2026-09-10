#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""WorkBuddy UI 坐标点击签到兜底（F-18，批次4 T4.2）。

无 API 可用时的最后手段：驱动鼠标对 WorkBuddy 客户端签到按钮做坐标点击。
红线：仅手动触发、默认关闭（settings.ui_click_enabled）；坐标需用户预先配置；
全程零 token 输出；单次执行只点击一次，不做循环连点。

用法（stdout 末行 JSON，Rust 捕获）：
  python workbuddy_ui_click.py --capture          # 3 秒后记录当前鼠标坐标（取点）
  python workbuddy_ui_click.py --click --x N --y N  # 移动并点击
输出：{"ok":true,"x":..,"y":..,"message":".."} / {"ok":false,"message":".."}
"""

import argparse
import ctypes
import ctypes.wintypes
import json
import sys
import time

# Windows user32（ctypes 标准库，零新依赖红线）
user32 = ctypes.WinDLL("user32", use_last_error=True)

MOVE_DELAY_S = 0.35      # 移动后停顿，给悬浮反馈留时间
CAPTURE_DELAY_S = 3      # 取点倒计时
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004


def get_cursor_pos():
    pt = ctypes.wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(pt))
    return int(pt.x), int(pt.y)


def move_to(x, y):
    user32.SetCursorPos(int(x), int(y))


def left_click():
    user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.05)
    user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--capture", action="store_true", help="3 秒后记录当前鼠标坐标（取点模式）")
    ap.add_argument("--click", action="store_true", help="移动到坐标并左键单击")
    ap.add_argument("--x", type=int, default=0)
    ap.add_argument("--y", type=int, default=0)
    args = ap.parse_args()

    try:
        if args.capture:
            time.sleep(CAPTURE_DELAY_S)
            x, y = get_cursor_pos()
            print(json.dumps({"ok": True, "x": x, "y": y,
                              "message": "已记录坐标 (%d, %d)" % (x, y)}, ensure_ascii=False))
            return
        if args.click:
            if args.x <= 0 or args.y <= 0:
                print(json.dumps({"ok": False, "message": "坐标无效（需正整数屏幕坐标）"}, ensure_ascii=False))
                return
            move_to(args.x, args.y)
            time.sleep(MOVE_DELAY_S)
            left_click()
            time.sleep(0.2)
            print(json.dumps({"ok": True, "x": args.x, "y": args.y,
                              "message": "已点击 (%d, %d)，请查看客户端签到结果" % (args.x, args.y)},
                             ensure_ascii=False))
            return
        print(json.dumps({"ok": False, "message": "未指定动作（--capture 或 --click）"}, ensure_ascii=False))
    except Exception as e:
        print(json.dumps({"ok": False, "message": "执行失败: %s" % e}, ensure_ascii=False))


if __name__ == "__main__":
    main()
