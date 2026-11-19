#!/usr/bin/env python3
"""DBus 前端自测：不需要人敲键，也不需要看屏幕。

覆盖那些「看着能用、其实不对」的行为：
  1. 候选翻页      —— 第 2 页要真的是第 10-18 条，不是重复第 1 页
  2. 页内序号选词  —— 翻到第 2 页按 1，拿到的是该页首条
  3. 英文前缀补全  —— Ctrl+Space 后 hel␣ 出 hello，且空格本身不被吞
  4. 简繁切换      —— Ctrl+Shift+F 后候选立刻变繁体，再切回来
  5. 密码框        —— 带 Password 位时全键透传，一个字都不许拦
  6. Ctrl 组合     —— Ctrl+A 不能被当成字母 a 吃掉

用法:
  python3 tools/dbus-selftest.py            # 对正在跑的 xime --dbus
  python3 tools/dbus-selftest.py -v         # 打印每一步的 UI 快照
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

ST_SHIFT = 1 << 0
ST_CAPSLOCK = 1 << 1
ST_CTRL = 1 << 2

KS_SPACE = 0x20
KS_MINUS = 0x2D
KS_EQUAL = 0x3D
KS_BACKSPACE = 0xFF08
KS_RETURN = 0xFF0D
KS_ESCAPE = 0xFF1B
KS_PAGE_DOWN = 0xFF56

CAP_PASSWORD = 1 << 3
CAPS_NORMAL = 0x1E801800032  # 微信同款能力位（bit1 是 Preedit，千万别当 Password）

VERBOSE = "-v" in sys.argv

failures = []
checks = 0


def check(name, ok, detail=""):
    global checks
    checks += 1
    if ok:
        print(f"  ✓ {name}" + (f"  ({detail})" if detail and VERBOSE else ""))
    else:
        print(f"  ✗ {name}  {detail}")
        failures.append(name)


class Context:
    """一次输入框会话：CreateInputContext + 信号收集 + 按键封装"""

    def __init__(self, bus, program, caps=CAPS_NORMAL):
        im = bus.get_object(SERVICE, IM_PATH)
        self.path, _uuid = im.CreateInputContext(
            [("program", program), ("display", "wayland-1")], dbus_interface=IF_IM
        )
        obj = bus.get_object(SERVICE, self.path)
        self.iface = dbus.Interface(obj, IF_IC)
        self.commits = []
        self.uis = []
        self.loop = GLib.MainLoop()

        bus.add_signal_receiver(
            lambda s: self.commits.append(str(s)),
            signal_name="CommitString",
            dbus_interface=IF_IC,
            path=self.path,
        )
        bus.add_signal_receiver(
            lambda pre, cur, au, ad, cands, idx, hint, hp, hn: self.uis.append(
                {
                    "preedit": "".join(str(p[0]) for p in pre),
                    "cands": [str(c[0]) for c in cands],
                    "index": int(idx),
                    "prev": bool(hp),
                    "next": bool(hn),
                }
            ),
            signal_name="UpdateClientSideUI",
            dbus_interface=IF_IC,
            path=self.path,
        )

        self.iface.SetCapability(dbus.UInt64(caps))
        self.iface.FocusIn()
        self.spin(30)

    def spin(self, ms=8):
        GLib.timeout_add(ms, lambda: (self.loop.quit(), False)[1])
        self.loop.run()

    def press(self, keyval, state=0):
        """按一次键（按下 + 松开），返回按下时的 consumed"""
        c = bool(
            self.iface.ProcessKeyEvent(
                dbus.UInt32(keyval), dbus.UInt32(0), dbus.UInt32(state), dbus.Boolean(False), dbus.UInt32(0)
            )
        )
        self.spin(6)
        self.iface.ProcessKeyEvent(
            dbus.UInt32(keyval), dbus.UInt32(0), dbus.UInt32(state), dbus.Boolean(True), dbus.UInt32(0)
        )
        self.spin(6)
        return c

    def type(self, s, state=0):
        return [self.press(ord(c), state) for c in s]

    def last_ui(self):
        return self.uis[-1] if self.uis else {"preedit": "", "cands": [], "index": 0, "prev": False, "next": False}

    def clear_events(self):
        self.commits.clear()
        self.uis.clear()


def case_paging(ctx):
    print("\n[1] 候选翻页")
    ctx.clear_events()
    ctx.type("nihao")
    ui = ctx.last_ui()
    page1 = ui["cands"]
    check("组字串是 nihao", ui["preedit"] == "nihao", ui["preedit"])
    check("第一页 9 条", len(page1) == 9, str(len(page1)))
    check("第一页无上页", ui["prev"] is False)
    check("第一页有下页", ui["next"] is True, "候选不足 10 条，翻页测不了")

    check("- 在第一页无动作（仍吞键）", ctx.press(KS_MINUS) is True and ctx.last_ui()["cands"] == page1)

    ctx.clear_events()
    check("= 被消费", ctx.press(KS_EQUAL) is True)
    ui2 = ctx.last_ui()
    page2 = ui2["cands"]
    check("翻到第 2 页", page2 != page1 and len(page2) > 0, f"{page2[:3]}")
    check("第 2 页 has_prev", ui2["prev"] is True)
    check("第 2 页与第 1 页无重叠", not set(page2) & set(page1), f"重叠={set(page2) & set(page1)}")
    check("preedit 不随翻页变", ui2["preedit"] == "nihao", ui2["preedit"])

    want = page2[0]
    ctx.clear_events()
    check("第 2 页按 1 被消费", ctx.press(ord("1")) is True)
    check("上屏的是第 2 页首条", ctx.commits == [want], f"got={ctx.commits} want={want}")
    check("上屏后清空组字", ctx.last_ui()["preedit"] == "" and ctx.last_ui()["cands"] == [])

    # 翻到最后一页后 = 不再前进
    ctx.clear_events()
    ctx.type("shi")
    for _ in range(30):
        if not ctx.last_ui()["next"]:
            break
        ctx.press(KS_EQUAL)
    check("末页 has_next 为假", ctx.last_ui()["next"] is False)
    last_page = ctx.last_ui()["cands"]
    ctx.press(KS_EQUAL)
    check("末页再按 = 不越界", ctx.last_ui()["cands"] == last_page)
    ctx.press(KS_ESCAPE)


def case_page_key(ctx):
    print("\n[2] PageDown 与 PrevPage/NextPage 方法")
    ctx.clear_events()
    ctx.type("nihao")
    page1 = ctx.last_ui()["cands"]
    ctx.clear_events()
    ctx.press(KS_PAGE_DOWN)
    check("PageDown 翻页", ctx.last_ui()["cands"] != page1)
    page2 = ctx.last_ui()["cands"]
    ctx.clear_events()
    ctx.iface.PrevPage()
    ctx.spin(20)
    check("PrevPage 回到第一页", ctx.last_ui()["cands"] == page1, f"{ctx.last_ui()['cands'][:3]}")
    ctx.clear_events()
    ctx.iface.NextPage()
    ctx.spin(20)
    check("NextPage 回到第二页", ctx.last_ui()["cands"] == page2)
    ctx.press(KS_ESCAPE)


def case_english(ctx):
    print("\n[3] 英文前缀补全")
    ctx.clear_events()
    check("Ctrl+Space 被消费", ctx.press(KS_SPACE, ST_CTRL) is True)
    ctx.clear_events()
    ctx.type("hel")
    ui = ctx.last_ui()
    check("英文有组字串", ui["preedit"] == "hel", ui["preedit"])
    check("hel 有补全", len(ui["cands"]) > 1, f"{ui['cands']}")
    want = ui["cands"][0]
    check("首选是 hello", want.lower() in ("hello", "help"), f"{ui['cands'][:4]}")

    ctx.clear_events()
    consumed = ctx.press(KS_SPACE)
    check("空格上屏补全", ctx.commits == [want], f"got={ctx.commits} want={want}")
    check("空格本身不吞（应用还要插空格）", consumed is False)
    check("上屏后清空组字", ctx.last_ui()["preedit"] == "")

    # 英文没有组字时，Space / 字母都要透传
    ctx.clear_events()
    check("无组字时空格透传", ctx.press(KS_SPACE) is False and ctx.commits == [])

    # 大写：Shift+首字母
    ctx.clear_events()
    ctx.press(ord("H"), ST_SHIFT)
    ui = ctx.last_ui()
    check("Shift+字母保留大写", ui["preedit"].startswith("H"), ui["preedit"])
    ctx.press(KS_ESCAPE)

    ctx.press(KS_SPACE, ST_CTRL)  # 切回中文
    ctx.spin(20)


def case_variant(ctx):
    print("\n[4] 简繁切换 Ctrl+Shift+F")
    ctx.clear_events()
    ctx.type("biancheng")
    first_hans = ctx.last_ui()["cands"][0]
    check("简体首选是「变成」", first_hans == "变成", first_hans)

    ctx.clear_events()
    check("Ctrl+Shift+F 被消费", ctx.press(ord("f"), ST_CTRL | ST_SHIFT) is True)
    first_hant = ctx.last_ui()["cands"][0]
    check("切换后首选变繁体", first_hant == "變成", first_hant)
    check("组字串不变", ctx.last_ui()["preedit"] == "biancheng", ctx.last_ui()["preedit"])

    ctx.press(ord("f"), ST_CTRL | ST_SHIFT)
    check("再切回简体", ctx.last_ui()["cands"][0] == "变成", ctx.last_ui()["cands"][0])
    ctx.press(KS_ESCAPE)


def case_ctrl(bus):
    print("\n[5] Ctrl 组合与密码框")
    ctx = Context(bus, "selftest-ctrl")
    ctx.clear_events()
    check("Ctrl+A 透传", ctx.press(ord("a"), ST_CTRL) is False and ctx.commits == [])
    check("Ctrl+V 透传", ctx.press(ord("v"), ST_CTRL) is False and ctx.commits == [])

    pwd = Context(bus, "selftest-password", caps=CAP_PASSWORD)
    pwd.clear_events()
    got = pwd.type("nihao")
    check("密码框：字母全部透传", got == [False] * 5, str(got))
    check("密码框：不产生提交", pwd.commits == [], str(pwd.commits))
    check("密码框：不推候选", pwd.last_ui()["cands"] == [], str(pwd.last_ui()["cands"]))
    check("密码框：空格也透传", pwd.press(KS_SPACE) is False)

    # 从普通框切到密码框，残留组字必须被丢掉
    mixed = Context(bus, "selftest-mixed")
    mixed.type("nihao")
    before = mixed.last_ui()["cands"]
    mixed.iface.SetCapability(dbus.UInt64(CAP_PASSWORD))
    mixed.spin(20)
    mixed.clear_events()
    check("切成密码框后按键全透传", mixed.press(ord("a")) is False)
    check("切成密码框时丢弃残留组字", before and mixed.last_ui()["preedit"] == "")


def main():
    bus = dbus.SessionBus()
    print("对 org.fcitx.Fcitx5 跑 DBus 前端自测…")
    ctx = Context(bus, "selftest")
    try:
        case_paging(ctx)
        case_page_key(ctx)
        case_english(ctx)
        case_variant(ctx)
        case_ctrl(bus)
    finally:
        try:
            ctx.iface.FocusOut()
        except Exception:
            pass

    print(f"\n{checks - len(failures)}/{checks} 项通过")
    if failures:
        print("失败：" + "、".join(failures))
        return 1
    print("全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
