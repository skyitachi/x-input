#!/bin/sh
# 构建 xime（Rust 版）：数据 → 静态二进制 → 测试 → deb + Arch 包
set -eu
cd "$(dirname "$0")"

TARGET=x86_64-unknown-linux-musl

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少 $1" >&2; exit 1; }; }
need cargo
need python3

printf '== 1/4 数据（词库 + 简繁表 + 字形）==\n'
if [ ! -s data/dict.bin ] || [ ! -s data/zh.bin ] || [ ! -s data/atlas.bin ]; then
    sh scripts/gen-data.sh
else
    echo "  复用 data/dict.bin、data/zh.bin 与 data/atlas.bin"
fi

printf '== 2/4 静态编译（%s）==\n' "$TARGET"
cargo build --release --target "$TARGET"

printf '== 3/4 单元测试 ==\n'
cargo test --release 2>&1 | grep -E "^test result|running"

printf '== 4/4 打包 ==\n'
sh packaging/build-deb.sh
if command -v bsdtar >/dev/null 2>&1; then
    sh packaging/build-arch.sh
else
    echo "  （未检测到 bsdtar，跳过 Arch 包）"
fi

printf '\n产物：\n'
ls -lh dist/
