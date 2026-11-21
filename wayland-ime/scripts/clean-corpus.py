#!/usr/bin/env python3
"""把维基百科中文 dump 清洗成纯文本，每行一句，供 tools/mkbigram.py 分词。

维基 XML 的正文 <text> 含大量标记：{{模板}}、[[链接]]、<ref>引用</ref>、
HTML 标签、表格 {|...|}、分类 [[Category:...]] 等。这里做「够用」的清洗——
bigram 统计对少量噪声不敏感，不追求逐字节干净。

用法: python3 scripts/clean-corpus.py --in zhwiki-pages-articles.xml.bz2 --out corpus.txt
"""

import argparse
import bz2
import re

CJK = re.compile(r"[一-鿿]")


def is_skip_title(title):
    # 含冒号的命名空间页（分类/模板/帮助/文件…）正文不是语料
    return ":" in title


def strip_templates(s):
    """去掉 {{...}}（正确处理嵌套）。"""
    out = []
    depth = 0
    i = 0
    n = len(s)
    while i < n:
        two = s[i:i + 2]
        if two == "{{":
            depth += 1
            i += 2
            continue
        if two == "}}" and depth > 0:
            depth -= 1
            i += 2
            continue
        if depth == 0:
            out.append(s[i])
        i += 1
    return "".join(out)


def strip_links(s):
    """[[目标|文字]] -> 文字；[[目标]] -> 目标；[[Category:xx]] -> 空。"""

    def repl(m):
        inner = m.group(1)
        if inner.startswith(("Category:", "分类:", "File:", "Image:", "文件:")):
            return ""
        if "|" in inner:
            return inner.split("|", 1)[1]
        return inner

    prev = None
    while prev != s:  # 反复替换，处理嵌套链接（少见，保险）
        prev = s
        s = re.sub(r"\[\[([^\[\]]*)\]\]", repl, s)
    return s


def clean_text(text):
    text = re.sub(r"<!--.*?-->", "", text, flags=re.S)                 # 注释
    text = re.sub(r"<ref[^>]*/>", "", text, flags=re.S)                # 引用
    text = re.sub(r"<ref[^>]*>.*?</ref>", "", text, flags=re.S)
    text = re.sub(r"\{\|.*?\|\}", "", text, flags=re.S)                # 表格
    text = strip_templates(text)
    text = strip_links(text)
    text = re.sub(r"<[^>]+>", "", text)                                # 剩余标签
    text = (text.replace("&nbsp;", " ").replace("&amp;", "&")
                .replace("&lt;", "<").replace("&gt;", ">"))
    return text


def split_sentences(text):
    """按句末标点切分，去掉空白，只保留含中文的行。"""
    text = re.sub(r"[。！？!?；;]", "\n", text)
    out = []
    for ln in text.split("\n"):
        ln = re.sub(r"\s+", "", ln)
        if len(ln) >= 2 and CJK.search(ln):
            out.append(ln)
    return out


def iter_pages(path):
    opener = bz2.open if path.endswith(".bz2") else open
    page = ""
    with opener(path, "rt", encoding="utf-8", errors="ignore") as f:
        for line in f:
            page += line
            if "</page>" in line:
                yield page
                page = ""
    if page:
        yield page


def parse_page(page):
    m = re.search(r"<title>(.*?)</title>", page, flags=re.S)
    if not m:
        return None, None
    title = m.group(1).strip()
    m = re.search(r"<text[^>]*>(.*?)</text>", page, flags=re.S)
    return title, (m.group(1) if m else "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="inp", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--max-pages", type=int, default=0, help="调试：只处理前 N 页")
    a = ap.parse_args()

    n_out = 0
    with open(a.out, "w", encoding="utf-8") as out:
        for i, page in enumerate(iter_pages(a.inp)):
            if a.max_pages and i >= a.max_pages:
                break
            title, text = parse_page(page)
            if title is None or is_skip_title(title):
                continue
            if text.startswith(("#REDIRECT", "#重定向")):
                continue
            for ln in split_sentences(clean_text(text)):
                out.write(ln + "\n")
                n_out += 1
    print(f"输出 {n_out} 行到 {a.out}")


if __name__ == "__main__":
    main()
