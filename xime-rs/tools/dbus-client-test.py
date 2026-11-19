#!/usr/bin/env python3
"""xime DBus 前端的端到端测试客户端。

模拟 fcitx5-qt 的行为：创建输入上下文 -> 打字 -> 检查 CommitString 与候选。
用法: python3 /tmp/dbus_test.py [拼音]
"""
import sys
import dbus
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

DBusGMainLoop(set_as_default=True)

SERVICE = "org.fcitx.Fcitx5"
IM_PATH = "/org/freedesktop/portal/inputmethod"
IF_IM = "org.fcitx.Fcitx.InputMethod1"
IF_IC = "org.fcitx.Fcitx.InputContext1"

word = sys.argv[1] if len(sys.argv) > 1 else "nihao"

bus = dbus.SessionBus()
im = bus.get_object(SERVICE, IM_PATH)
path, uuid = im.CreateInputContext([("program", "xime-test")],
                                   dbus_interface=IF_IM)
print(f"IC 路径: {path}  uuid {len(bytes(uuid))} 字节")

ic = bus.get_object(SERVICE, path)
iface = dbus.Interface(ic, IF_IC)

commits = []
uis = []


def on_commit(s):
    commits.append(str(s))
    print(f"  [CommitString] {s!r}")


def on_ui(preedit, cursorpos, aux_up, aux_down, cands, idx, hint, hp, hn):
    words = [str(c[0]) for c in cands]
    pre = "".join(str(p[0]) for p in preedit)
    uis.append((pre, words, int(idx)))
    print(f"  [UI] 组字={pre!r} 候选={words[:6]} 选中={int(idx)} hint={int(hint)}")


bus.add_signal_receiver(on_commit, signal_name="CommitString",
                        dbus_interface=IF_IC, path=path)
bus.add_signal_receiver(on_ui, signal_name="UpdateClientSideUI",
                        dbus_interface=IF_IC, path=path)

iface.FocusIn()
print(f"输入 {word!r}:")

for ch in word:
    kv = ord(ch)
    consumed = iface.ProcessKeyEvent(
        dbus.UInt32(kv), dbus.UInt32(0), dbus.UInt32(0),
        dbus.Boolean(True), dbus.UInt32(0))
    print(f"  按下 {ch!r} -> 消费={bool(consumed)}")
    iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(0), dbus.UInt32(0),
                          dbus.Boolean(False), dbus.UInt32(0))

print("按空格选首候选:")
consumed = iface.ProcessKeyEvent(dbus.UInt32(0x20), dbus.UInt32(0),
                                 dbus.UInt32(0), dbus.Boolean(True),
                                 dbus.UInt32(0))
print(f"  空格 -> 消费={bool(consumed)}")

loop = GLib.MainLoop()
GLib.timeout_add(600, lambda: (loop.quit(), False)[1])
loop.run()

print()
print(f"结果: CommitString {commits}  候选推送 {len(uis)} 次")
if commits:
    print("PASS")
else:
    print("FAIL: 没有任何 CommitString")
