#!/usr/bin/env python3
"""GTK3 输入法探针：一个输入框，把落进缓冲区的文本实时打到标准输出。

用途：配合 tools/uinput-inject 注入真实按键，验证「第三方应用到底收到了什么」。
GTK3 走 im-fcitx5.so（DBus），与 gnome 系应用、GIMP 等同一路径。

用法:
  python3 tools/gtk3-probe.py [--seconds 12] [--out /tmp/probe.txt]

输出行:
  READY            窗口已显示
  BUFFER <text>    输入框内容变化（只含已提交文本，不含 preedit）
  FINAL <text>     退出时的最终内容
"""
import argparse
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--seconds", type=float, default=12.0)
ap.add_argument("--out", default="/tmp/gtk3-probe-final.txt")
ap.add_argument("--title", default="xime-probe")
args = ap.parse_args()

GLib.set_prgname("xime-probe")
GLib.set_application_name("xime-probe")

win = Gtk.Window(title=args.title)
win.set_wmclass("xime-probe", "xime-probe")
win.set_default_size(420, 120)
win.set_position(Gtk.WindowPosition.CENTER_ALWAYS)

box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
label = Gtk.Label(label="输入测试")
entry = Gtk.Entry()
entry.set_size_request(380, -1)
box.pack_start(label, False, False, 0)
box.pack_start(entry, False, False, 0)
win.add(box)

state = {"text": ""}


def on_changed(e):
    t = e.get_text()
    if t != state["text"]:
        state["text"] = t
        print(f"BUFFER {t}", flush=True)
        # 每次变化都落盘，外部不必等进程退出就能读到结果
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(t)


entry.connect("changed", on_changed)
entry.connect("activate", lambda e: print(f"ACTIVATE {e.get_text()}", flush=True))


def finish():
    print(f"FINAL {state['text']}", flush=True)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(state["text"])
    Gtk.main_quit()
    return False


win.connect("destroy", lambda *_: finish())
win.show_all()
entry.grab_focus()
print("READY", flush=True)
GLib.timeout_add(int(args.seconds * 1000), finish)
Gtk.main()
sys.exit(0)
