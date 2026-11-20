#!/bin/sh
# kwin-focus.sh <标题片段>
#
# 把标题包含该片段的窗口置为活动窗口。KDE 的 Wayland 会话里没有 xdotool/wmctrl，
# 想「确定地」让某个窗口拿到焦点（例如自动化输入测试）只能走 KWin 脚本。
#
# 两个必须注意的点：
#   1. loadScript 只负责加载，**必须再调 Scripting.start()** 才会执行。
#      漏掉这一步的现象是：拿到脚本 id，journal 里什么都没有，窗口焦点纹丝不动。
#   2. 脚本接口不能带参数，所以这里每次把匹配串生成进一个临时 js。
#
# 执行结果看 journal（KWin 里没有 stdout）：
#   journalctl --user -q --since "10 seconds ago" | grep xime-test
set -eu

if [ $# -lt 1 ]; then
    echo "用法: kwin-focus.sh <窗口标题片段>" >&2
    exit 2
fi

JS=$(mktemp /tmp/kwin-focus-XXXXXX.js)
trap 'rm -f "$JS"' EXIT

cat >"$JS" <<EOF
function pick() {
    var list = (typeof workspace.windowList === "function")
        ? workspace.windowList() : workspace.clientList();
    for (var i = 0; i < list.length; i++) {
        var w = list[i], cap = w.caption || "";
        if (cap.indexOf("$1") >= 0) {
            workspace.activeWindow = w;
            if (typeof workspace.raiseWindow === "function") workspace.raiseWindow(w);
            console.error("kwin-focus: focused [" + cap + "]");
            return;
        }
    }
    console.error("kwin-focus: no window matched $1");
}
pick();
EOF

qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript kwin-focus >/dev/null 2>&1 || true
qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript "$JS" kwin-focus >/dev/null
qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.start >/dev/null
