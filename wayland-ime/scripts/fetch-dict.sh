#!/bin/sh
# 下载构建词库所需的公开数据（均为 MIT 许可），缓存到 .cache/dict/。
set -eu
cd "$(dirname "$0")/.."
mkdir -p .cache/dict

get() {
    dest=".cache/dict/$1"
    url="$2"
    [ -s "$dest" ] && { echo "  已有 $1"; return 0; }
    echo "  下载 $1"
    curl -sfL --max-time 120 -o "$dest" "$url"
}

get pinyin.txt \
    "https://raw.githubusercontent.com/mozillazg/pinyin-data/master/pinyin.txt"
get large_pinyin.txt \
    "https://raw.githubusercontent.com/mozillazg/phrase-pinyin-data/master/large_pinyin.txt"
get jieba_dict.txt \
    "https://raw.githubusercontent.com/fxsjy/jieba/master/jieba/dict.txt"
get LICENSE.pinyin-data \
    "https://raw.githubusercontent.com/mozillazg/pinyin-data/master/LICENSE"
get LICENSE.phrase-pinyin-data \
    "https://raw.githubusercontent.com/mozillazg/phrase-pinyin-data/master/LICENSE"
get LICENSE.jieba \
    "https://raw.githubusercontent.com/fxsjy/jieba/master/LICENSE"
