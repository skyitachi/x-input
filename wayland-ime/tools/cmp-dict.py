#!/usr/bin/env python3
"""对比两份 dict.bin，检查换语料后有没有把常用键的首选词挤掉。

为什么需要它：换词频源是「全局重排」，改完之后「感觉好像没问题」不算数——
必须有个客观数字。这个工具只看两件真正影响手感的事：

1. **常用键的首选变没变**（120 个高频拼音）。变了就是回归，要逐个解释。
2. **候选窗里被换掉的是哪些词**。全量统计变化键数，再抽样看具体词——
   如果换掉的是「是得/一就/不能很好地」这类旧语料的错误分词，那是收益；
   如果换掉的是「你好/谢谢」，那是事故。

用法:
  python3 tools/cmp-dict.py --old a.bin --new b.bin
  python3 tools/cmp-dict.py --old a.bin --new b.bin --keys nihao,xiexie --show 40
"""

import argparse
import struct
import sys
from collections import defaultdict

# 常用拼音键：覆盖问候、代词、疑问、时间、动作、名词、关联词。
# 这些键的首选一变，用户就会立刻察觉，所以单独盯住。
COMMON_KEYS = """
nihao xiexie zaijian duibuqi qingwen meiguanxi keyi buneng xihuan xuyao yinggai
women tamen ziji dajia nimen zanmen bieren dajia
shenme weishenme zenme nali zheli nage zhege naer zheer
jintian mingtian zuotian xianzai shijian shihou zaoshang wanshang xingqi nianyue
gongzuo shenghuo xuexi pengyou jiating haizi fumu laoshi xuesheng
chifan shuijiao heshui wanr chuqu huijia shangban xiaban jiaban
diannao shouji wangluo yinhang zhifu gouwu lvxing feiji huoche qiche ditie
zhongguo renmin shijie guojia chengshi difang dongxi wenti banfa jueding xiangfa
gongsi chanpin shichang kehu jingji zhengzhi wenhua jiaoyu kexue jishu fazhan
yisheng yiyuan jiankang yundong xiuxi kafei chifan mianshi gongzi
danshi yinwei suoyi ruguo suiran jingran ranhou erqie haiyou yijing
zhongyao zhuyao jiben yiban teshu putong jiandan fuza mingxian
anquan weixian baozhang baohu jiancha jiandu guanli zhiding zhixing
shurufa pinyin wenzhang wenjian tupian shipin yinyue dianying youxi
""".split()


def load(path):
    raw = open(path, "rb").read()
    if raw[:8] != b"XIMEIM01":
        sys.exit(f"{path} 不是 xime 词库文件")
    wc, bl = struct.unpack_from("<2I", raw, 8)
    off = 32
    wb = raw[off: off + wc * 12]
    blob = raw[off + wc * 12: off + wc * 12 + bl]

    def cstr(o):
        return blob[o: blob.index(b"\0", o)].decode()

    by = defaultdict(list)
    for i in range(wc):
        ko, vo, fr = struct.unpack_from("<3I", wb, i * 12)
        by[cstr(ko)].append((cstr(vo), fr))
    for v in by.values():
        v.sort(key=lambda x: -x[1])
    return by


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--old", required=True)
    ap.add_argument("--new", required=True)
    ap.add_argument("--keys", default="", help="额外要盯的拼音键，逗号分隔")
    ap.add_argument("--show", type=int, default=30, help="抽样条数")
    a = ap.parse_args()

    old, new = load(a.old), load(a.new)
    fo = {v for ws in old.values() for v, _ in ws}
    fn = {v for ws in new.values() for v, _ in ws}
    print(f"词形 {len(fo)} -> {len(fn)}（新增 {len(fn - fo)}，消失 {len(fo - fn)}）")

    keys = COMMON_KEYS + [k.strip() for k in a.keys.split(",") if k.strip()]
    regress = []
    for k in keys:
        o = old.get(k, [])
        n = new.get(k, [])
        ot = o[0][0] if o else "-"
        nt = n[0][0] if n else "-"
        if ot != nt:
            regress.append((k, ot, nt))
    print(f"\n常用键 {len(keys)} 个，首选变化 {len(regress)} 个")
    for k, o, n in regress:
        print(f"   {k:14} {o} -> {n}")

    # 候选窗（前 6，与 engine HITS 一致）的内容变化
    changed = 0
    dropped = []
    added = []
    for k, ws in old.items():
        o = [v for v, _ in ws[:6]]
        n = [v for v, _ in new.get(k, [])[:6]]
        if o == n:
            continue
        changed += 1
        dropped.extend((k, v) for v in o if v not in n)
        added.extend((k, v) for v in n if v not in o)
    print(f"\n候选窗内容变化的键 {changed} / {len(old)}；"
          f"换出 {len(dropped)} 条，换入 {len(added)} 条")
    if dropped:
        print(f"\n换出的词抽样（前 {a.show}）:")
        for k, v in dropped[:a.show]:
            print(f"   {k:14} -{v}")
    if added:
        print(f"\n换入的词抽样（前 {a.show}）:")
        for k, v in added[:a.show]:
            print(f"   {k:14} +{v}")


if __name__ == "__main__":
    main()
