#!/usr/bin/env bash
# 开发/试用入口：把 input-method 席位临时交给 xime，退出后自动交还原输入法。
#
# 为什么需要这个脚本：zwp_input_method_manager_v2 每个 wl_seat 同时只接受一个
# 客户端，合成器对第二个申请者立刻回 `unavailable`。Hyprland 会话里默认占着
# 这个席位的是 fcitx5（omarchy-fcitx5.service），所以直接跑 xime 一定拿不到。
#
# 用法：
#   ./scripts/dev-run.sh                 # 用已构建的二进制
#   ./scripts/dev-run.sh --verbose       # 参数原样传给 xime
#   XIME_IM_SERVICE=fcitx5 ./scripts/dev-run.sh
set -uo pipefail

cd "$(dirname "$0")/.."

SERVICE=${XIME_IM_SERVICE:-omarchy-fcitx5}
BIN=${XIME_BIN:-./target/release/xime}

if [[ ! -x $BIN ]]; then
    echo "找不到 $BIN，先跑 cargo build --release" >&2
    exit 1
fi

stopped=0
if systemctl --user is-active --quiet "$SERVICE"; then
    systemctl --user stop "$SERVICE"
    stopped=1
    echo "已暂停 $SERVICE，input-method 席位交给 xime" >&2
elif pgrep -x fcitx5 >/dev/null; then
    # 服务没在跑但进程在（手工启动的情形）：同样让出来
    pkill -x fcitx5
    stopped=1
    echo "已结束手工启动的 fcitx5" >&2
fi

child=0
cleaned=0
cleanup() {
    # TERM 处理器跑完还会再触发一次 EXIT，做个幂等保护
    ((cleaned)) && return
    cleaned=1
    # 先收掉 xime，否则它一走神就把席位攥到 fcitx5 重启之后
    if ((child)); then
        kill "$child" 2>/dev/null
        wait "$child" 2>/dev/null
    fi
    if ((stopped)); then
        systemctl --user start "$SERVICE" >/dev/null 2>&1 \
            && echo "$SERVICE 已恢复" >&2 \
            || echo "恢复 $SERVICE 失败，手动跑 systemctl --user start $SERVICE" >&2
    fi
}
trap cleanup EXIT INT TERM HUP

# GTK/Qt 的 IM 模块变量指向 fcitx 的 DBus 前端；fcitx 停掉后应用要退回合成器
# 的 text-input 协议，否则拿不到输入上下文，xime 永远等不到 activate。
unset GTK_IM_MODULE QT_IM_MODULE XMODIFIERS INPUT_METHOD

status=0
"$BIN" "$@" &
child=$!
wait "$child" || status=$?
child=0
exit "$status"
