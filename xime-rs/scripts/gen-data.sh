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

# 英文词表：把 data/en-words.txt（按词频降序）转成字母序索引
echo "  英文 -> data/en.bin"
python3 "$here/scripts/gen-en.py"

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

# 优先用 mkfont-ft：构建期调系统 FreeType 做 hinting（等效 hintslight），
# 和 fcitx5 走同一条光栅化路径。实测 18px 汉字有墨像素里 40.7% 完全实心，
# 而 stb 不做 hinting 只有 32%，肉眼就是「比 fcitx 糊」。
# 运行时不依赖 freetype——位图已经烘焙进二进制。
if pkg-config --exists freetype2 2>/dev/null; then
    echo "  字形用 FreeType hinting 烘焙"
    cc -O2 -I"$src/src" $(pkg-config --cflags freetype2) -o "$src/build/mkfont-ft" \
       "$src/tools/mkfont-ft.c" "$src/src/dict.c" "$src/src/utf8.c" \
       $(pkg-config --libs freetype2) -lm
    "$src/build/mkfont-ft" "$font" "$here/data/atlas.bin" 18 "$here/data/hant-chars.txt" light
else
    echo "  没有 freetype2 开发包，回退到 stb 烘焙（无 hinting，字会略糊）"
    cc -O2 -I"$src/third_party" -I"$src/src" -o "$src/build/mkfont" \
       "$src/tools/mkfont.c" "$src/src/dict.c" "$src/src/utf8.c" -lm
    # 0.85 = 边缘压暗，26 = 低于该 alpha 的灰雾归零，用来补足没有 hinting 的短板
    "$src/build/mkfont" "$font" "$here/data/atlas.bin" 18 "$here/data/hant-chars.txt" 0.85 26
fi
