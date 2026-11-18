#!/bin/sh
# 一键构建：生成协议绑定 -> 烘焙字形 -> 编译 -> 跑测试 -> 打 deb
set -eu
cd "$(dirname "$0")"

die() { printf 'build: %s\n' "$*" >&2; exit 1; }

for t in cc make pkg-config wayland-scanner; do
    command -v "$t" >/dev/null 2>&1 || die "缺少构建工具: $t"
done
for p in wayland-client xkbcommon; do
    pkg-config --exists "$p" || die "缺少开发包: $p（Debian: libwayland-dev libxkbcommon-dev）"
done

[ -f src/dict_gen.h ] || die "缺少 src/dict_gen.h，请先执行 make dict 生成词库（需要网络）"

printf '== 1/4 生成协议绑定 ==\n'
sh proto/gen.sh build/gen

printf '== 2/4 烘焙字形图集 ==\n'
make font

printf '== 3/4 编译 ==\n'
make all

printf '== 4/4 测试 ==\n'
make build/test_engine >/dev/null
build/test_engine
if pkg-config --exists wayland-server 2>/dev/null; then
    make build/mock_compositor >/dev/null
    build/mock_compositor
else
    printf '（跳过协议集成测试：未安装 wayland-server 开发包）\n'
fi

printf '== 打包 ==\n'
sh packaging/build-deb.sh
