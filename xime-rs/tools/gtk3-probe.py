#!/usr/bin/env python3
"""GTK3 输入法探针：一个输入框，把缓冲区文本、光标位置、预编辑串实时记录下来。

用途：配合 tools/uinput-inject 注入真实按键，验证「第三方应用到底收到了什么」。
GTK3 走 im-fcitx5.so（DBus），与 gnome 系应用、GIMP 等同一路径。

用法:
  python3 tools/gtk3-probe.py [--seconds 12] [--out /tmp/probe.txt]

输出:
  标准输出实时打印；同时追加到 <out>.events，便于进程还在跑的时候随时 tail。

事件类型:
  READY                     窗口已显示
  STATE text=.. pos=.. sel=..   文本/光标/选区变化（pos 是 GtkEntry 的 cursor-position）
  PREEDIT '..'              预编辑串变化
  DELETE args=[..]          控件收到删除请求
  FINAL <text> pos=<n>      退出时的最终内容

观测要点：输入法把光标改到行首时，STATE 里 pos 会直接从末尾变成 0。
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
win.set_default_size(420, 120)
win.set_position(Gtk.WindowPosition.CENTER_ALWAYS)

box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
entry = Gtk.Entry()
entry.set_size_request(380, -1)
box.pack_start(Gtk.Label(label="输入测试"), False, False, 0)
box.pack_start(entry, False, False, 0)
win.add(box)

state = {"text": "", "pos": -1, "sel_s": "-", "preedit": ""}
events_path = args.out + ".events"
# 每次启动清空事件文件，避免读到上一轮的残留
open(events_path, "w", encoding="utf-8").close()


def log(line):
    print(line, flush=True)
    with open(events_path, "a", encoding="utf-8") as f:
        f.write(line + "\n")


def dump(tag=""):
    """打印一次完整状态。tag 用来区分触发原因。

    GtkEntry 的 changed 信号会比光标位置更新**更早**触发，所以这里再挂一个
    idle 采样：立刻读一次是「中间态」，空闲时读一次才是用户真正看到的稳态。
    只报中间态会把正常行为误判成「光标跳到行首」。
    """
    sample(tag)

    def later():
        sample(tag + "+idle")
        return False

    GLib.idle_add(later)


def sample(tag=""):
    t = entry.get_text()
    pos = entry.get_position()
    sel = entry.get_selection_bounds()
    # PyGObject 里没有选区时返回空元组，有选区时返回 (True, start, end)
    sel_s = f"{sel[1]}-{sel[2]}" if len(sel) == 3 else "-"
    if (t, pos, sel_s) != (state["text"], state["pos"], state["sel_s"]):
        state["text"], state["pos"], state["sel_s"] = t, pos, sel_s
        log(f"STATE text={t!r} pos={pos} sel={sel_s} {tag}".rstrip())
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(f"text={t}\npos={pos}\nsel={sel_s}\n")


entry.connect("changed", lambda e: dump("changed"))
entry.connect("notify::cursor-position", lambda e, p: dump("cursor"))
entry.connect("activate", lambda e: log(f"ACTIVATE {e.get_text()}"))


def on_preedit(_entry):
    # 信号参数是 GtkEntry 本身，preedit 要从它内部的 IM context 取
    s = entry.get_im_context().get_preedit_string()[0]
    if s != state["preedit"]:
        state["preedit"] = s
        log(f"PREEDIT {s!r}")


entry.connect("preedit-changed", on_preedit)
entry.connect("delete-from-cursor", lambda e, n, u: log(f"DELETE args=[{n},{u}]") or False)


def finish():
    log(f"FINAL {state['text']} pos={state['pos']}")
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(f"text={state['text']}\npos={state['pos']}\n")
    Gtk.main_quit()
    return False


win.connect("destroy", lambda *_: finish())
win.show_all()
entry.grab_focus()
log("READY")
dump("init")
GLib.timeout_add(int(args.seconds * 1000), finish)
Gtk.main()
sys.exit(0)
