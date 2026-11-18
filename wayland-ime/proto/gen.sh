#!/bin/sh
# 生成 Wayland 协议绑定（client + server 头，private-code 实现）。
# 协议 XML 全部随仓库 vendored，构建时不需要安装 wayland-protocols / wlr-protocols。
set -eu
out=${1:-build/gen}
root=$(cd "$(dirname "$0")/.." && pwd)
scanner=${WAYLAND_SCANNER:-wayland-scanner}

mkdir -p "$out"

gen() {
    xml=$1
    base=$2
    [ -f "$xml" ] || { echo "missing protocol xml: $xml" >&2; exit 1; }
    "$scanner" client-header "$xml" "$out/$base.h"
    "$scanner" server-header "$xml" "$out/$base-server.h"
    "$scanner" private-code  "$xml" "$out/$base.c"
    echo "  gen  $base.h / $base-server.h / $base.c"
}

cd "$root"
gen proto/vendor/input-method-unstable-v2.xml       input-method-v2
gen proto/vendor/wlr-virtual-keyboard-unstable-v1.xml virtual-keyboard
