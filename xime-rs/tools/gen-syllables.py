#!/usr/bin/env python3
"""生成 src/fuzzy.rs 里的无声调拼音音节表。

音节表是模糊音匹配的地基：切分输入要靠它，判断「这个替换结果是不是合法音节」
也要靠它。它必须**完整**（缺一个音节，那个音节的词就永远切不出来）且**有序**
（is_syllable 用二分）。

数据源用 `mozillazg/phrase-pinyin-data` 的词条读音（`wayland-ime` 构建词库时
下载的那份），不是手写的：手写表漏音节是必然的，而读音表覆盖了所有真实出现过的
音节。`ü` 按 mkdict.py 的约定统一写成 `v`（lv/nv/lve/nve），j/q/x/y 后的 ü 本来就
写作 u（ju/qu/xu/yu）。

用法:
  python3 tools/gen-syllables.py                  # 打印到 stdout
  python3 tools/gen-syllables.py --write          # 直接改 src/fuzzy.rs
  python3 tools/gen-syllables.py --readings PATH  # 指定 large_pinyin.txt

改完记得跑 `cargo test fuzzy`——表有序/无重复、以及几个长音节的存在性都有断言。
"""

import argparse
import os
import re
import sys
import unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_READINGS = os.path.join(HERE, "..", "..", "wayland-ime", ".cache", "dict", "large_pinyin.txt")
FUZZY_RS = os.path.join(HERE, "..", "src", "fuzzy.rs")

# 拼音音节最长 6 字母（zhuang/chuang/shuang）
MAX_LEN = 6


def toneless(s):
    """带调拼音 -> 无声调小写，ü 统一成 v（与 mkdict.py 的 toneless 一致）"""
    s = s.strip().lower().replace("u:", "v")
    for ch in "üǖǘǚǜ":
        s = s.replace(ch, "v")
    s = unicodedata.normalize("NFKD", s)
    s = "".join(c for c in s if not unicodedata.combining(c))
    return s


def collect(path):
    syl = set()
    with open(path, encoding="utf-8", errors="ignore") as f:
        for line in f:
            if ":" not in line or line.startswith("#"):
                continue
            _, py = line.split(":", 1)
            for part in py.split():
                p = toneless(part)
                if re.fullmatch(r"[a-z]{1,%d}" % MAX_LEN, p):
                    syl.add(p)
    return sorted(syl)


def render(syl):
    """排成每行 ~100 列的 Rust 数组字面量"""
    out, line = [], "   "
    for s in syl:
        item = f' "{s}",'
        if len(line) + len(item) > 100:
            out.append(line)
            line = "   "
        line += item
    if line.strip():
        out.append(line)
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--readings", default=DEFAULT_READINGS)
    ap.add_argument("--write", action="store_true", help="直接替换 src/fuzzy.rs 里的表")
    a = ap.parse_args()

    if not os.path.exists(a.readings):
        sys.exit(f"读不到 {a.readings}；先跑 wayland-ime/scripts/fetch-dict.sh 或指定 --readings")

    syl = collect(a.readings)
    if len(syl) < 350:
        sys.exit(f"只抽到 {len(syl)} 个音节，明显不完整（正常约 409），拒绝写入")
    body = render(syl)

    if not a.write:
        print(f"// 共 {len(syl)} 条")
        print(body)
        return

    src = open(FUZZY_RS, encoding="utf-8").read()
    new, n = re.subn(
        r"(static SYLLABLES: &\[&str\] = &\[\n).*?(\n\];)",
        lambda m: m.group(1) + body + m.group(2),
        src,
        flags=re.S,
    )
    if n != 1:
        sys.exit("在 src/fuzzy.rs 里找不到 SYLLABLES 表，格式变了？")
    open(FUZZY_RS, "w", encoding="utf-8").write(new)
    print(f"已更新 {FUZZY_RS}：{len(syl)} 个音节。下一步 cargo test fuzzy")


if __name__ == "__main__":
    main()
