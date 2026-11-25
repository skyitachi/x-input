#!/usr/bin/env python3
"""把旧 bigram.bin 的 val_id 重映射到新词库，词库变了但语料不可得时用。

为什么需要这一步：val_id 是「按词形词频降序排的名次」（见 mkbigram.py），词库一改
（新增词条、调整词频），同一个词形的 val_id 就可能整体平移，而 bigram.bin 里的
转移对是按 val_id 存的。不重映射的话，上下文消歧会静默错位——不报错、不崩溃，
只是词与词的搭配概率对到了别的词上，表现为「有些词突然选得莫名其妙」。

正规做法是重跑 mkbigram.py（需要 zhwiki 语料 2.2GB）。本脚本是它的离线替代：
按「词形」把旧表的 id 翻译成新表的 id，语料统计量沿用旧的。
新增词因为没有语料证据，本来也不会出现在转移表里，这不会损失原有质量。

用法:
  python3 tools/remap-bigram.py \\
      --old-dict /tmp/dict-old.bin --old-map /tmp/val_map-old.bin \\
      --new-dict xime-rs/data/dict.bin --new-map xime-rs/data/val_map.bin \\
      --in-bigram xime-rs/data/bigram.bin --out-bigram xime-rs/data/bigram.bin
"""

import argparse
import struct
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from importlib import import_module

_impl = import_module("mkbigram".replace("-", "_"))


def load_rows(path):
    return _impl.load_dict(path)


def load_val_map(path, n_words):
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEVM01":
        sys.exit(f"{path} 不是 val_map 文件")
    (n,) = struct.unpack_from("<I", raw, 8)
    if n != n_words:
        sys.exit(f"{path} 的条目数 {n} 与词库 {n_words} 不符")
    return list(struct.unpack_from(f"<{n}I", raw, 32))


def load_bigram(path):
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEBG01":
        sys.exit(f"{path} 不是 bigram 文件")
    n_prev, n_pairs = struct.unpack_from("<2I", raw, 8)
    pairs_off = 32 + (n_prev + 1) * 4
    pairs = []
    for i in range(n_pairs):
        nxt, cost = struct.unpack_from("<Ih", raw, pairs_off + i * 6)
        pairs.append((nxt, cost))
    offsets = struct.unpack_from(f"<{n_prev + 1}I", raw, 32)
    return offsets, pairs, n_prev


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--old-dict", required=True)
    ap.add_argument("--old-map", required=True)
    ap.add_argument("--new-dict", required=True)
    ap.add_argument("--new-map", required=True)
    ap.add_argument("--in-bigram", required=True)
    ap.add_argument("--out-bigram", required=True)
    a = ap.parse_args()

    old_rows = load_rows(a.old_dict)
    old_ids = load_val_map(a.old_map, len(old_rows))
    old_form = {}
    for wid, vid in enumerate(old_ids):
        old_form[vid] = old_rows[wid][1]

    new_rows = load_rows(a.new_dict)
    # 返回 (词形 -> val_id, 词条 -> val_id)，与 mkbigram.py 的分配规则完全一致
    new_val_to_id, new_word_to_val = _impl.build_val_map(new_rows)

    # 新 val_map 落盘（与 mkbigram.py 同格式）
    out = bytearray(b"XIMEVM01")
    out += struct.pack("<I", len(new_rows)) + b"\0" * 20
    out += struct.pack(f"<{len(new_rows)}I", *new_word_to_val)
    with open(a.new_map, "wb") as f:
        f.write(out)

    offsets, pairs, n_prev = load_bigram(a.in_bigram)
    stat_keep = stat_drop = 0
    new_pairs = []
    for prev in range(n_prev):
        lo, hi = offsets[prev], offsets[prev + 1]
        if lo == hi:
            continue
        pform = old_form.get(prev)
        nprev = new_val_to_id.get(pform) if pform else None
        if nprev is None:
            stat_drop += hi - lo
            continue
        for i in range(lo, hi):
            nxt = old_form.get(pairs[i][0])
            nid = new_val_to_id.get(nxt) if nxt else None
            if nid is None:
                stat_drop += 1
                continue
            new_pairs.append((nprev, nid, pairs[i][1]))
            stat_keep += 1

    new_pairs.sort()
    n_vals = len(new_val_to_id)
    bucket = [[] for _ in range(n_vals)]
    for p, n, c in new_pairs:
        bucket[p].append((n, c))

    offs = [0]
    for b in bucket:
        offs.append(offs[-1] + len(b))

    buf = bytearray(b"XIMEBG01")
    buf += struct.pack("<2I", n_vals, len(new_pairs)) + b"\0" * 16
    buf += struct.pack(f"<{len(offs)}I", *offs)
    for b in bucket:
        for n, c in b:
            buf += struct.pack("<Ih", n, c)
    with open(a.out_bigram, "wb") as f:
        f.write(buf)

    print(f"词形 {n_vals}（旧 {n_prev}）  转移对 {len(new_pairs)}（保留 {stat_keep}，丢弃 {stat_drop}）")
    print(f"写出 {a.new_map} / {a.out_bigram}")


if __name__ == "__main__":
    main()
