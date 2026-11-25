#!/bin/sh
# verify-real-input.sh — 端到端验证 xime 真的能输入（替代不可靠的截图）
#
# 为什么需要它：
#   `xime --ui-probe` 只是离屏渲染——把候选数据直接喂给 Slint 出图，不经过
#   键盘、合成器抓键、前端连接、候选显示任何一环。图好看不等于能打字。
#   本脚本走真实 DBus 链路：新建输入上下文 → 逐键 ProcessKeyEvent →
#   断言收到 CommitString。这才是「能输入」的证据。
#
# 用法: verify-real-input.sh [拼音] [期望上屏]
#   默认: verify-real-input.sh nihao 你好
# 退出码: 0 = 通过, 1 = 失败
set -u
export XIME_VERIFY_INPUT="${1:-nihao}"
export XIME_VERIFY_EXPECT="${2:-你好}"
fail=0

echo "-- 1. 服务归属 --"
owner=$(busctl --user list 2>/dev/null | awk '$1 == "org.fcitx.Fcitx5" { print $3; exit }')
if [ "$owner" != "xime" ]; then
    echo "   [失败] org.fcitx.Fcitx5 持有者 = ${owner:-无人}，不是 xime"
    echo "          先执行: xime-switch xime"
    exit 1
fi
xp=$(busctl --user list 2>/dev/null | awk '$1 == "org.fcitx.Fcitx5" { print $2; exit }')
echo "   [通过] 由 xime 持有（PID ${xp:-?}）"

echo
echo "-- 2. 当前输入法状态 --"
im=$(gdbus call --session --dest org.fcitx.Fcitx5 --object-path /controller \
     --method org.fcitx.Fcitx.Controller1.CurrentInputMethod 2>/dev/null |
     sed "s/^('//; s/',)$//")
case "$im" in
    *pinyin*) echo "   [通过] $im（中文）" ;;
    *)        echo "   [失败] 当前为 ${im:-未知}，不是中文"
              echo "          先按 Shift 切到中文再测"
              exit 1 ;;
esac

echo
echo "-- 3. 端到端组字（真实 DBus 输入上下文）--"
python3 - <<'PY' || fail=1
import os, sys
try:
    import dbus
    import dbus.mainloop.glib as ml
    from gi.repository import GLib
except Exception as e:
    print(f"   [失败] 缺少 python-dbus / python-gi: {e}")
    sys.exit(1)

want = os.environ.get("XIME_VERIFY_INPUT", "nihao")
expect = os.environ.get("XIME_VERIFY_EXPECT", "你好")

ml.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
commits = []
bus.add_signal_receiver(
    lambda *a, **k: commits.append(str(a[0]) if a else ""),
    signal_name="CommitString",
    dbus_interface="org.fcitx.Fcitx.InputContext1")

# evdev keycode（US qwerty 三行）
KEY = {}
for i, ch in enumerate("qwertyuiop"):
    KEY[ch] = 16 + i
for i, ch in enumerate("asdfghjkl"):
    KEY[ch] = 30 + i
for i, ch in enumerate("zxcvbnm"):
    KEY[ch] = 44 + i

bad = [c for c in want if c not in KEY and c != " "]
if bad:
    print(f"   [失败] 仅支持小写字母与空格，遇到: {''.join(bad)}")
    sys.exit(1)

im = dbus.Interface(
    bus.get_object("org.fcitx.Fcitx5", "/org/freedesktop/portal/inputmethod"),
    "org.fcitx.Fcitx.InputMethod1")
path, _uuid = im.CreateInputContext([("program", "xime-verify")])
ic = dbus.Interface(bus.get_object("org.fcitx.Fcitx5", str(path)),
                    "org.fcitx.Fcitx.InputContext1")
ic.SetCapability(dbus.UInt64(0xE000800072))
ic.FocusIn()

def press(keysym, keycode):
    return bool(ic.ProcessKeyEvent(dbus.UInt32(keysym), dbus.UInt32(keycode),
                                   dbus.UInt32(0), False, dbus.UInt32(0)))

missed = []
for ch in want:
    ks, kc = (0x20, 57) if ch == " " else (ord(ch), KEY[ch])
    if not press(ks, kc):
        missed.append(ch)
press(0x20, 57)  # 空格上屏

loop = GLib.MainLoop()
GLib.timeout_add(800, loop.quit)
loop.run()
try:
    ic.DestroyIC()
except Exception:
    pass

got = "".join(commits)
print(f"   按键 {want!r}：{'全部被消费' if not missed else '未被消费: ' + ''.join(missed)}")
print(f"   期望上屏 {expect!r}，实际上屏 {got!r}")
if got == expect:
    print("   [通过] 端到端组字并提交正确")
    sys.exit(0)
print("   [失败] 服务端没有按预期提交")
sys.exit(1)
PY

echo
echo "-- 4. shell 是否已重连 --"
qs_pid=$(pgrep -x quickshell 2>/dev/null | head -1)
starttime_of() {
    awk '{ p = 0; for (i = 1; i <= NF; i++) if ($i ~ /\)$/) { p = i; break }
           if (p) print $(p + 20) }' "/proc/$1/stat" 2>/dev/null
}
if [ -z "$qs_pid" ]; then
    echo "   [跳过] quickshell 未运行"
else
    xime_pid=
    for p in $(pgrep -x xime 2>/dev/null || true); do
        tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- '--dbus' && { xime_pid=$p; break; }
    done
    qs_t=$(starttime_of "$qs_pid")
    xm_t=$(starttime_of "${xime_pid:-0}")
    if [ -n "$qs_t" ] && [ -n "$xm_t" ] && [ "$qs_t" -lt "$xm_t" ]; then
        echo "   [失败] shell(PID $qs_pid) 比输入法服务更老 → 可能持有旧连接"
        echo "          执行 omarchy-restart-shell（xime-switch xime 也会自动重连）"
        fail=1
    else
        echo "   [通过] shell 启动晚于输入法服务，连接是新的"
    fi
fi

echo
if [ "$fail" -eq 0 ]; then
    echo "结论: 通过 —— xime 端到端可用"
else
    echo "结论: 失败 —— 见上面 [失败] 项"
fi
exit "$fail"
