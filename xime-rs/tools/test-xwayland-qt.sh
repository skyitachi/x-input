#!/usr/bin/env bash
# 测试 X11/XWayland Qt 应用走「fcitx5-qt 模块 → xime --dbus」的完整链路。
#
# 背景：会话里有 QT_IM_MODULE=fcitx / GTK_IM_MODULE=fcitx 时，X11/XWayland 的 Qt
# 应用不会建 zwp_text_input_v3，而是加载 libfcitx5platforminputcontextplugin.so、
# 走 DBus 连 org.fcitx.Fcitx5。xime 用 --dbus 前端冒充 fcitx5 守护进程接住它。
# 本脚本把这条路端到端验证一遍，两层：
#
#   A. 协议层   —— dbus-trace.py 模拟 fcitx5-qt 客户端的完整握手 + 按键，
#                  断言 xime 回了正确的 CommitString。
#   B. 真实应用层 —— 编译并跑 wayland-ime/test/qt_probe.c（Qt6，xcb/XWayland），
#                  确认 fcitx5-qt 插件被加载、连上 xime --dbus，再对它自己的
#                  input context 发 ProcessKeyEvent（等价于在窗口里敲键），
#                  断言 Qt 控件最终渲染出提交的汉字。
#
# 用法:
#   xime-rs/tools/test-xwayland-qt.sh [序列] [期望提交]
#     默认序列 "nihao_"，期望提交 "你好"。序列语法同 tools/dbus-trace.py。
#
# 前置:
#   1. xime --dbus（或 xime-both）已运行，占用 org.fcitx.Fcitx5 服务名
#   2. Qt6Widgets 开发包（pkg-config --modversion Qt6Widgets 有值）
#   3. 系统 python 带 dbus+gi（Debian/Ubuntu 的 /usr/bin/python3 自带；
#      conda/miniforge 的 python 没有，别用它）
#   4. 处于图形会话（DISPLAY=:N 指向 XWayland；DBUS_SESSION_BUS_ADDRESS 已设）
set -u

SEQ="${1:-nihao_}"
WANT="${2:-你好}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TOOLS="$ROOT/xime-rs/tools"
PROBE_SRC="$ROOT/wayland-ime/test/qt_probe.c"
PROBE_BIN="$ROOT/wayland-ime/build/qt_probe"
PY="$(command -v /usr/bin/python3 || command -v python3)"

# 把序列里的 '_' 空格、'<' 标记等归一化成「按键驱动用」的 token 流，
# 与 dbus-trace.py 保持一致；这里 A 层直接交给 dbus-trace.py，B 层自己解析。
die() { echo "✗ $*" >&2; exit 1; }
ok()  { echo "✓ $*"; }

echo "== 环境自检 =="
[ -n "${DISPLAY:-}" ] || die "无 DISPLAY（需要 XWayland 的 X 显示）"
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || die "无 DBUS_SESSION_BUS_ADDRESS"
busctl --user status org.fcitx.Fcitx5 >/dev/null 2>&1 \
  || die "org.fcitx.Fcitx5 无主——先跑 xime --dbus 或 xime-both"
pkg-config --exists Qt6Widgets || die "缺 Qt6Widgets 开发包"
"$PY" -c 'import dbus, gi' 2>/dev/null || die "$PY 缺 dbus/gi 模块（用系统 /usr/bin/python3）"
ok "DBus 服务在、Qt6Widgets 在、python dbus/gi 在"

# ---------------------------------------------------------------------------
# A 层：协议（模拟 fcitx5-qt 客户端）
# ---------------------------------------------------------------------------
echo
echo "== A 层：协议握手 + 按键（dbus-trace.py） =="
AOUT="$(mktemp /tmp/xime-trace-XXXXXX.json)"
"$PY" "$TOOLS/dbus-trace.py" "$AOUT" "$SEQ" >/dev/null 2>&1 \
  || die "dbus-trace.py 跑失败"
A_GOT="$("$PY" -c 'import json,sys; d=json.load(open(sys.argv[1])); print("".join(d["commits"]))' "$AOUT")"
rm -f "$AOUT"
if [ "$A_GOT" = "$WANT" ]; then
  ok "提交 = 「$A_GOT」"
else
  die "提交不符：期望「$WANT」，实得「$A_GOT」"
fi

# ---------------------------------------------------------------------------
# B 层：真实 Qt6 应用跑在 XWayland（xcb）+ fcitx5 模块
# ---------------------------------------------------------------------------
echo
echo "== B 层：真实 Qt6 应用（xcb）+ fcitx5-qt 插件 =="
[ -x "$PROBE_BIN" ] || {
  echo "  编译 qt_probe …"
  g++ -O2 -fPIC "$(pkg-config --cflags Qt6Widgets)" "$PROBE_SRC" \
      "$(pkg-config --libs Qt6Widgets)" -o "$PROBE_BIN" \
    || die "qt_probe 编译失败"
}

before="$(busctl --user tree org.fcitx.Fcitx5 2>/dev/null | grep -oP 'inputcontext/\K\d+' | sort -n | tail -1)"
before="${before:-0}"

QOUT="$(mktemp /tmp/xime-qtprobe-XXXXXX.out)"
# QT_DEBUG_PLUGINS=1 让 qt_probe 把「是否加载了 fcitx5-qt 插件」打进 stderr，
# 一起落进 QOUT，作为 B 层的第一条证据。
env QT_QPA_PLATFORM=xcb QT_IM_MODULE=fcitx QT_DEBUG_PLUGINS=1 "$PROBE_BIN" >"$QOUT" 2>&1 &
QTPID=$!
trap 'kill $QTPID 2>/dev/null' EXIT
sleep 1.5

if grep -q 'libfcitx5platforminputcontextplugin.so" loaded library' "$QOUT"; then
  ok "fcitx5-qt 插件被加载（QT_IM_MODULE=fcitx 生效）"
else
  echo "  --- qt_probe 输出 ---" && cat "$QOUT"
  die "fcitx5-qt 插件未加载（QT_IM_MODULE 没生效或插件缺失）"
fi

after="$(busctl --user tree org.fcitx.Fcitx5 2>/dev/null | grep -oP 'inputcontext/\K\d+' | sort -n | tail -1)"
after="${after:-0}"
if [ "$after" -le "$before" ]; then
  echo "  --- qt_probe 输出 ---" && cat "$QOUT"
  die "qt_probe 未创建 input context（没连上 xime --dbus）"
fi
ICPATH="/org/freedesktop/portal/inputcontext/$after"
ok "qt_probe 连上 xime --dbus，IC = $ICPATH"

# 驱动该真实 IC 敲键（等价于在窗口里按键）
"$PY" - "$ICPATH" "$SEQ" <<'PY'
import dbus, sys, time
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
DBusGMainLoop(set_as_default=True)
path, seq = sys.argv[1], sys.argv[2]
bus = dbus.SessionBus()
ic = bus.get_object("org.fcitx.Fcitx5", path)
iface = dbus.Interface(ic, "org.fcitx.Fcitx.InputContext1")
def key(kv):
    iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(0), dbus.UInt32(0), dbus.Boolean(False), dbus.UInt32(0))
    time.sleep(0.03)
    iface.ProcessKeyEvent(dbus.UInt32(kv), dbus.UInt32(0), dbus.UInt32(0), dbus.Boolean(True), dbus.UInt32(0))
    time.sleep(0.03)
for ch in seq:
    if ch == '_':
        key(0x20)
    elif ch == '<':
        pass  # 特殊标记首版不支持，交给 A 层覆盖
    else:
        key(ord(ch))
time.sleep(0.6)
PY
sleep 0.4

B_COMMIT="$(grep -oP 'commit="\K[^"]*' "$QOUT" | tail -1)"
B_TEXT="$(grep -oP '控件里的文本现在是 "\K[^"]*' "$QOUT" | tail -1)"
echo "  qt_probe 收到的 commit = 「$B_COMMIT」，控件文本 = 「$B_TEXT」"
if [ "$B_COMMIT" = "$WANT" ] && [ "$B_TEXT" = "$WANT" ]; then
  ok "真实 Qt 控件渲染出「$B_TEXT」"
else
  echo "  --- qt_probe 完整输出 ---" && cat "$QOUT"
  die "真实应用层提交不符：commit=「$B_COMMIT」文本=「$B_TEXT」，期望「$WANT」"
fi

rm -f "$QOUT"
echo
echo "全部通过：X11/XWayland Qt（QT_IM_MODULE=fcitx）→ xime --dbus 链路正常。"
