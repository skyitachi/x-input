#!/usr/bin/env python3
"""把英文词表转成运行时查表用的 data/en.bin。

词表（data/en-words.txt）按词频降序、一行一词，取自 google-10000-english
（Google Trillion Word Corpus 频次统计）。这里把它重排成**字母序**并保留
词频排名，因为英文补全的核心操作是「前缀二分 + 按频次排序」：

    hel  ->  hello, help, held, hell, helicopter ...

字母序负责定位，rank 负责排序，两者都要，所以不能沿用原文件的顺序。
"""
import os
import struct
import sys

MAGIC = b"XIMEEN01"
HDR = 16  # magic + n_words + blob_len
ENTRY = 6  # u32 词串偏移 + u16 词频排名（1 万条用 u16 足够）


def load(path):
    """读词表，返回 [(词, 排名), ...]，排名 0 表示最高频。"""
    rows = []
    seen = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            w = line.strip()
            # 词表里有 "ns" 这类缩写和个别带标点的条目；只保留纯小写字母，
            # 大写靠运行时的大小写策略生成，不需要词表里再存一遍。
            if not w or not w.isascii() or not w.isalpha() or not w.islower():
                continue
            if w in seen:
                continue
            seen.add(w)
            rows.append(w)
    return [(w, i) for i, w in enumerate(rows)]


def build(rows):
    """按字母序重排，返回 (词表字节, 索引字节, 词数)。"""
    ordered = sorted(rows)  # 按词串排序，rank 跟着走
    blob = bytearray()
    idx = bytearray()
    for w, rank in ordered:
        off = len(blob)
        blob += w.encode("ascii") + b"\0"
        idx += struct.pack("<IH", off, rank)
    return bytes(blob), bytes(idx), len(ordered)


def main():
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    src = os.path.join(here, "data", "en-words.txt")
    out = os.path.join(here, "data", "en.bin")

    if not os.path.exists(src):
        sys.exit(f"缺少词表 {src}（应为一行一词、按词频降序）")

    rows = load(src)
    if len(rows) < 1000:
        sys.exit(f"词表太小（{len(rows)} 词），检查 {src} 是否完整")

    blob, idx, n = build(rows)

    with open(out, "wb") as f:
        f.write(MAGIC)
        f.write(struct.pack("<II", n, len(blob)))
        f.write(idx)
        f.write(blob)

    size = os.path.getsize(out)
    print(f"生成 {out}")
    print(f"  词数 {n}  索引 {len(idx) / 1024:.0f}KB  词串 {len(blob) / 1024:.0f}KB  共 {size / 1024:.0f}KB")


if __name__ == "__main__":
    main()
