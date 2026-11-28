#!/bin/sh
# 构建 xime（Rust 版）：版本号 → 数据 → 二进制 → 测试 → deb + Arch 包
#
# 每次构建先把 Cargo.toml 的 patch 位 +1（0.3.0 -> 0.3.1），让每份产物都有
# 唯一版本号：二进制里的 CARGO_PKG_VERSION、deb 文件名、Arch 包 pkgver 共用它。
# 要重打同一版本（不涨号）用：./build.sh --no-bump
set -eu
cd "$(dirname "$0")"

for arg in "$@"; do
    case "$arg" in
        --no-bump) XIME_NO_BUMP=1 ;;
        -h|--help)
            sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "未知参数: $arg（可用: --no-bump）" >&2
            exit 2
            ;;
    esac
done
export XIME_NO_BUMP="${XIME_NO_BUMP:-0}"

# 候选窗改用 Slint（master 主线）后引入 fontconfig/fontique：其 build script
# 要用宿主 pkg-config 查 freetype/fontconfig，musl 交叉编译会在 pkg-config
# 的 cross 检查处直接 panic（RUST_FONTCONFIG_DLOPEN 绕过在 fontique 直接
# 引用 Fc* 符号时同样编译失败）。因此主线构建走宿主 gnu target，动态链接。
# 产物路径与 packaging/build-{deb,arch}.sh 保持一致：target/release/xime。
TARGET="${XIME_TARGET:-}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少 $1" >&2; exit 1; }; }
need cargo
need python3

printf '== 1/5 版本号 ==\n'
if [ "$XIME_NO_BUMP" = 1 ]; then
    VER=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
    echo "  保持 $VER（--no-bump）"
else
    VER=$(sh scripts/bump-version.sh)
    echo "  $VER（patch 自增，已写回 Cargo.toml）"
fi

printf '== 2/5 数据（词库 + 简繁表 + 英文词表 + 字形 + emoji）==\n'
if [ ! -s data/dict.bin ] || [ ! -s data/zh.bin ] || [ ! -s data/en.bin ] || \
   [ ! -s data/atlas.bin ] || [ ! -s data/emoji.bin ]; then
    sh scripts/gen-data.sh
else
    echo "  复用 data/*.bin（词库、简繁、英文、字形、emoji）"
fi

if [ -n "$TARGET" ]; then
    printf '== 3/5 交叉编译（%s）==\n' "$TARGET"
    cargo build --release --target "$TARGET"
    BIN="target/$TARGET/release/xime"
else
    printf '== 3/5 宿主编译（gnu，Slint 需 fontconfig）==\n'
    cargo build --release
    BIN=target/release/xime
fi
[ -f "$BIN" ] || { echo "编译产物未找到: $BIN" >&2; exit 1; }

printf '== 4/5 单元测试 ==\n'
cargo test --release 2>&1 | grep -E "^test result|running"

printf '== 5/5 打包 ==\n'
export XIME_BIN="$BIN"
sh packaging/build-deb.sh
if command -v bsdtar >/dev/null 2>&1; then
    sh packaging/build-arch.sh
else
    echo "  （未检测到 bsdtar，跳过 Arch 包）"
fi

printf '\n产物：\n'
ls -lh dist/
