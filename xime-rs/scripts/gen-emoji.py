#!/usr/bin/env python3
"""生成 emoji 词表与键索引。

数据来源（都是构建期一次性读取，生成结果提交进仓库，运行时零依赖）：

- emoji-test.txt    Unicode 官方测试数据，给出全部 fully-qualified emoji
                    与英文名（如 "grinning face"）
- CLDR annotations  中文注解（如 😀 -> 笑脸），需要 cldr-annotations-full
                    包里的 annotations/zh.json。取法（离线环境先从别处拷好即可）：
                    curl -sSL -o cldr.tgz \
                      https://registry.npmjs.org/cldr-annotations-full/-/cldr-annotations-full-45.0.0.tgz
                    tar xzf cldr.tgz package/annotations/zh/annotations.json
                    不传 --cldr 时只生成英文键，中文拼音键全部缺失。
- data/dict.bin     词库本身是「拼音 -> 词」，这里反向查单字读音，
                    把中文注解转成拼音键（笑脸 -> xiaolian）

输出：

- data/emoji.txt      每行 `<emoji>\\t<key...>`，供 mkemoji 读字符、也便于人查
- data/emoji-keys.bin 字母序键索引，运行时二分查前缀

只收单字形 emoji（单码点，或 码点+VS16 这种组合），ZWJ 序列与国旗这类多码点
合成全部跳过：候选窗按码点逐个查图集，画不了合成序列，硬收只会变成几个
emoji 并排。
"""
import argparse
import json
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

EMOJI_TEST = "/usr/share/unicode/emoji/emoji-test.txt"
DICT_BIN = os.path.join(ROOT, "data", "dict.bin")

VS16 = 0xFE0F

# 英文名里的虚词：单独作为键会命中一大片，且没有检索价值
STOPWORDS = {"with", "and", "of", "the", "a", "an", "in", "on", "for"}

# 中文注解里常见的「类别词」，单独作键会把半屏候选都点亮
CN_STOPWORDS = {"符号", "标记", "表情", "脸", "人", "物体", "其他", "颜色"}


def load_dict_pinyin(path):
    """从 dict.bin 反查单字拼音。

    词库布局见 wayland-ime/tools/mkdict.py: magic(8) + 词数/串长 + 词条表 + 字符串池。
    key 是拼音键（不含声调），value 是词。单字词条即得「字 -> 拼音」。
    """
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

    # 同一个字可能有多个读音（多音字），全都留着，生成键时任取其一
    py = {}
    for i in range(wc):
        ko, vo, fr = struct.unpack_from("<3I", words, i * 12)
        key, val = cstr(ko), cstr(vo)
        if len(val) == 1 and key.isascii() and key.isalpha():
            py.setdefault(val, []).append(key)
    return py


def is_han(c):
    return "\u4e00" <= c <= "\u9fff"


def cn_to_pinyin(word, pinyins):
    """中文词 -> 无空格全拼。有一个字查不到就整词放弃，避免半个拼音串。
    多音字取词库里的第一个读音：emoji 注解多是常用词，命中率还行。"""
    out = []
    for c in word:
        if not is_han(c):
            return None
        rs = pinyins.get(c)
        if not rs:
            return None
        out.append(rs[0])
    s = "".join(out)
    return s if 2 <= len(s) <= 24 else None


def parse_emoji_test(path):
    """返回 [(emoji_char, english_name, group)]，只留单字形 fully-qualified。"""
    items = []
    group = ""
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("# group:"):
                group = line.split(":", 1)[1].strip()
                continue
            if not line or line.startswith("#") or ";" not in line:
                continue
            codes, rest = line.split(";", 1)
            status = rest.split("#", 1)[0].strip()
            if status != "fully-qualified":
                continue
            cps = [int(x, 16) for x in codes.split()]
            # 单字形：一个码点，或「码点 + VS16」
            if len(cps) > 2:
                continue
            if len(cps) == 2 and cps[1] != VS16:
                continue
            base = cps[0]
            # 区域性指示符（国旗）、组合用附加符不单独成字
            if 0x1F1E6 <= base <= 0x1F1FF:
                continue
            name = rest.split("#", 1)[1].strip()
            # 名称形如 "😀 E1.0 grinning face"，去掉开头的 emoji 与版本号
            parts = name.split()
            while parts and (parts[0].startswith("E") and "." in parts[0] or not parts[0].isascii()):
                parts.pop(0)
            eng = " ".join(parts).strip()
            if not eng:
                continue
            items.append((chr(base), eng, group))
    return items


def keys_for(eng, cn_list, pinyins):
    """一个 emoji 的全部检索键。"""
    ks = set()
    words = [w for w in eng.lower().replace("-", " ").split() if w not in STOPWORDS]
    if words:
        ks.add("".join(words))  # 连写全名，方便 "grinningface" 这种整体检索
        for w in words:
            if len(w) >= 3 and w.isalpha():
                ks.add(w)
    for cn in cn_list:
        if not cn or cn in CN_STOPWORDS:
            continue
        py = cn_to_pinyin(cn, pinyins)
        if py:
            ks.add(py)
    # 键必须是纯小写 ASCII，运行时只做前缀比较，不做归一化
    return sorted(k for k in ks if k.isascii() and k.isalpha() and 2 <= len(k) <= 24)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--emoji-test", default=EMOJI_TEST)
    ap.add_argument("--cldr", help="CLDR annotations/zh.json")
    ap.add_argument("--dict", default=DICT_BIN)
    ap.add_argument("--out-txt", default=os.path.join(ROOT, "data", "emoji.txt"))
    ap.add_argument("--out-keys", default=os.path.join(ROOT, "data", "emoji-keys.bin"))
    a = ap.parse_args()

    if not os.path.exists(a.emoji_test):
        sys.exit(f"找不到 {a.emoji_test}（Ubuntu 上来自 unicode-data 包）")
    pinyins = load_dict_pinyin(a.dict)

    cn_map = {}
    if a.cldr:
        d = json.load(open(a.cldr, encoding="utf-8"))
        for cp, v in d["annotations"]["annotations"].items():
            cn_map[cp] = v.get("default", [])
        print(f"  CLDR 中文注解 {len(cn_map)} 条")

    items = parse_emoji_test(a.emoji_test)
    rows = []
    # 键 -> emoji 列表。用 dict 保序：同键内保持 emoji-test 的顺序，
    # 而 emoji-test 本身按 CLDR 常用度排列，第一个就是最常用的那个。
    by_key = {}
    for ch, eng, _group in items:
        ks = keys_for(eng, cn_map.get(ch, []), pinyins)
        if not ks:
            continue
        rows.append((ch, ks))
        for k in ks:
            by_key.setdefault(k, []).append(ch)

    pairs = [(k, ch) for k in sorted(by_key) for ch in by_key[k]]

    os.makedirs(os.path.dirname(a.out_txt), exist_ok=True)
    with open(a.out_txt, "w", encoding="utf-8") as f:
        f.write("# 由 scripts/gen-emoji.py 生成，勿手改。\n")
        f.write("# 格式：<emoji>\\t<检索键，空格分隔>\n")
        for ch, ks in rows:
            f.write(f"{ch}\t{' '.join(ks)}\n")

    blob = bytearray()
    idx = bytearray()
    for k, ch in pairs:
        off = len(blob)
        blob += k.encode()
        blob += b"\0"
        idx += struct.pack("<II", off, ord(ch))

    with open(a.out_keys, "wb") as f:
        f.write(b"XIMEEK01")
        f.write(struct.pack("<6I", len(pairs), len(blob), 0, 0, 0, 0))
        f.write(bytes(idx))
        f.write(bytes(blob))

    uniq = len({ch for _k, ch in pairs})
    print(f"  emoji {len(rows)} 个（有键 {uniq}），键 {len(pairs)} 条")
    print(f"  词表 -> {a.out_txt}")
    print(f"  键索引 -> {a.out_keys}（{os.path.getsize(a.out_keys)} 字节）")


if __name__ == "__main__":
    main()
