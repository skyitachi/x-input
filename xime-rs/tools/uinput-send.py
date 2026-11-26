#!/usr/bin/env python3
"""通过 /dev/uinput 造一个临时键盘，按脚本注入按键，并可在过程中截图。

用法: uinput-send.py shift sleep:0.6 text:nihao sleep:1.2 shot:/tmp/a.png space enter

为什么不用 wtype：wtype 会创建 zwp_virtual_keyboard 并上传自己的 keymap，
合成器会把这份 keymap 转发给正在抓键盘的输入法；xime 的手写 XKB 解析器
读不懂那份精简 keymap（"xkb_keycodes (unnamed)"），按键映射随即错位。
uinput 走真实的 evdev -> libinput -> 合成器路径，keymap 与真键盘一致。
"""

import fcntl
import json
import os
import struct
import subprocess
import sys
import time

EV_SYN, EV_KEY = 0, 1
UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502

KEY = {
    "a": 30, "b": 48, "c": 46, "d": 32, "e": 18, "f": 33, "g": 34, "h": 35,
    "i": 23, "j": 36, "k": 37, "l": 38, "m": 50, "n": 49, "o": 24, "p": 25,
    "q": 16, "r": 19, "s": 31, "t": 20, "u": 22, "v": 47, "w": 17, "x": 45,
    "y": 21, "z": 44,
    "0": 11, "1": 2, "2": 3, "3": 4, "4": 5, "5": 6,
    "6": 7, "7": 8, "8": 9, "9": 10,
    "space": 57, "enter": 28, "minus": 12, "equal": 13,
    "backspace": 14, "tab": 15, "escape": 1,
}
SHIFT = 42
CTRL = 29


def ev(fd, typ, code, value):
    os.write(fd, struct.pack("qqHHi", 0, 0, typ, code, value))
    os.write(fd, struct.pack("qqHHi", 0, 0, EV_SYN, 0, 0))


def tap(fd, code, hold=0.012):
    ev(fd, EV_KEY, code, 1)
    time.sleep(hold)
    ev(fd, EV_KEY, code, 0)


def shoot(path):
    """截目标窗口上方一段：候选窗可能画在窗口上沿之外。

    目标窗口类名默认 xime-foot-test，可用 XIME_TEST_CLASS 覆盖。
    """
    class_name = os.environ.get("XIME_TEST_CLASS", "xime-foot-test")
    geom = None
    try:
        out = subprocess.run(
            ["hyprctl", "clients", "-j"], capture_output=True, text=True, check=True
        )
        for c in json.loads(out.stdout):
            if c.get("class") == class_name:
                x, _ = c["at"]
                w, _ = c["size"]
                geom = f"{x},0 {w}x440"
                break
    except Exception as exc:  # noqa: BLE001
        print(f"取窗口几何失败: {exc}", file=sys.stderr)
    cmd = ["grim"] + (["-g", geom] if geom else []) + [path]
    subprocess.run(cmd, check=False)
    print(f"已抓 {path}" + (f"（-g {geom}）" if geom else "（全屏）"), file=sys.stderr)


def main():
    tokens = sys.argv[1:]
    if not tokens:
        print("用法: uinput-send.py shift text:nihao sleep:1 shot:/tmp/a.png space", file=sys.stderr)
        return 2

    fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
    for code in set(KEY.values()) | {SHIFT, 29}:
        fcntl.ioctl(fd, UI_SET_KEYBIT, code)
    # 旧式 uinput_user_dev 必须整块写满（80 名 + 8 input_id + 4 ff + 64×4×4 = 1116 字节），
    # 少写会 EINVAL。
    dev = struct.pack("80sHHHHi", b"xime-test-kbd", 0x03, 0x1, 0x1, 0x1, 0)
    os.write(fd, dev + b"\0" * (1116 - len(dev)))
    fcntl.ioctl(fd, UI_DEV_CREATE)
    time.sleep(0.8)  # 等合成器识别新设备

    try:
        for tok in tokens:
            if tok.startswith("sleep:"):
                time.sleep(float(tok.split(":", 1)[1]))
            elif tok.startswith("shot:"):
                shoot(tok.split(":", 1)[1])
            elif tok == "shift":
                tap(fd, SHIFT, 0.03)
            elif tok in ("ctrl-c", "ctrl-d", "ctrl-l"):
                ev(fd, EV_KEY, CTRL, 1)
                tap(fd, KEY[tok[-1]])
                ev(fd, EV_KEY, CTRL, 0)
            elif tok.startswith("text:"):
                for ch in tok.split(":", 1)[1]:
                    key = KEY.get(ch) or KEY.get(ch.lower())
                    if key is None:
                        print(f"跳过未知字符 {ch!r}", file=sys.stderr)
                        continue
                    upper = ch.isupper()
                    if upper:
                        ev(fd, EV_KEY, SHIFT, 1)
                    tap(fd, key)
                    if upper:
                        ev(fd, EV_KEY, SHIFT, 0)
            elif tok in KEY:
                tap(fd, KEY[tok])
            else:
                print(f"未知指令 {tok!r}", file=sys.stderr)
                return 2
        time.sleep(0.2)
    finally:
        fcntl.ioctl(fd, UI_DEV_DESTROY)
        os.close(fd)
    return 0


if __name__ == "__main__":
    sys.exit(main())
