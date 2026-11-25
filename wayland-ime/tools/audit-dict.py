#!/usr/bin/env python3
"""查一个词在词库里到底有没有、排第几；缺失或排太后时给出可直接粘贴的补词行。

为什么需要它：输入法里「打不出某个词」有四种完全不同的成因，肉眼分不清——
  1. 词条不存在（读音表和语料都没有，或词频低于门槛）；
  2. 词条存在，但同音键被更常见的词占满（MAX_SAME_KEY）；
  3. 词条存在且在同一键内排位靠后（词频被旧噪声词压住）；
  4. 读音推导错了（多音字按单字首选音拼，如 空调 -> kongdiao）。
本脚本把前三种判开并给出结论，第四种靠第二列人工确认。

用法:
  python3 tools/audit-dict.py 内卷 躺平 充电宝
  python3 tools/audit-dict.py --file words.txt        # 每行一个词
  python3 tools/audit-dict.py --dict ../xime-rs/data/dict.bin 简历

输出里带 `建议` 的行可以直接追加到 tools/extra-words.txt。
"""

import argparse
import os
import re
import struct
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mkdict as M

DEFAULT_DICT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "xime-rs", "data", "dict.bin"
)


def load_dict(path):
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEIM01":
        sys.exit(f"{path} 不是 xime 词库文件")
    wc, bl = struct.unpack_from("<2I", raw, 8)
    off = 32
    words = raw[off : off + wc * 12]
    blob = raw[off + wc * 12 : off + wc * 12 + bl]

    def cstr(o):
        end = blob.index(b"\0", o)
        return blob[o:end].decode()

    by_key = defaultdict(list)
    forms = set()
    for i in range(wc):
        ko, vo, fr = struct.unpack_from("<3I", words, i * 12)
        k, v = cstr(ko), cstr(vo)
        by_key[k].append((v, fr))
        forms.add(v)
    for v in by_key.values():
        v.sort(key=lambda x: -x[1])
    return by_key, forms


def pinyin_of(word, phrases, pinyins):
    """推导拼音键：词组读音表优先，其次单字首选音。返回值同时给出是否可靠。"""
    if word in phrases and len(phrases[word]) == len(word):
        return "".join(M.toneless(r) for r in phrases[word]), True
    keys = []
    for c in word:
        rs = pinyins.get(c)
        if not rs:
            return None, False
        keys.append(M.toneless(rs[0]))
    # 单字有多个读音时，整词读音不可靠（多音字可能选错）
    dubious = [c for c in word if len(pinyins.get(c, [])) > 1]
    return "".join(keys), not dubious


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("words", nargs="*")
    ap.add_argument("--dict", default=DEFAULT_DICT)
    ap.add_argument("--file")
    ap.add_argument("--limit", type=int, default=6, help="同键内超过这个名次就提醒")
    a = ap.parse_args()

    words = list(a.words)
    if a.file:
        with open(a.file, encoding="utf-8") as f:
            words += [ln.strip() for ln in f if ln.strip() and not ln.startswith("#")]
    if not words:
        ap.error("给几个词，或用 --file")

    by_key, forms = load_dict(a.dict)
    phrases = M.load_phrases()
    pinyins = M.load_pinyin()

    ok = miss = deep = 0
    for w in words:
        key, reliable = pinyin_of(w, phrases, pinyins)
        if key is None:
            print(f"  {w:<8} ✗ 有字不在读音表里，请手写拼音")
            miss += 1
            continue
        lst = by_key.get(key, [])
        names = [v for v, _ in lst]
        tag = "" if reliable else "  (读音按单字首选音拼，多音字建议核对)"
        if w not in forms:
            print(f"  {w:<8} ✗ 缺失    key={key}   建议: {w}\t\t30000{tag}")
            miss += 1
        elif w not in names:
            # 词形存在，但不在这个键下：多半是同形异音词的另一读音
            print(f"  {w:<8} △ 词形存在但 key={key} 下没有它（同形异音？）{tag}")
            deep += 1
        else:
            rank = names.index(w) + 1
            if rank == 1:
                print(f"  {w:<8} ✓ 首选    key={key}{tag}")
                ok += 1
            elif rank <= a.limit:
                print(f"  {w:<8} ○ 第{rank}位  key={key}  首选「{names[0]}」{tag}")
                deep += 1
            else:
                print(f"  {w:<8} ✗ 第{rank}位（超出候选窗口）key={key}   建议: {w}\t\t30000{tag}")
                deep += 1

    print(f"\n首选 {ok}  需处理 {miss}  排位偏后 {deep}  （共 {len(words)}）")


if __name__ == "__main__":
    main()
