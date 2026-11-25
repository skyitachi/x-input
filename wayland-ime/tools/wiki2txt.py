#!/usr/bin/env python3
"""把中文维基百科 dump（bz2 XML）抽成纯文本句子，供 mkmodernfreq.py 统计近期词频。

为什么需要它：词库原来的两套词频都停在 2014 年前后（jieba 词典 2014、
OpenSubtitles 字幕更早），所以「内卷」「躺平」「充电宝」「大模型」在语料里
计数为 0，只能靠 tools/extra-words.txt 人工补。维基百科的 dump 每月更新，
用它统计一遍，新词、专名、技术词就能自己进来。

用法:
  python3 tools/wiki2txt.py                       # .cache/corpus/*.bz2 -> .cache/corpus/wiki.txt
  python3 tools/wiki2txt.py --jobs 10 --stride 6  # 全量 dump 均匀降采样（推荐）
  python3 tools/wiki2txt.py --in a.bz2 --out a.txt --max-pages 5000

三个参数值得解释：

`--jobs` 是多进程清洗。整条流水线里 bz2 解压 (~30 MB/s) 远快于 Python 正则
清洗 (~1 MB/s)，所以瓶颈全在 CPU；父进程只管解压和切页，清洗交给 worker，
12 核机器上接近 10 倍加速。

`--stride K` 是均匀降采样：只处理每 K 页中的 1 页。整份中文维基正文约 13 亿
字，全部统计既没必要（词频是概率量，2 亿字已足够稳定）也不可能（2-gram
统计按 3 GB/亿字吃内存）。**必须用 stride 而不是 --max-pages 截断**：dump 按
页面 ID 排序，前 20% 基本是 2003~2008 年创建的基础条目，截断等于把近代条目
全丢掉，新词反而统计不到。

清洗原则：宁少勿脏。维基正文里的模板、表格、脚注、外链对词频没贡献，
留着只会污染 n-gram；重定向页丢弃（它们只是指向别名，同一句话重复几千次
会把词频抬到不真实的水平）。抽出的每行是一句，长度不足 6 字的丢掉。
"""

import argparse
import bz2
import glob
import os
import re
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# 维基里高频但与「中文用词」无关的元词，抽文本时不必特判，交给 mkmodernfreq 的黑名单
REDIRECT = re.compile(r"^\s*#\s*(?:REDIRECT|重定向)", re.I)

RE_REF_SELF = re.compile(r"<ref[^>]*/\s*>", re.I)
RE_REF = re.compile(r"<ref[^>]*>.*?</ref>", re.I | re.S)
RE_COMMENT = re.compile(r"<!--.*?-->", re.S)
RE_TABLE = re.compile(r"\{\|.*?\|\}", re.S)
# 文件名/分类命名空间：简繁都要覆盖，否则繁体条目会漏出「圖像」「檔案」残渣。
# 注意这里**不能**写成 (?:\[\[[^\]]*\]\][^\]]*)* —— 那种嵌套量词在
# 含大量 [[ 的长文本上会指数回溯，实测能把一个 worker 卡死十几分钟。
# 文件链接把说明里的内层链接一起吃掉并不重要，宁可漏一点也不能炸。
RE_FILE = re.compile(
    r"\[\[\s*(?:File|Image|文件|图像|圖像|檔案|Category|分类|分類|Media|媒体)\s*"
    r":[^\[\]]*\]\]",
    re.I,
)
RE_LINK = re.compile(r"\[\[(?:[^\[\]|]*\|)?([^\[\]]*)\]\]")
RE_EXTLINK = re.compile(r"\[(?:https?:)?//[^\]]*\]")
RE_URL = re.compile(r"(?:https?:)?//\S+")
RE_TEMPLATE = re.compile(r"\{\{[^{}]*\}\}")
RE_HEADING = re.compile(r"^=+\s*(.*?)\s*=+\s*$", re.M)
RE_TAG = re.compile(r"<[^>]{1,200}>")
RE_QUOTE = re.compile(r"'{2,}")
RE_KEEP = re.compile(r"[^\u3400-\u4dbf\u4e00-\u9fff\u3000-\u303f\uff00-\uffef\s]")
RE_SENT_SPLIT = re.compile(r"[。！？；\n]+")
RE_SPACE = re.compile(r"[ \t\u3000]+")
# 模板删掉后剩下的日期骨架。数字先被 RE_KEEP 抹成空格，所以这里要吃掉空格：
# {{bd|1950年|1月|1日}} -> "1950 年 1 月 1 日" -> " 年 月 日"
RE_DATE_SHELL = re.compile(r"[\d\s]*年[\d\s]*月[\d\s]*日|[\d\s]+年[\d\s]*月")

# 页面级解析（在 bytes 上做，避免整页解码两次）
RE_PAGE_TITLE = re.compile(rb"<title>(.*?)</title>", re.S)
RE_PAGE_TEXT = re.compile(rb"<text[^>]*>", re.S)

# 单页正文上限。超过这个体积的基本是「列表/模板/资料汇总」类页面：正则开销大、
# 内容重复度极高（同一句式重复上万次），对词频统计是纯噪声。整份中文维基里
# 这类页面占不到 0.1%，直接跳过，顺便避免单页把语料搅偏。
MAX_PAGE_CHARS = 200_000

# 维基编辑残留：模板/说明文字被删干净后，正文里留下这些固定搭配。它们
# 在词频统计里没有价值，反而会造出「内容为」「空白之前」这种假词。
NOISE = ("已删除", "内容为", "此处插入", "链接标题", "斜体文字", "粗体文字",
         "空白之前", "在此处输入", "请勿删除", "本条目需要", "维基化")


def clean(s):
    """去掉 wiki 标记，只留中文正文。

    每个多行正则前面都加了「便宜的前置判断」：`{|` 与 `|}` 数量不匹配时
    惰性匹配会退化成 O(k·n)，这是维基清洗里最常见的卡死原因（实测两次
    全量抽取都停在同一页）。判断一个 `in` 是 O(n)，比回溯便宜几个数量级。
    """
    if "</ref>" in s or "/>" in s:
        s = RE_REF_SELF.sub("", s)
        if "</ref>" in s:
            s = RE_REF.sub("", s)
    if "-->" in s:
        s = RE_COMMENT.sub("", s)
    if "{|" in s and "|}" in s:
        s = RE_TABLE.sub("", s)
    if "[[" in s:
        s = RE_FILE.sub("", s)
    s = RE_EXTLINK.sub("", s)
    s = RE_URL.sub("", s)
    if "[[" in s:
        s = RE_LINK.sub(r"\1", s)
    # 模板可能嵌套，迭代到稳定（一般 3 轮够，深嵌套的会留下残渣，不影响统计）
    if "{{" in s:
        for _ in range(4):
            new = RE_TEMPLATE.sub("", s)
            if new == s:
                break
            s = new
    s = RE_HEADING.sub(r"\1。", s)
    s = RE_TAG.sub("", s)
    s = RE_QUOTE.sub("", s)
    s = RE_KEEP.sub(" ", s)
    s = RE_DATE_SHELL.sub("", s)
    return s


def usable(sent, min_len, max_len):
    """判断一个切分片段是否值得进语料。

    模板删干净后剩下的「年 月 日」「| name =」这类参数残渣有个共同特征：
    **空格比字多**（正常中文句子里几乎没有空格）。用空格密度一刀切，
    比逐条维护模板黑名单可靠得多。
    """
    n = len(sent)
    if n < min_len or n > max_len:
        return False
    spaces = sent.count(" ")
    if spaces * 3 >= n:          # 空格占比 >= 1/3：信息框残渣
        return False
    if n - spaces < min_len:     # 去掉空格后不足最短字数
        return False
    for bad in NOISE:
        if bad in sent:
            return False
    return True


def make_t2s():
    """构造繁简归一函数：整句转换，但只对确实含繁体字的句子调用。

    为什么要整句转而不是逐字转：opencc 的字表对单字覆盖不全（「甦」单字转换
    不变，「復甦」整词才转成「复苏」）。逐字转会留下「复甦」这种半繁半简的
    脏词，统计出来就是假词。

    中文维基是繁简混排的：同一概念在条目 A 写「網路」、条目 B 写「网络」，
    不归一的话一个词的计数被劈成两半，两边都进不了候选窗。

    先用单字转换扫一遍汉字区，能转换出不同结果的收集成繁体集合，句子命中
    才整句转——这样只有三成左右的句子走慢路径，整体速度可接受。
    """
    try:
        import opencc
        conv = opencc.OpenCC("t2s").convert
    except Exception:
        return lambda s: s

    trad = set()
    for cp in range(0x4E00, 0xA000):
        ch = chr(cp)
        if conv(ch) != ch:
            trad.add(ch)
    if not trad:
        return lambda s: s

    def t2s(s):
        for c in s:
            if c in trad:
                return conv(s)
        return s

    return t2s


# ---------------------------------------------------------------- 单进程路径

def iter_pages(path, max_pages=0):
    """流式解析 dump，yield (title, text)。不落盘解压，内存只留当前页。"""
    opener = bz2.open if path.endswith(".bz2") else open
    with opener(path, "rt", encoding="utf-8", errors="ignore") as f:
        title = None
        buf = None
        n = 0
        try:
            for line in f:
                if buf is not None:
                    end = line.find("</text>")
                    if end >= 0:
                        buf.append(line[:end])
                        text = "".join(buf)
                        buf = None
                        yield title or "", text
                        n += 1
                        if max_pages and n >= max_pages:
                            return
                    else:
                        buf.append(line)
                elif line.startswith("    <title>"):
                    title = line[11:].rstrip("</title>\n\r ")
                elif line.startswith("      <text"):
                    gt = line.find(">")
                    rest = line[gt + 1:]
                    end = rest.find("</text>")
                    if end >= 0:
                        yield title or "", rest[:end]
                        n += 1
                        if max_pages and n >= max_pages:
                            return
                    else:
                        buf = [rest]
        except EOFError:
            # 截断的测试切片会走到这里；已 yield 的页仍然有效
            print("  （输入在流结束标记前被截断，按已读部分处理）", file=sys.stderr)


# ---------------------------------------------------------------- 多进程路径

_W = {}


def _init_worker(keep_trad, min_len, max_len):
    _W["t2s"] = (lambda s: s) if keep_trad else make_t2s()
    _W["min_len"] = min_len
    _W["max_len"] = max_len


def _work(raw_pages):
    """把一个 <page> 批次清洗成句子列表。返回 (页数, 重定向数, 丢弃数, 句子)。"""
    t2s = _W["t2s"]
    min_len, max_len = _W["min_len"], _W["max_len"]
    n_page = n_redir = n_drop = n_huge = 0
    out = []
    for raw in raw_pages:
        n_page += 1
        mo = RE_PAGE_TEXT.search(raw)
        if not mo:
            continue
        rest = raw[mo.end():]
        end = rest.find(b"</text>")
        if end < 0:
            continue
        if end > MAX_PAGE_CHARS:
            n_huge += 1
            continue
        text = rest[:end].decode("utf-8", "ignore")
        if REDIRECT.match(text):
            n_redir += 1
            continue
        body = clean(text)
        for sent in RE_SENT_SPLIT.split(body):
            sent = RE_SPACE.sub(" ", sent).strip()
            if not usable(sent, min_len, max_len):
                n_drop += 1
                continue
            out.append(t2s(sent))
    return n_page, n_redir, n_drop, n_huge, out


def page_batches(path, batch=64, stride=1, max_pages=0):
    """在 bz2 流上按 </page> 切页，yield 批次。

    父进程只做「解压 + 切分」，正则与 opencc 全在 worker 里。

    解压必须用 bz2 模块在父进程内做，**不能** `subprocess` 起 bzip2 再读管道：
    multiprocessing 的 worker 是 fork 出来的，会连父进程持有的管道写端一起继承。
    bzip2 退出后写端仍有 worker 握着，父进程的 read() 永远等不到 EOF——
    实测就是这样卡到最后 20 分钟白跑。bz2 模块没有额外的 fd，不存在这个问题。
    """
    if path.endswith(".bz2"):
        stream = bz2.open(path, "rb")
    else:
        stream = open(path, "rb")

    buf = b""
    pages = []
    seen = 0
    stop = False
    try:
        while not stop:
            try:
                chunk = stream.read(8 << 20)
            except EOFError:
                # 截断的切片（head -c 出来的样本）会在这里结束；完整 dump 不会
                print("  （输入在流结束标记前被截断，按已读部分处理）", file=sys.stderr)
                break
            if not chunk:
                break
            buf += chunk
            while True:
                i = buf.find(b"</page>")
                if i < 0:
                    break
                raw = buf[:i]
                buf = buf[i + 7:]
                seen += 1
                if stride > 1 and seen % stride:
                    continue
                pages.append(raw)
                if len(pages) >= batch:
                    yield pages
                    pages = []
                if max_pages and seen >= max_pages:
                    stop = True
                    break
        if pages:
            yield pages
    finally:
        stream.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="src", default="",
                    help="dump 路径；留空则取 .cache/corpus/*.xml.bz2")
    ap.add_argument("--out", default=os.path.join(ROOT, ".cache", "corpus", "wiki.txt"))
    ap.add_argument("--max-pages", type=int, default=0, help="只处理前 N 页（调试用）")
    ap.add_argument("--stride", type=int, default=1,
                    help="每 N 页取 1 页（均匀降采样，见文件头说明）")
    ap.add_argument("--jobs", type=int, default=1, help="清洗进程数，0/1 表示单进程")
    ap.add_argument("--batch", type=int, default=64, help="每次派给 worker 的页数")
    ap.add_argument("--min-len", type=int, default=6, help="句子最短保留字数")
    ap.add_argument("--max-len", type=int, default=200, help="句子最长保留字数")
    ap.add_argument("--keep-trad", action="store_true", help="不做繁简归一（调试用）")
    a = ap.parse_args()

    src = a.src
    if not src:
        cands = sorted(glob.glob(os.path.join(ROOT, ".cache", "corpus", "*.xml.bz2")))
        if not cands:
            sys.exit("没有找到 dump，先跑 scripts/fetch-corpus.sh")
        src = cands[-1]

    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    n_page = n_line = n_char = n_skip_redir = n_drop = n_skip_huge = 0
    last_report = 0
    t0 = time.time()

    def report():
        el = time.time() - t0
        print(f"  {n_page} 页 / {n_line} 句 / {n_char/1e6:.1f} M 字 / {el/60:.1f} 分",
              flush=True)

    with open(a.out, "w", encoding="utf-8") as out:
        if a.jobs and a.jobs > 1:
            from multiprocessing import Pool
            pool = Pool(a.jobs, initializer=_init_worker,
                        initargs=(a.keep_trad, a.min_len, a.max_len))
            try:
                it = pool.imap_unordered(_work,
                                         page_batches(src, a.batch, a.stride, a.max_pages),
                                         chunksize=1)
                for p, r, d, h, lines in it:
                    n_page += p
                    n_skip_redir += r
                    n_drop += d
                    n_skip_huge += h
                    if lines:
                        out.write("\n".join(lines))
                        out.write("\n")
                        n_line += len(lines)
                        n_char += sum(len(x) for x in lines)
                    if n_page - last_report >= 20000:
                        last_report = n_page
                        report()
            finally:
                pool.terminate()
        else:
            t2s = (lambda s: s) if a.keep_trad else make_t2s()
            for title, text in iter_pages(src, a.max_pages):
                n_page += 1
                if REDIRECT.match(text):
                    n_skip_redir += 1
                    continue
                if len(text) > MAX_PAGE_CHARS:
                    n_skip_huge += 1
                    continue
                body = clean(text)
                for sent in RE_SENT_SPLIT.split(body):
                    sent = RE_SPACE.sub(" ", sent).strip()
                    if not usable(sent, a.min_len, a.max_len):
                        n_drop += 1
                        continue
                    sent = t2s(sent)
                    out.write(sent)
                    out.write("\n")
                    n_line += 1
                    n_char += len(sent)
                if n_page % 20000 == 0:
                    report()

    print(f"完成 {src}")
    print(f"  {n_page} 页（跳过重定向 {n_skip_redir}、超大页 {n_skip_huge}）"
          f"-> {n_line} 句, {n_char/1e6:.1f} M 字")
    print(f"  丢弃碎片 {n_drop} 条；输出 {a.out}（{os.path.getsize(a.out)/1048576:.1f} MB）")
    print(f"  耗时 {(time.time()-t0)/60:.1f} 分")


if __name__ == "__main__":
    main()
