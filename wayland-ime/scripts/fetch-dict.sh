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
# 口语词频（OpenSubtitles 字幕语料）。mkdict.py 里 W_SUB=0.55 依赖它，
# 此前漏下载导致口语权重恒为 0、词库只剩 jieba 书面语词频。
get zh_freq_50k.txt \
    "https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content/2016/zh/zh_50k.txt"
get LICENSE.pinyin-data \
    "https://raw.githubusercontent.com/mozillazg/pinyin-data/master/LICENSE"
get LICENSE.phrase-pinyin-data \
    "https://raw.githubusercontent.com/mozillazg/phrase-pinyin-data/master/LICENSE"
get LICENSE.jieba \
    "https://raw.githubusercontent.com/fxsjy/jieba/master/LICENSE"
