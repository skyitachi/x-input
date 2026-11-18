#!/bin/sh
# 生成嵌入二进制的数据：词库（dict.bin）、简繁转换表（zh.bin）与字形图集（atlas.bin）。
#
# 生成器复用同级的 C 版工具（../wayland-ime/tools），避免同一套词频/字形逻辑
# 维护两份。如果只想构建 Rust 版而不改数据，直接使用已生成好的 data/*.bin 即可。
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
src=${XIME_SRC:-$here/../wayland-ime}

[ -d "$src/tools" ] || {
    echo "找不到生成器目录 $src（可用 XIME_SRC 指定 C 版仓库位置）" >&2
    exit 1
}

mkdir -p "$here/data"

echo "  词库 -> data/dict.bin"
python3 "$src/tools/mkdict.py" --bin "$here/data/dict.bin"

# 简繁转换表：依赖 OpenCC，同时顺带产出繁体字符集（供字体烘焙）
echo "  简繁 -> data/zh.bin"
python3 "$here/scripts/gen-zh.py"

font=$(sh "$src/scripts/find-font.sh") || {
    echo "系统里找不到含汉字的字体，无法烘焙字形" >&2
    exit 1
}
echo "  字形 -> data/atlas.bin（源字体 $font）"
# mkfont 需要 C 版生成好的词库头文件来收集字符集
if [ ! -f "$src/src/dict_gen.h" ]; then
    python3 "$src/tools/mkdict.py"
fi
mkdir -p "$src/build"
cc -O2 -I"$src/third_party" -I"$src/src" -o "$src/build/mkfont" \
   "$src/tools/mkfont.c" "$src/src/dict.c" "$src/src/utf8.c" -lm
"$src/build/mkfont" "$font" "$here/data/atlas.bin" 16 "$here/data/hant-chars.txt"
