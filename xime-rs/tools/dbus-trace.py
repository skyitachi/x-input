#!/usr/bin/env python3
"""DBus 输入法差分录制器。

模拟 fcitx5-qt 客户端的完整握手，然后按给定序列敲键，把「返回值 + 收到的
全部信号」按顺序录成 JSON。对真 fcitx5 与 xime --dbus 各录一份再 diff，
就能定位行为偏差（不只是「能不能出字」，还包括组字串、候选、消费标记）。

用法:
  python3 tools/dbus-trace.py out.json [按键序列]

按键序列语法:
  小写字母      普通字母键
  _             空格
  .  ,         对应标点
  <bs>          退格
  <esc>         Esc
  <enter>       回车
  <1>..<9>      数字选词
  <shift>       单击 Shift（按下+松开）
  <tab>         Tab
  <ctrl-space>  Ctrl+空格

每个键都会发 press 与 release（Shift 单独处理），press 的返回值记为 consumed。
"""
import json
import sys
import time

import dbus
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

DBusGMainLoop(set_as_default=True)

SERVICE = "org.fcitx.Fcitx5"
IM_PATH = "/org/freedesktop/portal/inputmethod"
IF_IM = "org.fcitx.Fcitx.InputMethod1"
IF_IC = "org.fcitx.Fcitx.InputContext1"

# fcitx5 flags.h / FcitxKeyState
ST_SHIFT = 1 << 0
ST_CTRL = 1 << 2

KS_SPACE = 0x20
KS_BACKSPACE = 0xFF08
KS_TAB = 0xFF09
KS_RETURN = 0xFF0D
KS_ESCAPE = 0xFF1B
KS_SHIFT_L = 0xFFE1

SPECIAL = {
    "<bs>": (KS_BACKSPACE, 0),
    "<esc>": (KS_ESCAPE, 0),
    "<enter>": (KS_RETURN, 0),
    "<tab>": (KS_TAB, 0),
}
for _d in range(1, 10):
    SPECIAL[f"<{_d}>"] = (0x30 + _d, 0)

# fcitx5-qt 声明支持的能力位：Preedit|Password|FormattedPreedit|... 取微信同款量级
CAPS = 0x1E801800032


def parse(seq):
    """把序列串拆成 [(label, keyval, state, kind)]"""
    out = []
    i = 0
    while i < len(seq):
        if seq[i] == "<":
            j = seq.index(">", i)
            tok = seq[i : j + 1]
            i = j + 1
            if tok == "<shift>":
                out.append(("shift", KS_SHIFT_L, 0, "shift"))
            elif tok == "<ctrl-space>":
                out.append(("ctrl-space", KS_SPACE, ST_CTRL, "key"))
            elif tok in SPECIAL:
                kv, st = SPECIAL[tok]
                out.append((tok, kv, st, "key"))
            else:
                raise SystemExit(f"未知标记 {tok}")
        elif seq[i] == "_":
            out.append(("space", KS_SPACE, 0, "key"))
            i += 1
        else:
            ch = seq[i]
            out.append((ch, ord(ch), 0, "key"))
            i += 1
    return out


def main():
    out_path = sys.argv[1] if len(sys.argv) > 1 else "trace.json"
    seq = sys.argv[2] if len(sys.argv) > 2 else "nihao_"

    bus = dbus.SessionBus()
    im = bus.get_object(SERVICE, IM_PATH)
    path, uuid = im.CreateInputContext(
        [("program", "xime-trace"), ("display", "wayland-1")], dbus_interface=IF_IM
    )

    ic = bus.get_object(SERVICE, path)
    iface = dbus.Interface(ic, IF_IC)

    events = []

    def rec(name, payload):
        events.append({"t": round(time.time() * 1000) % 100000, "sig": name, "data": payload})

    def on_commit(s):
        rec("CommitString", str(s))

    def on_ui(preedit, cursorpos, aux_up, aux_down, cands, idx, hint, hp, hn):
        rec(
            "UpdateClientSideUI",
            {
                "preedit": "".join(str(p[0]) for p in preedit),
                "preedit_segs": [[str(p[0]), int(p[1])] for p in preedit],
                "cursorpos": int(cursorpos),
                "cands": [str(c[0]) for c in cands],
                "index": int(idx),
                "hint": int(hint),
                "prev": bool(hp),
                "next": bool(hn),
            },
        )

    def on_forward(keyval, state, typ):
        rec("ForwardKey", {"keyval": int(keyval), "state": int(state), "type": int(typ)})

    def on_delete(before, after):
        rec("DeleteSurroundingText", {"before": int(before), "after": int(after)})

    def on_preedit(text, cursor):
        # xime 的 UpdateFormattedPreedit 签名是 (Vec<(String, i32)>, i32 cursorpos)，
        # 只有两个参数；cursorpos 是预编辑内部光标偏移。
        rec("UpdateFormattedPreedit", {
            "text": "".join(str(t[0]) for t in text),
            "cursorpos": int(cursor),
        })

    bus.add_signal_receiver(on_commit, signal_name="CommitString", dbus_interface=IF_IC, path=path)
    bus.add_signal_receiver(on_ui, signal_name="UpdateClientSideUI", dbus_interface=IF_IC, path=path)
    bus.add_signal_receiver(on_forward, signal_name="ForwardKey", dbus_interface=IF_IC, path=path)
    bus.add_signal_receiver(
        on_delete, signal_name="DeleteSurroundingText", dbus_interface=IF_IC, path=path
    )
    bus.add_signal_receiver(
        on_preedit, signal_name="UpdateFormattedPreedit", dbus_interface=IF_IC, path=path
    )

    # 客户端的握手：能力、光标、周边文本、FocusIn
    proto = []
    try:
        iface.SetSupportedCapability(dbus.UInt64(CAPS))
        proto.append("SetSupportedCapability ok")
    except Exception as e:
        proto.append(f"SetSupportedCapability ERR {e}")
    try:
        iface.SetCapability(dbus.UInt64(CAPS))
        proto.append("SetCapability ok")
    except Exception as e:
        proto.append(f"SetCapability ERR {e}")
    try:
        iface.SetCursorRectV2(dbus.Int32(0), dbus.Int32(0), dbus.Int32(4), dbus.Int32(20), dbus.Double(1.0))
        proto.append("SetCursorRectV2 ok")
    except Exception as e:
        proto.append(f"SetCursorRectV2 ERR {e}")
    try:
        iface.SetSurroundingText("", dbus.UInt32(0), dbus.UInt32(0))
        proto.append("SetSurroundingText ok")
    except Exception as e:
        proto.append(f"SetSurroundingText ERR {e}")
    try:
        iface.FocusIn()
        proto.append("FocusIn ok")
    except Exception as e:
        proto.append(f"FocusIn ERR {e}")

    # 探测客户端可能用到的其它方法，记录是否被实现
    probes = {}
    for m, args in [
        ("Reset", []),
        ("PrevPage", []),
        ("NextPage", []),
        ("InvokeAction", [dbus.UInt32(0), dbus.Int32(-1)]),
        ("SelectCandidate", [dbus.Int32(0)]),
        ("IsVirtualKeyboardVisible", []),
        ("SetSurroundingTextPosition", [dbus.UInt32(0), dbus.UInt32(0)]),
    ]:
        try:
            getattr(iface, m)(*args)
            probes[m] = "ok"
        except dbus.exceptions.DBusException as e:
            probes[m] = f"ERR {e.get_dbus_name()}"
    # Reset / SelectCandidate 会改状态，握手后立刻重来一次干净的开始
    try:
        iface.Reset()
        iface.SetSurroundingText("", dbus.UInt32(0), dbus.UInt32(0))
        iface.FocusIn()
    except Exception:
        pass

    def pump(ms=6):
        loop.run()
        _ = ms

    loop = GLib.MainLoop()

    def spin(ms):
        GLib.timeout_add(ms, lambda: (loop.quit(), False)[1])
        loop.run()

    steps = []
    for label, kv, st, kind in parse(seq):
        if kind == "shift":
            c1 = bool(iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(50), dbus.UInt32(st), dbus.Boolean(False), dbus.UInt32(0)))
            spin(4)
            c2 = bool(iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(50), dbus.UInt32(st), dbus.Boolean(True), dbus.UInt32(0)))
            spin(4)
            steps.append({"key": label, "press_consumed": c1, "release_consumed": c2})
            continue
        # 第四个参数是 isRelease：false=按下，true=松开（与 fcitx5-qt 一致）
        c1 = bool(iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(0), dbus.UInt32(st), dbus.Boolean(False), dbus.UInt32(0)))
        spin(4)
        c2 = bool(iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(0), dbus.UInt32(st), dbus.Boolean(True), dbus.UInt32(0)))
        spin(4)
        # 客户端会把「已提交的文本」回填进周边文本，这里简化：直接累积 commit
        steps.append({"key": label, "press_consumed": c1, "release_consumed": c2})

    spin(120)

    # 收尾：读一下 introspect，记录对方实现了哪些方法
    iface_xml = ""
    try:
        iface_xml = str(
            ic.Introspect(dbus_interface="org.freedesktop.DBus.Introspectable")
        )
    except Exception as e:
        iface_xml = f"introspect ERR {e}"

    def methods_of(xml):
        import re

        out = []
        for m in re.finditer(r"<method name=\"([^\"]+)\"", xml):
            out.append(m.group(1))
        return sorted(out)

    def signals_of(xml):
        import re

        return sorted(re.findall(r"<signal name=\"([^\"]+)\"", xml))

    result = {
        "target": SERVICE,
        "ic_path": str(path),
        "sequence": seq,
        "handshake": proto,
        "probes": probes,
        "ic_methods": methods_of(iface_xml),
        "ic_signals": signals_of(iface_xml),
        "steps": steps,
        "events": events,
        "commits": [e["data"] for e in events if e["sig"] == "CommitString"],
        "final_preedit": "",
    }
    for e in reversed(events):
        if e["sig"] == "UpdateClientSideUI":
            result["final_preedit"] = e["data"]["preedit"]
            break

    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print(f"已写入 {out_path}: {len(events)} 个信号, 提交={result['commits']}")


if __name__ == "__main__":
    main()
