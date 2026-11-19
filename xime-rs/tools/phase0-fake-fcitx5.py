#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Phase 0 假 fcitx5 守护进程 —— 只验证一件事：
「响应 org.fcitx.Fcitx5 的 DBus 协议，能否让微信（内置 fcitx5-qt 客户端）上屏」

不做组字、不做候选、不做词库。收到按键 -> 直接发一个固定字符串。

用法:
    python3 fake_fcitx5.py [commit|candidate|passthrough] [要上屏的文字]

    commit       收到按键 -> emit CommitString(文字) 并返回 True（吞掉按键）
    candidate    收到 a-z -> 攒组字串并推 UpdateClientSideUI（含 5 条候选）；
                 空格/回车 -> 提交首候选。用于验证「候选窗是否由应用自绘」
    passthrough  收到按键 -> 返回 False（按键原样交给应用），用于对照
"""
import os
import sys
import time
import uuid
import dbus
import dbus.service
import dbus.mainloop.glib
from gi.repository import GLib

LOG_PATH = "/tmp/fake_fcitx5.log"
SERVICE = "org.fcitx.Fcitx5"
ALT_SERVICE = "org.fcitx.Fcitx-0"
IM_PATH = "/org/freedesktop/portal/inputmethod"
CTL_PATH = "/controller"
IF_IM = "org.fcitx.Fcitx.InputMethod1"
IF_IC = "org.fcitx.Fcitx.InputContext1"
IF_CTL = "org.fcitx.Fcitx.Controller1"

MODE = sys.argv[1] if len(sys.argv) > 1 else "commit"
TEXT = sys.argv[2] if len(sys.argv) > 2 else "XIME接管成功"

_logf = open(LOG_PATH, "w", buffering=1)


def log(msg):
    ts = time.strftime("%H:%M:%S")
    _logf.write(f"[{ts}] {msg}\n")


dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)

addr = os.environ.get("FCITX_DBUS_ADDRESS")
if addr:
    log(f"警告: FCITX_DBUS_ADDRESS={addr}（客户端可能走私有总线）")
    bus = dbus.bus.BusConnection(addr)
else:
    bus = dbus.SessionBus()
log(f"已连接会话总线: {bus.get_unique_name()}")

names = []
for n in (SERVICE, ALT_SERVICE):
    try:
        names.append(dbus.service.BusName(n, bus, do_not_queue=True))
        log(f"已占用服务名 {n}")
    except dbus.exceptions.NameExistsException:
        log(f"!! 服务名 {n} 已被占用，退出")
        sys.exit(2)


class InputContext(dbus.service.Object):
    """一个输入框对应一个对象实例"""

    def __init__(self, bus_name, path, sender, args):
        super().__init__(bus_name, path)
        self.path = path
        self.sender = sender
        self.args = dict(args)
        self.focused = False
        self.key_count = 0
        self.buf = ""
        log(f"创建输入上下文 {path}  program={self.args.get('program')!r} "
            f"从 {sender}")
        # 客户端声明自己要画候选窗时，说明它会消费 UpdateClientSideUI
        log(f"   args={self.args}")

    # ---- 生命周期 ----
    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def FocusIn(self):
        self.focused = True
        log("FocusIn")

    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def FocusOut(self):
        self.focused = False
        log("FocusOut")

    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def Reset(self):
        log("Reset")

    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def DestroyIC(self):
        log("DestroyIC")
        self.remove_from_connection()

    # ---- 能力/光标 ----
    @dbus.service.method(IF_IC, in_signature="t", out_signature="")
    def SetSupportedCapability(self, caps):
        log(f"SetSupportedCapability 0x{int(caps):x}")

    @dbus.service.method(IF_IC, in_signature="t", out_signature="")
    def SetCapability(self, caps):
        log(f"SetCapability 0x{int(caps):x}")

    @dbus.service.method(IF_IC, in_signature="iiii", out_signature="")
    def SetCursorRect(self, x, y, w, h):
        log(f"SetCursorRect {x},{y} {w}x{h}")

    @dbus.service.method(IF_IC, in_signature="iiiid", out_signature="")
    def SetCursorRectV2(self, x, y, w, h, scale):
        log(f"SetCursorRectV2 {x},{y} {w}x{h} @{float(scale)}")

    @dbus.service.method(IF_IC, in_signature="suu", out_signature="")
    def SetSurroundingText(self, text, cursor, anchor):
        log(f"SetSurroundingText {text[:40]!r} cursor={int(cursor)}")

    @dbus.service.method(IF_IC, in_signature="uu", out_signature="")
    def SetSurroundingTextPosition(self, cursor, anchor):
        pass

    # ---- 核心：按键 ----
    @dbus.service.method(IF_IC, in_signature="uuubu", out_signature="b")
    def ProcessKeyEvent(self, keyval, keycode, state, ktype, tm):
        self.key_count += 1
        n = self.key_count
        if n <= 12 or n % 20 == 0:
            log(f"ProcessKeyEvent #{n} keyval=0x{int(keyval):x} "
                f"keycode={int(keycode)} state=0x{int(state):x} "
                f"press={bool(ktype)} focused={self.focused}")
        if MODE == "passthrough":
            return False
        if MODE == "candidate":
            return self._imitate_composition(keyval, ktype)
        # 实验：吞掉按键，并把固定文本送给客户端
        self.CommitString(TEXT)
        log(f"  -> CommitString({TEXT!r})")
        return True

    # ---- candidate 模式：模拟一次真实的组字，看微信会不会自己画候选窗 ----
    def _imitate_composition(self, keyval, ktype):
        kv = int(keyval)
        press = bool(ktype)
        # Step 1：字母 -> 攒进 preedit，并把候选表推给客户端
        if 0x61 <= kv <= 0x7A:  # a-z
            if not press:
                # 松开事件：按下已被消费，这里同样吞掉，但不能重复累积字符
                return True
            self.buf += chr(kv)
            self._push_ui()
            log(f"  -> UpdateClientSideUI preedit={self.buf!r} 候选 5 条")
            return True
        # Step 2：空格 / 回车 -> 提交首个候选
        if kv in (0x20, 0xFF0D):
            if not press:
                return True
            if self.buf:
                self.CommitString("你")
                log(f"  -> CommitString('你')（选首候选，丢弃 {self.buf!r}）")
                self.buf = ""
                self._push_ui()
            return True
        # Step 3：BackSpace -> 退格
        if kv == 0xFF08:
            if not press:
                return True
            if self.buf:
                self.buf = self.buf[:-1]
                self._push_ui()
            return True
        return False

    def _push_ui(self):
        # a(si) 的 i 是 TextFormatFlags：1 = Underline（组字串习惯加下划线）
        preedit = [(self.buf, 1)] if self.buf else []
        cands = [("你", ""), ("尼", ""), ("泥", ""), ("拟", ""), ("逆", "")]
        if not self.buf:
            cands = []
        # i = CandidateLayoutHint: 0=NotSet 1=Vertical 2=Horizontal
        self.UpdateClientSideUI(preedit, len(self.buf), [], [], cands,
                                0, 2, False, True)

    # ---- 翻页/选词/动作 ----
    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def PrevPage(self):
        pass

    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def NextPage(self):
        pass

    @dbus.service.method(IF_IC, in_signature="i", out_signature="")
    def SelectCandidate(self, index):
        log(f"SelectCandidate {int(index)}")

    @dbus.service.method(IF_IC, in_signature="ui", out_signature="")
    def InvokeAction(self, action, cursor):
        pass

    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def ShowVirtualKeyboard(self):
        pass

    @dbus.service.method(IF_IC, in_signature="", out_signature="")
    def HideVirtualKeyboard(self):
        pass

    @dbus.service.method(IF_IC, in_signature="", out_signature="b")
    def IsVirtualKeyboardVisible(self):
        return False

    # ---- 信号 ----
    @dbus.service.signal(IF_IC, signature="s")
    def CommitString(self, text):
        pass

    @dbus.service.signal(IF_IC, signature="sss")
    def CurrentIM(self, name, unique_name, lang):
        pass

    @dbus.service.signal(IF_IC, signature="a(si)i")
    def UpdateFormattedPreedit(self, preedit, cursor):
        pass

    @dbus.service.signal(IF_IC, signature="a(si)ia(si)a(si)a(ss)iibb")
    def UpdateClientSideUI(self, preedit, cursor, aux_up, aux_down,
                           candidates, cand_index, layout, has_prev, has_next):
        log(f"  -> UpdateClientSideUI 候选 {len(candidates)} 条")

    @dbus.service.signal(IF_IC, signature="iu")
    def DeleteSurroundingText(self, offset, nchar):
        pass

    @dbus.service.signal(IF_IC, signature="uub")
    def ForwardKey(self, keyval, state, ktype):
        pass

    @dbus.service.signal(IF_IC, signature="")
    def NotifyFocusOut(self):
        pass


class InputMethod(dbus.service.Object):
    def __init__(self, bus_name):
        super().__init__(bus_name, IM_PATH)
        self.bus_name = bus_name
        self.n = 0
        self.contexts = {}

    @dbus.service.method(IF_IM, in_signature="a(ss)", out_signature="oay",
                         sender_keyword="sender")
    def CreateInputContext(self, kvs, sender=None):
        self.n += 1
        path = f"/org/freedesktop/portal/inputcontext/{self.n}"
        u = uuid.uuid4().bytes
        log(f"== CreateInputContext -> {path}（来自 {sender}，"
            f"入参 {[(str(k), str(v)) for k, v in kvs]}）")
        ic = InputContext(self.bus_name, path, sender, kvs)
        self.contexts[path] = ic
        return dbus.ObjectPath(path), dbus.ByteArray(u)

    @dbus.service.method(IF_IM, in_signature="", out_signature="u")
    def Version(self):
        return dbus.UInt32(1)


class Controller(dbus.service.Object):
    def __init__(self, bus_name):
        super().__init__(bus_name, CTL_PATH)

    @dbus.service.method(IF_CTL, in_signature="", out_signature="i")
    def State(self):
        return dbus.Int32(2)  # 2 = 已激活

    @dbus.service.method(IF_CTL, in_signature="", out_signature="s")
    def CurrentInputMethod(self):
        return "pinyin"

    @dbus.service.method(IF_CTL, in_signature="s", out_signature="")
    def SetCurrentIM(self, name):
        log(f"SetCurrentIM {name}")

    @dbus.service.method(IF_CTL, in_signature="", out_signature="")
    def Activate(self):
        log("Controller.Activate")

    @dbus.service.method(IF_CTL, in_signature="", out_signature="")
    def Toggle(self):
        pass

    @dbus.service.method(IF_CTL, in_signature="", out_signature="")
    def Exit(self):
        log("Controller.Exit")


def main():
    bus_name = names[0]
    im = InputMethod(bus_name)
    ctl = Controller(bus_name)
    log(f"模式={MODE} 上屏文本={TEXT!r}；等待客户端连接…")
    loop = GLib.MainLoop()
    try:
        loop.run()
    except KeyboardInterrupt:
        log("退出")


if __name__ == "__main__":
    main()
