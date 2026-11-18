#!/usr/bin/env python3
"""把公开拼音数据源编译成 xime 内置词库（C 头文件）。

输入（由 scripts/fetch-dict.sh 下载到 .cache/dict/）：
  pinyin.txt          单字全部读音（Unicode 16 进制码点 -> 带调拼音）
  large_pinyin.txt    词组逐字读音（比按单字拼更可靠，多音字在词中读音正确）
  jieba_dict.txt      词频，偏新闻/书面语料
  zh_freq_50k.txt     词频，来自 OpenSubtitles，偏口语

为什么要把两套词频混起来：单用 jieba 时「你好」只有 725 次，「号码」却有 8000 次，
于是 "nihaoma" 会被解成「你/号码」；口语语料里「你好」有 5 万次，混合后才符合直觉。

输出：src/dict_gen.h —— 音节表、词条 blob、按拼音排序的索引、简拼索引。
"""

import os
import re
import sys
import unicodedata
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = os.path.join(ROOT, ".cache", "dict")
OUT = os.path.join(ROOT, "src", "dict_gen.h")

MAX_SAME_KEY = 12           # 同一拼音最多保留多少候选
MAX_INITIALS_WORDS = 32000  # 简拼索引最多收录多少词条
MAX_INI_PER_KEY = 4         # 同一简拼串保留几个候选（nh -> 你好/南海/女孩/...）

# 两套语料的权重：输入法更偏口语，但书面词也不能丢
W_JIEBA = 0.45
W_SUB = 0.55
PROB_SCALE = 1e9            # 概率放大成整数词频

# 纯辅音音节（嗯/呒的方言读音）不做独立词条：输入 "m" 时应当补全成「吗」而不是「呒」
CONSONANT_ONLY = {"m", "n", "ng", "hm", "hng"}
# 常见但拼音特殊的字，挂到一个用户真正会敲的键上
KEY_ALIAS = {"嗯": "en"}


def toneless(s):
    """带调拼音 -> 无声调小写拼音；ü 统一写成 v（与 "lv"/"nv" 音节表一致）。"""
    s = s.strip().lower()
    s = s.replace("u:", "v")
    for ch in "üǖǘǚǜ":
        s = s.replace(ch, "v")
    s = unicodedata.normalize("NFKD", s)
    s = "".join(c for c in s if not unicodedata.combining(c))
    return s.replace("'", "").replace("’", "").replace(" ", "")


def load_pinyin():
    """返回 {字符: [读音, ...]}，数据自带的首个读音即为常用读音。"""
    table = {}
    with open(os.path.join(CACHE, "pinyin.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            head, _, tail = line.partition(":")
            m = re.match(r"^U\+([0-9A-Fa-f]{4,6})$", head.strip())
            if not m:
                continue
            readings = [r.strip() for r in tail.split("#")[0].split(",") if r.strip()]
            if readings:
                table[chr(int(m.group(1), 16))] = readings
    return table


def load_phrases():
    """返回 {词组: [逐字拼音, ...]}。"""
    out = {}
    with open(os.path.join(CACHE, "large_pinyin.txt"), encoding="utf-8") as f:
        for line in f:
            if line.startswith("#") or ":" not in line:
                continue
            word, _, tail = line.rstrip("\n").partition(":")
            word = word.strip()
            readings = tail.split("#")[0].split()
            if word and readings:
                out[word] = readings
    return out


def load_counts(name):
    counts = {}
    path = os.path.join(CACHE, name)
    if not os.path.exists(path):
        return counts
    with open(path, encoding="utf-8") as f:
        for line in f:
            parts = line.split()
            if len(parts) < 2:
                continue
            try:
                n = int(parts[1])
            except ValueError:
                continue
            counts[parts[0]] = counts.get(parts[0], 0) + n
    return counts


def load_freq():
    """两套语料按概率混合，返回 {词: 整数词频}。"""
    jieba = load_counts("jieba_dict.txt")
    sub = load_counts("zh_freq_50k.txt")
    tj = sum(jieba.values()) or 1
    ts = sum(sub.values()) or 1
    prob = defaultdict(float)
    for w, c in jieba.items():
        prob[w] += W_JIEBA * c / tj
    for w, c in sub.items():
        prob[w] += W_SUB * c / ts
    return {w: max(1, min(int(p * PROB_SCALE), 0xFFFFFFFF)) for w, p in prob.items()}


def is_han(ch):
    o = ord(ch)
    return 0x3400 <= o <= 0x4DBF or 0x4E00 <= o <= 0x9FFF or 0xF900 <= o <= 0xFAFF


def build_rows():
    pinyins = load_pinyin()
    phrases = load_phrases()
    freqs = load_freq()

    # 1) 单字：只在词频表里出现过的字才收录，避免生僻字把图集撑大
    char_freq = {w: n for w, n in freqs.items() if len(w) == 1 and is_han(w)}
    char_rows = []
    for ch, readings in pinyins.items():
        n = char_freq.get(ch)
        if not n:
            continue
        for rank, r in enumerate(readings[:3]):
            key = toneless(r)
            if not re.match(r"^[a-z]{1,6}$", key):
                continue
            if key in CONSONANT_ONLY:
                key = KEY_ALIAS.get(ch, "")
                if not key:
                    continue
            char_rows.append((key, ch, max(1, n >> (rank * 2))))

    # 2) 词组：收 2~6 字。长词要求更高词频，避免长尾词把候选窗塞满
    min_freq = {2: 400, 3: 250, 4: 500, 5: 1200, 6: 2500}

    def word_freq(word):
        n = freqs.get(word, 0)
        if n:
            return n
        # 语料里没有的词（新词、专业词）用"最冷字的字频"做先验，
        # 否则「中国人」这类词会因为没进语料而输给「中国/任」这种偶然切分
        known = [char_freq[c] for c in word if c in char_freq]
        if len(known) != len(word):
            return 0
        return max(1, int(0.02 * min(known)))

    ph_rows = []
    for word, readings in phrases.items():
        L = len(word)
        if L < 2 or L > 6 or not all(is_han(c) for c in word):
            continue
        if len(readings) != L:
            continue
        n = word_freq(word)
        if n < min_freq[L]:
            continue
        keys = [toneless(r) for r in readings]
        if any(not re.match(r"^[a-z]{1,6}$", k) for k in keys):
            continue
        if any(k in CONSONANT_ONLY for k in keys):
            continue
        ph_rows.append(("".join(keys), word, n, "".join(k[0] for k in keys), L))

    # 3) 词组表没覆盖、但语料里有的词：用单字首选读音兜底（只收较高频的）
    covered = set(phrases)
    extra_rows = []
    for word, n in freqs.items():
        L = len(word)
        if L < 2 or L > 4 or word in covered or n < min_freq[L]:
            continue
        if not all(is_han(c) for c in word):
            continue
        keys, ok = [], True
        for ch in word:
            rs = pinyins.get(ch)
            if not rs:
                ok = False
                break
            k = toneless(rs[0])
            if not re.match(r"^[a-z]{1,6}$", k) or k in CONSONANT_ONLY:
                ok = False
                break
            keys.append(k)
        if ok:
            extra_rows.append(("".join(keys), word, n, "".join(k[0] for k in keys), L))

    merged = [(key, ch, n, key[0], 1) for key, ch, n in char_rows]
    merged.extend(ph_rows)
    merged.extend(extra_rows)

    seen = set()
    uniq = []
    for row in merged:
        ident = (row[0], row[1])
        if ident in seen:
            continue
        seen.add(ident)
        uniq.append(row)

    by_key = defaultdict(list)
    for row in uniq:
        by_key[row[0]].append(row)
    final = []
    for rows in by_key.values():
        rows.sort(key=lambda r: -r[2])
        final.extend(rows[:MAX_SAME_KEY])
    final.sort(key=lambda r: (r[0], -r[2]))

    syllables = set()
    for k, t, n, ini, L in final:
        if L == 1:
            syllables.add(k)
    for readings in phrases.values():
        for r in readings:
            k = toneless(r)
            if re.match(r"^[a-z]{1,6}$", k):
                syllables.add(k)

    hanzi = sorted({c for k, t, n, i, L in final for c in t if is_han(c)})
    return final, sorted(syllables), hanzi


def write_binary(rows, syllables, out_path):
    """输出供 Rust 版直接 include_bytes! 嵌入的二进制词库。

    设计目标是运行期零解析：所有条目按固定宽度排布，Rust 侧只需按偏移切片，
    不分配、不拷贝。头部固定 32 字节，后面按 words / blob / ini / ini_blob / syl 顺序排列。
    """
    import struct

    blob = []
    pos = 0
    refs = []
    for key, text, freq, ini, L in rows:
        key_b = key.encode()
        text_b = text.encode()
        key_off = pos
        blob.append(key_b + b"\0")
        pos += len(key_b) + 1
        val_off = pos
        blob.append(text_b + b"\0")
        pos += len(text_b) + 1
        refs.append((key_off, val_off, freq))
    blob_raw = b"".join(blob)

    ini_groups = defaultdict(list)
    for i, r in enumerate(rows):
        if 2 <= r[4] <= 4:
            ini_groups[r[3]].append(i)
    pairs = []
    for idxs in ini_groups.values():
        idxs.sort(key=lambda i: -rows[i][2])
        pairs.extend((rows[i][3], i) for i in idxs[:MAX_INI_PER_KEY])
    if len(pairs) > MAX_INITIALS_WORDS:
        pairs = sorted(pairs, key=lambda kv: -rows[kv[1]][2])[:MAX_INITIALS_WORDS]
    pairs.sort()

    ini_blob = []
    ini_pos = 0
    ini_refs = []
    for s, wid in pairs:
        b = s.encode()
        ini_refs.append((wid, ini_pos))
        ini_blob.append(b + b"\0")
        ini_pos += len(b) + 1
    ini_raw = b"".join(ini_blob)

    syl_raw = " ".join(syllables).encode()

    hdr = b"XIMEIM01"
    hdr += struct.pack("<IIIIII", len(refs), len(blob_raw), len(ini_refs), len(ini_raw),
                       len(syl_raw), 0)

    with open(out_path, "wb") as f:
        f.write(hdr)
        for key_off, val_off, freq in refs:
            f.write(struct.pack("<III", key_off, val_off, freq))
        f.write(blob_raw)
        for wid, off in ini_refs:
            f.write(struct.pack("<II", wid, off))
        f.write(ini_raw)
        f.write(syl_raw)

    size = os.path.getsize(out_path)
    print(f"生成 {out_path}")
    print(f"  词条 {len(refs)}  简拼 {len(ini_refs)}  音节 {len(syllables)}  共 {size / 1024 / 1024:.2f}MB")


def write_header(rows, syllables, hanzi):
    blob = []
    pos = 0
    refs = []
    for key, text, freq, ini, L in rows:
        key_b = key.encode()
        text_b = text.encode()
        key_off = pos
        blob.append(key_b + b"\0")
        pos += len(key_b) + 1
        val_off = pos
        blob.append(text_b + b"\0")
        pos += len(text_b) + 1
        refs.append((key_off, val_off, freq))

    # 简拼索引：同一简拼串保留若干高频词条，最终按简拼串排序供二分查找
    ini_groups = defaultdict(list)
    for i, r in enumerate(rows):
        if 2 <= r[4] <= 4:
            ini_groups[r[3]].append(i)
    pairs = []
    for idxs in ini_groups.values():
        idxs.sort(key=lambda i: -rows[i][2])
        pairs.extend((rows[i][3], i) for i in idxs[:MAX_INI_PER_KEY])
    if len(pairs) > MAX_INITIALS_WORDS:
        pairs = sorted(pairs, key=lambda kv: -rows[kv[1]][2])[:MAX_INITIALS_WORDS]
    pairs.sort()

    ini_blob = []
    ini_pos = 0
    ini_refs = []
    for s, wid in pairs:
        b = s.encode()
        ini_refs.append((wid, ini_pos))
        ini_blob.append(b + b"\0")
        ini_pos += len(b) + 1

    with open(OUT, "w", encoding="utf-8") as f:
        f.write("/* 由 tools/mkdict.py 生成，勿手改。\n")
        f.write(" * 数据来源（均为 MIT）：mozillazg/pinyin-data、mozillazg/phrase-pinyin-data、\n")
        f.write(" * fxsjy/jieba、hermitdave/FrequencyWords。重新生成：make dict\n */\n")
        f.write("#ifndef XIME_DICT_GEN_H\n#define XIME_DICT_GEN_H\n\n")
        f.write(f"#define XIME_DICT_WORD_COUNT {len(rows)}\n")
        f.write(f"#define XIME_DICT_INITIALS_COUNT {len(ini_refs)}\n\n")

        f.write("static const char xime_syllables[] =\n")
        line = '    "'
        for s in syllables:
            if len(line) + len(s) + 3 > 100:
                f.write(line + '"\n')
                line = '    "'
            line += s + " "
        f.write(line + '";\n\n')

        f.write("struct xime_word_ref {\n")
        f.write("    unsigned int key_off;\n    unsigned int val_off;\n    unsigned int freq;\n};\n\n")

        f.write("static const char xime_dict_blob[] =\n")
        buf = '    "'
        for chunk in blob:
            s = chunk.decode("utf-8", "surrogateescape").replace("\\", "\\\\").replace('"', '\\"')
            s = s.replace("\0", "\\0")
            if len(buf) + len(s) > 110:
                f.write(buf + '"\n')
                buf = '    "'
            buf += s
        f.write(buf + '";\n\n')

        f.write("static const struct xime_word_ref xime_dict_words[] = {\n")
        for key_off, val_off, freq in refs:
            f.write(f"    {{{key_off}, {val_off}, {freq}}},\n")
        f.write("};\n\n")

        f.write("struct xime_ini_ref {\n    unsigned int word;\n    unsigned int off;\n};\n\n")
        f.write("static const char xime_initials_blob[] =\n")
        buf = '    "'
        for chunk in ini_blob:
            s = chunk.decode("utf-8", "surrogateescape").replace('"', '\\"').replace("\0", "\\0")
            if len(buf) + len(s) > 110:
                f.write(buf + '"\n')
                buf = '    "'
            buf += s
        f.write(buf + '";\n\n')
        f.write("static const struct xime_ini_ref xime_initials[] = {\n")
        for wid, off in ini_refs:
            f.write(f"    {{{wid}, {off}}},\n")
        f.write("};\n\n#endif\n")

    print(f"生成 {OUT}")
    print(f"  词条 {len(rows)}  简拼 {len(ini_refs)}  字形 {len(hanzi)}  "
          f"blob {pos / 1024:.0f}KB  头文件 {os.path.getsize(OUT) / 1024 / 1024:.2f}MB")


def main():
    rows, syllables, hanzi = build_rows()
    if "--stats" in sys.argv:
        print(f"词条        : {len(rows)}")
        print(f"音节        : {len(syllables)}")
        print(f"字形        : {len(hanzi)}")
        print(f"最长拼音键  : {max(len(k) for k, t, n, i, L in rows)}")
        print("字数分布    : " + str(
            {L: sum(1 for _, _, _, _, x in rows if x == L) for L in range(1, 7)}))
        return
    if "--bin" in sys.argv:
        out = sys.argv[sys.argv.index("--bin") + 1]
        os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
        write_binary(rows, syllables, out)
        return
    write_header(rows, syllables, hanzi)


if __name__ == "__main__":
    main()
