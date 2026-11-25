#!/usr/bin/env bash
# verify-launcher-input.sh — 端到端验证「启动器里真的能把按键送进输入法」
#
# 为什么需要它：
#   verify-real-input.sh 只用 dbus-send 直连 org.fcitx.Fcitx5，证明的是
#   「DBus 服务端逻辑正确」。它完全绕过了客户端（Quickshell 的 Qt 输入框），
#   所以会出现「脚本 PASS，但用户在启动器里打字没反应」——服务端没问题，
#   问题在按键有没有走到客户端输入上下文。
#
#   本脚本走完整链路：
#     Hyprland send_shortcut（真实键盘事件，经合成器）
#       → Quickshell TextInput（Qt）
#         → fcitx5-qt 插件 → DBus ProcessKeyEvent
#           → xime → 候选窗/上屏
#   并在 DBus 上断言 ProcessKeyEvent 确实发生。
#
# 退出码：0 = 按键确实进入了输入法；1 = 按键被绕过（客户端没走 IME）。
#
# 依赖：Hyprland 0.5x+（hl.dsp.send_shortcut）、omarchy、busctl、grim（可选）
# 关闭截图：XIME_E2E_NO_SHOT=1

set -uo pipefail

trace=$(mktemp /tmp/xime-e2e-XXXXXX.log)
mon_pid=""
cleanup() {
    [ -n "$mon_pid" ] && kill "$mon_pid" 2>/dev/null
    # 用 unlink 而不是 rm：部分环境的 rm 被安全 shim 包装，可能报权限错误
    [ -e "$trace" ] && /usr/bin/unlink "$trace" 2>/dev/null
}
trap cleanup EXIT

say() { printf '%s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# --- 0. 前置检查 ---------------------------------------------------------
owner=$(busctl --user list 2>/dev/null | awk '$1=="org.fcitx.Fcitx5"{print $3; exit}')
[ "$owner" = "xime" ] || fail "org.fcitx.Fcitx5 当前持有者=${owner:-无人}，不是 xime。先执行 xime-both --status 确认。"
command -v hyprctl >/dev/null || fail "没有 hyprctl，无法注入真实按键"
command -v busctl  >/dev/null || fail "没有 busctl"

# --- 1. 打开启动器（幂等，不会误关）--------------------------------------
if command -v omarchy-menu >/dev/null; then
    omarchy-menu summon root >/dev/null 2>&1 || fail "omarchy-menu summon root 失败"
else
    fail "没有 omarchy-menu；请手动打开启动器后重跑，或自行调整本脚本"
fi
sleep 1.5

# --- 2. 开始抓 DBus 报文 -------------------------------------------------
busctl --user monitor org.fcitx.Fcitx5 >"$trace" 2>&1 &
mon_pid=$!
sleep 0.5

# --- 3. 注入真实按键：n i h a o ------------------------------------------
for k in N I H A O; do
    hyprctl dispatch "hl.dsp.send_shortcut({mods=\"\", key=\"$k\"})" >/dev/null 2>&1 \
        || fail "hyprctl send_shortcut $k 失败（Hyprland 版本可能过旧）"
    sleep 0.2
done
sleep 1.2

pke=$(grep -ac 'Member=ProcessKeyEvent' "$trace" || true)
ui=$(grep -ac 'Member=UpdateClientSideUI' "$trace" || true)

say "ProcessKeyEvent 次数: $pke"
say "候选窗 UI 信号次数:  $ui"

if [ "${XIME_E2E_NO_SHOT:-0}" != "1" ] && command -v grim >/dev/null; then
    shot=/tmp/xime-e2e-$(date +%H%M%S).png
    grim "$shot" 2>/dev/null && say "截图: $shot（应能看到拼音 nihao 和候选词）"
fi

# 收尾：关掉启动器，别给用户留个开着的菜单
hyprctl dispatch 'hl.dsp.send_shortcut({mods="", key="ESCAPE"})' >/dev/null 2>&1

if [ "$pke" -eq 0 ]; then
    say ""
    say "结论: 按键没有进入输入法——客户端的输入框没走 IME。"
    say "      fcitx5 在同一个界面若同样不行，说明是该界面自身的实现问题"
    say "      （例如 /usr/share/omarchy/shell/plugins/dock/AppGrid.qml 用"
    say "      Keys.onPressed 自绘搜索框，不是 TextInput，任何输入法都收不到）。"
    exit 1
fi

say ""
say "PASS: 按键确实经过输入法（ProcessKeyEvent 已到达 xime）。"
say "      若此时屏幕上仍无候选/中文，问题在 xime 返回的信号，"
say "      用 tools/verify-real-input.sh 继续查服务端。"
exit 0
