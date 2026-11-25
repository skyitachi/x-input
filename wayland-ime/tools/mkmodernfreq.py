#!/usr/bin/env python3
"""从近期语料（维基百科 dump）统计现代词频，输出 .cache/dict/modern_freq.txt。

为什么需要：mkdict.py 现在混的两套词频都停在 2014 年前后——
jieba 词典是 2014 年的新闻语料，OpenSubtitles 字幕更早。所以
「内卷 / 躺平 / 破防 / 充电宝 / 大模型 / 直播间」在两个源里计数都是 0，
只能靠 tools/extra-words.txt 人工补频。人工表的问题是：只补你知道的词，
而且词频是估的。用一份 2026 年的语料统一遍，这些问题就自动消失了。

两件事一起做：

1. **修正已有词的词频**：用当前词库对语料做贪心最长匹配切分（与 mkbigram.py
   同一套切分逻辑，保证粒度一致），统计每个词形在近期语料里的真实出现次数。
   「加班」在 2014 语料里输给「甲板」，在现代语料里不会。

2. **发现词库没有的新词**：对语料做 2-gram 统计，用「凝固度（PMI）+ 左右熵」
   筛出成词概率高的双字串，再反向验证它们没被词库覆盖。这是无监督新词发现的
   标准做法，不需要分词器，也不会把「的一」「是不」这类高频搭配选进来
   （它们的左右邻字太杂，熵很高但 PMI 极低）。

输出格式（每行 `词<TAB>计数`）供 mkdict.py 的 load_freq 读取：
  词库里已有的词 —— 近期语料中的出现次数
  新发现的词     —— 同一套计数口径

用法:
  python3 tools/wiki2txt.py                      # 先抽文本
  python3 tools/mkmodernfreq.py                  # 再统计
  python3 tools/mkmodernfreq.py --max-chars 30000000   # 只用前三千万字（调试）

依赖：无。切分用词库自身，n-gram 用纯 Python（2-gram 内存约 3GB / 亿字）。
"""

import argparse
import math
import os
import re
import struct
import sys
import time
from collections import Counter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = os.path.join(ROOT, ".cache")

# 只统计长度 2~6 的词形；单字不进这个文件（单字词频由 jieba 源负责）
MIN_LEN, MAX_LEN = 2, 6

# 2-gram 新词筛选阈值（在 1 亿字语料上标定，见 README）
MIN_PAIR_COUNT = 25     # 至少出现 25 次：8 次那档尽是「拘幽操/大便干燥」这类短语碎片
MIN_PMI = 6.0           # 凝固度：P(ab) 是 P(a)P(b) 的多少倍（自然对数）
MIN_ENTROPY = 1.2       # 左右熵（bits）：左右邻字至少这么杂，才像个独立词
MAX_ENTROPY = 9.0       # 过高的通常是虚词搭配（「的了」「我们」），靠词库已覆盖

# 维基语料里高频但与「用户会打的词」无关的元词，直接排除
# 助词/语气词几乎不可能出现在词的边界上（「的士」是极少数例外，且它已在词库）。
# 不挡的话 n-gram 会把「干燥地」「羞涩地看」这类"形容词+地"的短语当词收进来。
FUNC_EDGE = set("的地得了着过吗呢吧啊哦呀嘛")

BLOCK = {
    "维基", "条目", "页面", "编辑", "参考", "引用", "分类", "模板", "文件", "图像",
    "外部", "链接", "注释", "参见", "另见", "来源", "注释", "目录", "章节", "段落",
    "本页", "本条目", "该条目", "此条目", "编辑者", "贡献者", "许可", "协议",
    "讨论", "历史", "修订", "版本", "存档", "索引", "检索", "导航", "侧栏",
}

WORDS = set()
MAX_WORDLEN = 6

HAN = re.compile(r"^[\u3400-\u4dbf\u4e00-\u9fff]+$")
SPLIT = re.compile(r"[^\u3400-\u4dbf\u4e00-\u9fff]+")


def load_dict(path):
    """解析 dict.bin -> [(key, val, freq)]。格式见 xime-rs/src/dict.rs。"""
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEIM01":
        sys.exit(f"{path} 不是 xime 词库文件")
    wc, bl = struct.unpack_from("<2I", raw, 8)
    off = 32
    refs = [struct.unpack_from("<3I", raw, off + i * 12) for i in range(wc)]
    blob = raw[off + wc * 12: off + wc * 12 + bl]

    def cstr(o):
        return blob[o: blob.index(b"\0", o)].decode()

    return [(cstr(k), cstr(v), f) for k, v, f in refs]


def segment(text, words, maxlen):
    """贪心最长匹配，返回词形列表。未登录字串整段丢弃（不产生假词）。"""
    out = []
    i, n = 0, len(text)
    while i < n:
        for L in range(min(maxlen, n - i), 0, -1):
            w = text[i:i + L]
            if w in words:
                out.append(w)
                i += L
                break
        else:
            i += 1
    return out


def count_ngrams(lines, budget_chars):
    """一遍扫描：词库切分计数 + 单字/双字频次（新词发现用）。

    单字频次是 PMI 的分母，双字频次是分子；两者必须同一遍统计，
    否则语料口径不一致会让 PMI 整体偏移。
    """
    uni = Counter()
    bi = Counter()
    word_count = Counter()
    used = 0
    tick = 0
    t0 = time.time()
    for line in lines:
        used += len(line)
        segs = [s for s in SPLIT.split(line) if s]
        for s in segs:
            # 1) 词库切分：只统计词库里有的词形
            for w in segment(s, WORDS, MAX_WORDLEN):
                word_count[w] += 1
            # 2) n-gram：单字与相邻双字
            prev = ""
            for ch in s:
                uni[ch] += 1
                if prev:
                    bi[prev + ch] += 1
                prev = ch
        # 全量语料要跑十几分钟，没有进度输出会让人以为卡死了
        if used - tick >= 20_000_000:
            tick = used
            print(f"    {used/1e6:.0f} M 字，双字 {len(bi)/1e6:.1f} M 种，"
                  f"{time.time()-t0:.0f}s", flush=True)
        if budget_chars and used >= budget_chars:
            break
    return uni, bi, word_count, used


def discover(uni, bi, total_chars):
    """用 PMI + 左右熵筛双字候选。

    左右熵要有邻字分布才算得准，但保存每个双字的邻字 Counter 会吃掉几十 GB。
    折中：先按 PMI + 频次粗筛（这一步能滤掉 99% 的组合），再对剩下的几千个
    候选做第二遍扫描统计邻字。第二遍只在这些候选上工作，很快。
    """
    cands = []
    for w, c in bi.items():
        if c < MIN_PAIR_COUNT or w in BLOCK:
            continue
        a, b = w[0], w[1]
        ca, cb = uni.get(a, 0), uni.get(b, 0)
        if not ca or not cb:
            continue
        # PMI = ln( P(ab) / (P(a)P(b)) ) = ln( c * N / (ca * cb) )
        pmi = math.log(c * total_chars / (ca * cb))
        if pmi >= MIN_PMI:
            cands.append((w, c, pmi))
    cands.sort(key=lambda x: -x[1])
    return cands


def neighbor_entropy(lines, cand_set, budget_chars):
    """第二遍扫描：统计候选词的左/右邻字分布，返回 {词: (左熵, 右熵)}。

    邻字分布用 Counter 保存，候选只有几千个，内存可忽略。
    """
    left = {w: Counter() for w in cand_set}
    right = {w: Counter() for w in cand_set}
    used = 0
    tick = 0
    t0 = time.time()
    for line in lines:
        used += len(line)
        for s in SPLIT.split(line):
            for i in range(len(s) - 1):
                w = s[i:i + 2]
                if w not in cand_set:
                    continue
                if i > 0:
                    left[w][s[i - 1]] += 1
                if i + 2 < len(s):
                    right[w][s[i + 2]] += 1
        if used - tick >= 20_000_000:
            tick = used
            print(f"    {used/1e6:.0f} M 字，{time.time()-t0:.0f}s", flush=True)
        if budget_chars and used >= budget_chars:
            break

    def ent(counter):
        t = sum(counter.values())
        if t <= 0:
            return 0.0
        return -sum((c / t) * math.log2(c / t) for c in counter.values())

    return {w: (ent(left[w]), ent(right[w])) for w in cand_set}


def read_lines(path, budget_chars):
    used = 0
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            used += len(line)
            yield line
            if budget_chars and used >= budget_chars:
                return


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=os.path.join(CACHE, "corpus", "wiki.txt"))
    ap.add_argument("--dict", default=os.path.join(ROOT, "..", "xime-rs", "data", "dict.bin"))
    ap.add_argument("--out", default=os.path.join(CACHE, "dict", "modern_freq.txt"))
    ap.add_argument("--max-chars", type=int, default=0, help="只用前 N 字（0=全量）")
    ap.add_argument("--report", default="", help="候选新词报告路径（默认打印前 60 个）")
    a = ap.parse_args()

    if not os.path.exists(a.corpus):
        sys.exit(f"没有语料 {a.corpus}，先跑 tools/wiki2txt.py")

    global WORDS, MAX_WORDLEN
    rows = load_dict(a.dict)
    WORDS = {v for _, v, _ in rows}
    MAX_WORDLEN = max(MIN_LEN, min(MAX_LEN, max(len(v) for v in WORDS)))
    print(f"词库 {len(WORDS)} 个词形，最长 {MAX_WORDLEN} 字")

    print("第一遍：词库切分 + n-gram ...", flush=True)
    uni, bi, wc, used = count_ngrams(read_lines(a.corpus, a.max_chars), a.max_chars)
    print(f"  语料 {used/1e6:.1f} M 字；切出 {len(wc)} 个词形；"
          f"单字 {len(uni)}，双字 {len(bi)}", flush=True)

    print("PMI 粗筛 ...", flush=True)
    cands = discover(uni, bi, sum(uni.values()))
    print(f"  过 PMI 的候选 {len(cands)} 个", flush=True)

    cand_set = {w for w, _, _ in cands}
    print("第二遍：候选邻字熵 ...", flush=True)
    ent = neighbor_entropy(read_lines(a.corpus, a.max_chars), cand_set, a.max_chars)

    new_words = []
    for w, c, pmi in cands:
        le, re_ = ent[w]
        if min(le, re_) < MIN_ENTROPY or max(le, re_) > MAX_ENTROPY:
            continue
        if w in WORDS or w in BLOCK:
            continue
        if w[0] in FUNC_EDGE or w[-1] in FUNC_EDGE:
            continue
        new_words.append((w, c, pmi, le, re_))

    new_words.sort(key=lambda x: -x[1])
    print(f"\n新词候选（词库没有、PMI/熵都过）：{len(new_words)} 个")
    for w, c, pmi, le, re_ in new_words[:60]:
        print(f"   {w}  次数={c:<7} PMI={pmi:5.2f} 左熵={le:4.2f} 右熵={re_:4.2f}")

    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    with open(a.out, "w", encoding="utf-8") as f:
        f.write("# 近期语料词频（tools/mkmodernfreq.py 生成，勿手改）\n")
        f.write(f"# 语料 {used} 字；词库切分词形 {len(wc)}；新词 {len(new_words)}\n")
        for w, c in wc.most_common():
            if len(w) >= MIN_LEN:
                f.write(f"{w}\t{c}\n")
        # 新词追加在末尾；同一词若已出现（不可能，上面判过），以词库切分为准
        for w, c, _, _, _ in new_words:
            f.write(f"{w}\t{c}\n")

    print(f"\n输出 {a.out}（{os.path.getsize(a.out)/1048576:.2f} MB）")
    if a.report:
        with open(a.report, "w", encoding="utf-8") as f:
            for w, c, pmi, le, re_ in new_words:
                f.write(f"{w}\t{c}\t{pmi:.2f}\t{le:.2f}\t{re_:.2f}\n")
        print(f"候选报告 {a.report}")


if __name__ == "__main__":
    main()
