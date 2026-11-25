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

# 近期语料词频：维基 dump -> wiki.txt -> modern_freq.txt。
# mkdict.py 的三套词频源里，只有这一套是当期（其余两套停在 2014 年前后），
# 新词能不能排上去全看它。dump 不存在就跳过，mkdict 自动退回双源混合。
dump="$src/.cache/corpus/zhwiki-latest-pages-articles.xml.bz2"
wiki_txt="$src/.cache/corpus/wiki.txt"
modern="$src/.cache/dict/modern_freq.txt"
if [ -f "$dump" ]; then
    if [ ! -s "$wiki_txt" ] || [ "$dump" -nt "$wiki_txt" ]; then
        echo "  语料 -> .cache/corpus/wiki.txt（全量约 20 分钟）"
        # stride=6 是均匀降采样：dump 按页面 ID 排序，截断会丢掉最新条目
        python3 "$src/tools/wiki2txt.py" --jobs 8 --stride 6
    fi
    if [ ! -s "$modern" ] || [ "$wiki_txt" -nt "$modern" ]; then
        echo "  近期词频 -> .cache/dict/modern_freq.txt"
        python3 "$src/tools/mkmodernfreq.py" --dict "$here/data/dict.bin"
    fi
else
    echo "  跳过近期词频（没有维基 dump；先跑 $src/scripts/fetch-corpus.sh）"
fi

echo "  词库 -> data/dict.bin"
python3 "$src/tools/mkdict.py" --bin "$here/data/dict.bin"

# 简繁转换表：依赖 OpenCC，同时顺带产出繁体字符集（供字体烘焙）
echo "  简繁 -> data/zh.bin"
python3 "$here/scripts/gen-zh.py"

# 英文词表：把 data/en-words.txt（按词频降序）转成字母序索引
echo "  英文 -> data/en.bin"
python3 "$here/scripts/gen-en.py"

# 二元语言模型转移表：需要中文句子语料（scripts/fetch-corpus.sh 下载、
# scripts/clean-corpus.py 清洗成 corpus.txt）。语料未就绪时跳过——Rust 侧
# bigram.rs 会退化回 unigram（trans_cost 恒 0），不影响编译与运行。
corpus="$src/.cache/corpus/corpus.txt"
if [ -f "$corpus" ]; then
    echo "  bigram -> data/val_map.bin + data/bigram.bin"
    python3 "$src/tools/mkbigram.py" --dict "$here/data/dict.bin" --corpus "$corpus" \
        --out-map "$here/data/val_map.bin" --out-bigram "$here/data/bigram.bin"
else
    echo "  跳过 bigram（没有分词语料；先跑 fetch-corpus.sh + clean-corpus.py，"
    echo "    或直接用 .cache/corpus/wiki.txt 配 mkbigram.py，见 README「六点六」）"
    echo "    注意：词库刚变过的话 val_map/bigram 的 val_id 已失效，用"
    echo "    python3 $src/tools/remap-bigram.py --old-dict <旧> --old-map <旧> \\"
    echo "      --new-dict $here/data/dict.bin --new-map $here/data/val_map.bin \\"
    echo "      --in-bigram <旧> --out-bigram $here/data/bigram.bin"
fi

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
