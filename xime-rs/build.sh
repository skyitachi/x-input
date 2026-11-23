#!/bin/sh
# 构建 xime（Rust 版）：数据 → 二进制 → 测试 → deb + Arch 包
set -eu
cd "$(dirname "$0")"

# 候选窗改用 Slint（master 主线）后引入 fontconfig/fontique：其 build script
# 要用宿主 pkg-config 查 freetype/fontconfig，musl 交叉编译会在 pkg-config
# 的 cross 检查处直接 panic（RUST_FONTCONFIG_DLOPEN 绕过在 fontique 直接
# 引用 Fc* 符号时同样编译失败）。因此主线构建走宿主 gnu target，动态链接。
# 产物路径与 packaging/build-{deb,arch}.sh 保持一致：target/release/xime。
TARGET="${XIME_TARGET:-}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少 $1" >&2; exit 1; }; }
need cargo
need python3

printf '== 1/4 数据（词库 + 简繁表 + 英文词表 + 字形）==\n'
if [ ! -s data/dict.bin ] || [ ! -s data/zh.bin ] || [ ! -s data/en.bin ] || [ ! -s data/atlas.bin ]; then
    sh scripts/gen-data.sh
else
    echo "  复用 data/dict.bin、data/zh.bin、data/en.bin 与 data/atlas.bin"
fi

if [ -n "$TARGET" ]; then
    printf '== 2/4 交叉编译（%s）==\n' "$TARGET"
    cargo build --release --target "$TARGET"
    BIN="target/$TARGET/release/xime"
else
    printf '== 2/4 宿主编译（gnu，Slint 需 fontconfig）==\n'
    cargo build --release
    BIN=target/release/xime
fi
[ -f "$BIN" ] || { echo "编译产物未找到: $BIN" >&2; exit 1; }

printf '== 3/4 单元测试 ==\n'
cargo test --release 2>&1 | grep -E "^test result|running"

printf '== 4/4 打包 ==\n'
export XIME_BIN="$BIN"
sh packaging/build-deb.sh
if command -v bsdtar >/dev/null 2>&1; then
    sh packaging/build-arch.sh
else
    echo "  （未检测到 bsdtar，跳过 Arch 包）"
fi

printf '\n产物：\n'
ls -lh dist/
