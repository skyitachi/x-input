#!/usr/bin/env python3
"""生成简繁转换数据 data/zh.bin（供 Rust 版 include_bytes! 嵌入）。

为什么不在运行时调用 OpenCC：那会引入 C++ 依赖和词典文件，直接毁掉「单静态二进制、
零动态依赖」的前提。所以把转换结果在构建期压成三张小表：

  1. 单字映射（简 -> 繁，1727 条）：兜底任何词的逐字转换
  2. 词级例外（~2500 条）：逐字转换会错的词，例如
       头发 -> 頭髮（不是 頭發）   出发 -> 出發
       干净 -> 乾淨（不是 幹淨）   乾隆 -> 乾隆（不是 幹隆）
     这些靠 OpenCC 的整词转换得到正确结果，与逐字结果不同才收录。
  3. 每个词条的「繁体形」标记位图：词库里本来就有 `變成`/`变成` 这类重复条目，
     简体模式下必须把繁体形那一条过滤掉，否则候选窗里简繁混排。

另外输出 data/hant-chars.txt（全部繁体用字），交给 mkfont 一起烘焙进字形图集，
否则候选窗里的繁体字会是空白格。

用法: python3 scripts/gen-zh.py [--dict data/dict.bin] [--out data/zh.bin]
依赖: python3-opencc（Arch: pacman -S python-opencc；其他平台 pip install opencc）
"""

import argparse
import os
import struct
import sys

MAGIC = b"XIMEZH01"
HDR = 32  # magic(8) + 6*u32


def load_dict(path):
    """解析 dict.bin，返回 [(key, value, freq)]，顺序即词条索引。"""
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEIM01":
        sys.exit(f"{path} 不是 xime 词库文件")
    wc, bl, ic, ibl, sl, _ = struct.unpack_from("<6I", raw, 8)
    off = 32
    refs = [struct.unpack_from("<3I", raw, off + i * 12) for i in range(wc)]
    blob = raw[off + wc * 12 : off + wc * 12 + bl]

    def cstr(o):
        end = blob.index(b"\0", o)
        return blob[o:end].decode()

    return [(cstr(k), cstr(v), f) for k, v, f in refs]


def build(rows):
    try:
        import opencc
    except ImportError:
        sys.exit(
            "需要 python3-opencc：Arch 上 sudo pacman -S python-opencc，"
            "其他平台 pip install opencc"
        )
    s2t = opencc.OpenCC("s2t")
    t2s = opencc.OpenCC("t2s")

    values = [v for _, v, _ in rows]
    chars = sorted({c for v in values for c in v})

    # 1) 单字映射：只收「转出来还是单字」的，多字结果交给词级例外
    cmap = {}
    for c in chars:
        t = s2t.convert(c)
        if t != c and len(t) == 1:
            cmap[c] = t

    # 2) 词级例外：整词转换结果 != 逐字转换结果
    exceptions = {}
    for v in values:
        naive = "".join(cmap.get(c, c) for c in v)
        real = s2t.convert(v)
        if naive != real:
            exceptions[v] = real

    # 3) 繁体形词条标记：t2s 会改动它 -> 它本来就是繁体写法
    #    mongrel：虽然是繁体写法，但并不是规范繁体（头髮->頭髮 而词库里存的是錯的 頭發），
    #    这种混血条目两种模式都不应该出，否则繁体模式会漏出非规范写法。
    trad_bitmap = bytearray((len(rows) + 7) // 8)
    mongrel_bitmap = bytearray((len(rows) + 7) // 8)
    for i, (_, v, _) in enumerate(rows):
        s = t2s.convert(v)
        if s == v:
            continue
        trad_bitmap[i >> 3] |= 1 << (i & 7)
        if s2t.convert(s) != v:
            mongrel_bitmap[i >> 3] |= 1 << (i & 7)

    # 繁体用字全集：词库任意词条转换后可能出现的字，都要有字形
    hant_chars = sorted({c for v in values for c in s2t.convert(v)})

    return cmap, exceptions, bytes(trad_bitmap), bytes(mongrel_bitmap), hant_chars


def write_bin(path, rows, cmap, exceptions, trad_bitmap, mongrel_bitmap):
    cmap_pairs = sorted((ord(c), ord(t)) for c, t in cmap.items())
    phrase = sorted(exceptions.items())
    blob = bytearray()
    refs = []
    for s, t in phrase:
        sb = s.encode()
        so = len(blob)
        blob += sb + b"\0"
        to = len(blob)
        blob += t.encode() + b"\0"
        refs.append((so, to))

    hdr = MAGIC
    hdr += struct.pack(
        "<6I", len(cmap_pairs), len(refs), len(blob), len(rows), len(trad_bitmap), len(mongrel_bitmap)
    )

    with open(path, "wb") as f:
        f.write(hdr)
        for a, b in cmap_pairs:
            f.write(struct.pack("<II", a, b))
        for a, b in refs:
            f.write(struct.pack("<II", a, b))
        f.write(trad_bitmap)
        f.write(mongrel_bitmap)
        f.write(blob)

    size = os.path.getsize(path)
    print(f"生成 {path}")
    print(
        f"  单字映射 {len(cmap_pairs)}  词级例外 {len(refs)}  "
        f"繁体形词条 {bits(trad_bitmap)}（其中非规范写法 {bits(mongrel_bitmap)}）  "
        f"共 {size / 1024:.1f}KB"
    )


def bits(bm):
    return sum(bin(b).count("1") for b in bm)


def main():
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ap = argparse.ArgumentParser()
    ap.add_argument("--dict", default=os.path.join(here, "data", "dict.bin"))
    ap.add_argument("--out", default=os.path.join(here, "data", "zh.bin"))
    ap.add_argument(
        "--chars",
        default=os.path.join(here, "data", "hant-chars.txt"),
        help="输出繁体字符集，供 mkfont 烘焙字形",
    )
    a = ap.parse_args()

    rows = load_dict(a.dict)
    cmap, exceptions, trad_bitmap, mongrel_bitmap, hant_chars = build(rows)
    write_bin(a.out, rows, cmap, exceptions, trad_bitmap, mongrel_bitmap)

    with open(a.chars, "w", encoding="utf-8") as f:
        f.write("".join(hant_chars))
    print(f"生成 {a.chars}（{len(hant_chars)} 个繁体用字）")


if __name__ == "__main__":
    main()
