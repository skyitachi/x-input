#!/bin/sh
# 下载中文句子语料（维基百科中文 dump）到 .cache/corpus/，供 mkbigram.py 统计 bigram。
# 走 curl 默认的 http_proxy/https_proxy 环境变量（本机如需代理，先 export 再执行）。
set -eu
cd "$(dirname "$0")/.."
mkdir -p .cache/corpus

dest=".cache/corpus/zhwiki-pages-articles.xml.bz2"
[ -s "$dest" ] && { echo "  已有 $dest"; exit 0; }

echo "  下载 $dest（约 2.2GB，视带宽需数分钟）"
curl -fL --max-time 3600 -o "$dest" \
    "https://dumps.wikimedia.org/zhwiki/latest/zhwiki-latest-pages-articles.xml.bz2"
