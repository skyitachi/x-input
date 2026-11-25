#!/bin/sh
# 下载构建词库所需的语料，缓存到 .cache/corpus/ 与 .cache/dict/thuocl/。
# 走 curl 默认的 http_proxy/https_proxy 环境变量（本机如需代理，先 export 再执行）。
#
# 两类数据，用途不同：
#   1) 中文维基百科 dump（每月更新）—— tools/mkmodernfreq.py 用它统计当期词频。
#      README 里说的「2014 年语料」指 jieba 词典与字幕词频，两者都已过时，
#      新词在这两个源里计数为 0，只能靠 tools/extra-words.txt 人工补。
#      维基是唯一定期更新、可再分发（CC BY-SA）的大规模中文语料。
#   2) THUOCL 领域词表（清华 NLP，MIT）—— tools/mkdict.py 用它做领域收录证据。
#      IT/财经/医学/法律/汽车/食品/成语/动物共 8.5 万词条，解决「确定是一个词、
#      但语料里没有计数」的问题。
set -eu
cd "$(dirname "$0")/.."
mkdir -p .cache/corpus .cache/dict/thuocl

# ---------------- 1) 中文维基百科 ----------------
dest=".cache/corpus/zhwiki-latest-pages-articles.xml.bz2"
if [ -s "$dest" ]; then
    echo "  已有 $dest"
else
    echo "  下载 $dest（约 3.3GB，视带宽需数分钟到一小时）"
    curl -fL --max-time 3600 -o "$dest" \
        "https://dumps.wikimedia.org/zhwiki/latest/zhwiki-latest-pages-articles.xml.bz2"
fi

# ---------------- 2) THUOCL 领域词表 ----------------
get_thuocl() {
    name=$1
    d=".cache/dict/thuocl/thuoc-$name.txt"
    [ -s "$d" ] && { echo "  已有 thuoc-$name.txt"; return 0; }
    echo "  下载 thuoc-$name.txt"
    curl -sfL --max-time 120 -o "$d" \
        "https://raw.githubusercontent.com/thunlp/THUOCL/master/data/THUOCL_$name.txt"
}

# 文件名大小写以仓库实际情况为准：IT 是大写，其余小写
for n in IT caijing medical law car food chengyu animal; do
    get_thuocl "$n"
done

[ -s .cache/dict/LICENSE.thuocl ] || curl -sfL --max-time 60 \
    -o .cache/dict/LICENSE.thuocl \
    "https://raw.githubusercontent.com/thunlp/THUOCL/master/LICENSE"

echo
echo "语料已就绪。下一步："
echo "  python3 tools/wiki2txt.py --jobs 8 --stride 6   # dump -> wiki.txt（约 20 分钟）"
echo "  python3 tools/mkmodernfreq.py      # 统计当期词频 -> .cache/dict/modern_freq.txt"
echo "  sh ../xime-rs/scripts/gen-data.sh  # 重建词库"
