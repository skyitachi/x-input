#!/bin/sh
# 把 Cargo.toml 里 [package] 的 patch 版本号 +1，并把新版本号打到 stdout。
#
#   sh scripts/bump-version.sh             # 0.3.0 -> 0.3.1，写回 Cargo.toml
#   sh scripts/bump-version.sh --dry-run   # 只打印下一个版本号，不动文件
#
# 只改 [package] 段里那一行 version，注释、依赖、其它表原样保留。
# build.sh 在编译前调用它，于是一个版本号同时决定三处：
#   二进制里的 CARGO_PKG_VERSION（src/main.rs 用 env! 取）
#   deb 文件名        dist/xime_<ver>_amd64.deb
#   Arch 包 pkgver    dist/xime-<ver>-1-x86_64.pkg.tar.zst
set -eu
cd "$(dirname "$0")/.."

dry=0
case "${1:-}" in
    -n|--dry-run) dry=1 ;;
    "") : ;;
    -h|--help)
        sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "用法: $0 [--dry-run]" >&2
        exit 2
        ;;
esac

cur=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
[ -n "$cur" ] || { echo "Cargo.toml 里没找到 version = \"...\"" >&2; exit 1; }

case "$cur" in
    *.*.*) : ;;
    *) echo "版本号不是 MAJOR.MINOR.PATCH 形式：$cur" >&2; exit 1 ;;
esac

major=${cur%%.*}
rest=${cur#*.}
minor=${rest%%.*}
patch=${rest#*.}

case "$major$minor$patch" in
    *[!0-9]*) echo "版本号里有非数字段：$cur" >&2; exit 1 ;;
esac

# 去掉前导零：$((08 + 1)) 在某些 shell 里会按八进制解析
patch=$(printf '%s' "$patch" | sed 's/^0*//')
[ -n "$patch" ] || patch=0
new="$major.$minor.$((patch + 1))"

if [ "$dry" = 1 ]; then
    printf '%s\n' "$new"
    exit 0
fi

# 用 awk 状态机只在 [package] 段内替换，避免误改依赖里的 version =
mkdir -p build
tmp="build/.Cargo.toml.$$"
awk -v v="$new" '
    /^\[/ { in_pkg = ($0 == "[package]") }
    !hit && in_pkg && /^version[ \t]*=/ {
        sub(/"[^"]*"/, "\"" v "\"")
        hit = 1
    }
    { print }
' Cargo.toml > "$tmp"

grep -q "^version = \"$new\"$" "$tmp" || {
    echo "改写 Cargo.toml 失败（未找到 version 行）" >&2
    exit 1
}

cat "$tmp" > Cargo.toml
: > "$tmp"

printf '%s\n' "$new"
