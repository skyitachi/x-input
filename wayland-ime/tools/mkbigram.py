#!/usr/bin/env python3
"""统计词形 bigram，输出 val_map.bin 与 bigram.bin。

这两个文件与 Rust 侧（xime-rs/src/bigram.rs、xime-rs/src/dict.rs）严格对齐：

val_map.bin（word_id -> val_id，长 = word_count）:
  magic(8) b"XIMEVM01" + n(4, word_count) + reserved(20)
  + n * u32，val_map[word_id] = 该词条所属词形的 val_id

bigram.bin（转移表，按 prev_val_id 分桶）:
  magic(8) b"XIMEBG01" + n_prev(4, val_count) + n_pairs(4) + reserved(16)
  + (n_prev + 1) * u32（offset[i] 是 prev=i 在 pairs 区的起始，offset[n_prev] 为总对数右端点）
  + n_pairs * (u32 next_val_id, i16 cost)，按 prev 分组、组内 next 升序

代价 cost = round(-log10(P(next|prev)) * 100)，P 由语料计数估计。未收录的二元对
Rust 侧查不到 -> 返回 0，即 backoff 回 unigram（退化为不付转移代价）。

为什么需要 val_id 这一层：转移概率 P(词|前词) 基于词形而非读音；xime 词条是
(拼音 key, 词形 val) 唯一，多音词形会有多条词条、多个 word_id。若转移表直接按
word_id 统计，多音词会被分裂、语料分词结果（只有词形没有读音）也无法映射回。
val_id 让多音词条共享同一个转移，且语料分词无需多音消歧。

用法:
  python3 tools/mkbigram.py \
      --dict xime-rs/data/dict.bin --corpus corpus.txt \
      --out-map xime-rs/data/val_map.bin --out-bigram xime-rs/data/bigram.bin \
      [--top 1000000] [--max-lines N]

依赖: pip install jieba（仅构建期，运行时零依赖）
"""

import argparse
import math
import os
import struct
import sys
from collections import Counter

MAGIC_MAP = b"XIMEVM01"
MAGIC_BG = b"XIMEBG01"


def load_dict(path):
    """解析 dict.bin -> [(key, val, freq)]，列表下标即 word_id。"""
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEIM01":
        sys.exit(f"{path} 不是 xime 词库文件")
    wc, bl = struct.unpack_from("<2I", raw, 8)
    off = 32
    refs = [struct.unpack_from("<3I", raw, off + i * 12) for i in range(wc)]
    blob = raw[off + wc * 12 : off + wc * 12 + bl]

    def cstr(o):
        end = blob.index(b"\0", o)
        return blob[o:end].decode()

    return [(cstr(k), cstr(v), f) for k, v, f in refs]


def build_val_map(rows):
    """按词形最高频降序分配 val_id，返回 (val_to_id, word_to_val)。"""
    # 同一词形多条词条（多音），取其最高词频作为排序依据，高频词形 val_id 小
    best = {}
    for _, v, f in rows:
        best[v] = max(best.get(v, 0), f)
    vals = sorted(best.keys(), key=lambda v: -best[v])
    val_to_id = {v: i for i, v in enumerate(vals)}
    word_to_val = [val_to_id[v] for _, v, _ in rows]
    return val_to_id, word_to_val


def greedy_segment(text, val_set, max_len):
    """按词库贪心最长匹配切分，返回词形列表（孤立字若不在词库则跳过）。

    用词库本身（而非 jieba）切分语料，保证 bigram 统计粒度与 xime 解码
    （词网格、整词优先）一致——否则 jieba 会把「你好吗」「是一个」这类 jieba 有、
    词库没有的长词切成整词，导致「你好→吗」「是→一个」等子词转移统计不足，
    进而让这些高频搭配背上偏高的 trans_cost。
    """
    out = []
    i = 0
    n = len(text)
    while i < n:
        for l in range(min(max_len, n - i), 0, -1):
            if text[i:i + l] in val_set:
                out.append(text[i:i + l])
                i += l
                break
        else:
            i += 1
    return out


def _count_chunk(lines, val_to_id, max_len):
    """子进程：对一段行做贪心切分并统计局部计数。"""
    pair = Counter()
    prev_count = Counter()
    unigram = Counter()
    seen = val_to_id.get
    for line in lines:
        ids = [seen(w) for w in greedy_segment(line, val_to_id, max_len)]
        for wid in ids:
            unigram[wid] += 1
        for p, n in zip(ids, ids[1:]):
            pair[(p, n)] += 1
            prev_count[p] += 1
    return pair, prev_count, unigram


def count_bigrams(corpus_path, val_to_id, max_lines):
    """按词库贪心切分语料，统计 (prev_id, next_id) 与 prev_id 的出现次数。

    按 CPU 核数分块并行：fork 模式下 val_to_id（14 万条词形映射）被子进程继承，
    无需跨进程序列化。语料小（< 核数×2000 行）时走单进程，省掉进程 fork 开销。
    """
    import multiprocessing as mp

    lines = []
    with open(corpus_path, encoding="utf-8") as f:
        for ln, line in enumerate(f):
            if max_lines and ln >= max_lines:
                break
            line = line.strip()
            if line:
                lines.append(line)

    pair = Counter()
    prev_count = Counter()
    unigram = Counter()
    nproc = min(mp.cpu_count(), 12)
    if len(lines) <= nproc * 2000:
        p, c, u = _count_chunk(lines, val_to_id, 6)
        pair.update(p)
        prev_count.update(c)
        unigram.update(u)
    else:
        chunk = (len(lines) + nproc - 1) // nproc
        chunks = [lines[i:i + chunk] for i in range(0, len(lines), chunk)]
        with mp.Pool(nproc) as pool:
            for p, c, u in pool.starmap(_count_chunk, [(ch, val_to_id, 6) for ch in chunks]):
                pair.update(p)
                prev_count.update(c)
                unigram.update(u)
    return pair, prev_count, unigram


def write_val_map(path, word_to_val):
    n = len(word_to_val)
    with open(path, "wb") as f:
        f.write(MAGIC_MAP)
        f.write(struct.pack("<I", n))
        f.write(b"\0" * 20)
        f.write(struct.pack(f"<{n}I", *word_to_val))
    print(f"生成 {path}（{n} 词条 -> {max(word_to_val) + 1} 词形）")


def write_bigram(path, val_count, pair, prev_count, unigram, top):
    # 取 top N 个二元对（按计数降序），再按 prev 分组、组内 next 升序。
    # 代价用 PMI（点互信息）而非条件概率：cost = -log(P(next|prev)/P(next))。
    # 条件概率 -log P(next|prev) 对高频 prev（如「是」）会把所有 next 的概率都稀释，
    # 让「是→一个」这类合理搭配背上不合理惩罚；PMI 按 next 在 prev 后「比整体更常见
    # 还是更少见」给奖励/惩罚，不受 prev 高频稀释——常见搭配 cost 为负（奖励），
    # 罕见搭配为正（惩罚）。
    items = pair.most_common(top)
    total = sum(unigram.values()) or 1
    groups = {}
    for (p, n), c in items:
        p_cond = c / prev_count[p]
        p_uni = unigram[n] / total
        cost = round((-math.log10(p_cond) + math.log10(p_uni)) * 100)
        # PMI 可负可正，裁剪到 i16 范围
        cost = max(-32768, min(32767, cost))
        groups.setdefault(p, []).append((n, cost))

    n_pairs = len(items)
    # 偏移表：offset[0]=0，offset[prev+1] 累加
    offsets = [0] * (val_count + 1)
    running = 0
    for prev in range(val_count):
        offsets[prev] = running
        running += len(groups.get(prev, ()))
    offsets[val_count] = running
    assert running == n_pairs

    with open(path, "wb") as f:
        f.write(MAGIC_BG)
        f.write(struct.pack("<II", val_count, n_pairs))
        f.write(b"\0" * 16)
        f.write(struct.pack(f"<{val_count + 1}I", *offsets))
        for prev in range(val_count):
            for n, cost in sorted(groups.get(prev, ())):
                f.write(struct.pack("<Ih", n, cost))
    print(f"生成 {path}（{n_pairs} 二元对 / {val_count} 词形）")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dict", default=os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "..", "xime-rs", "data", "dict.bin"))
    ap.add_argument("--corpus", required=True, help="清洗后的纯文本语料")
    ap.add_argument("--out-map", default=os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "..", "xime-rs", "data", "val_map.bin"))
    ap.add_argument("--out-bigram", default=os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "..", "xime-rs", "data", "bigram.bin"))
    ap.add_argument("--top", type=int, default=1_000_000)
    ap.add_argument("--max-lines", type=int, default=0, help="调试：只处理前 N 行")
    a = ap.parse_args()

    rows = load_dict(a.dict)
    val_to_id, word_to_val = build_val_map(rows)
    print(f"词条 {len(rows)}，词形 {len(val_to_id)}")

    pair, prev_count, unigram = count_bigrams(a.corpus, val_to_id, a.max_lines)
    print(f"二元对 {len(pair)} 种，前词计数 {len(prev_count)} 种")

    write_val_map(a.out_map, word_to_val)
    write_bigram(a.out_bigram, len(val_to_id), pair, prev_count, unigram, a.top)


if __name__ == "__main__":
    main()
