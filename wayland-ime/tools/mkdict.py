#!/usr/bin/env python3
"""把公开拼音数据源编译成 xime 内置词库（C 头文件）。

输入（由 scripts/fetch-dict.sh 下载到 .cache/dict/）：
  pinyin.txt          单字全部读音（Unicode 16 进制码点 -> 带调拼音）
  large_pinyin.txt    词组逐字读音（比按单字拼更可靠，多音字在词中读音正确）
  jieba_dict.txt      词频，偏新闻/书面语料
  zh_freq_50k.txt     词频，来自 OpenSubtitles，偏口语（简繁混杂，约 3% 繁体计数）

另有人工词表 tools/extra-words.txt：读音表和语料都没有的新词（小程序、蓝牙耳机、
涨薪），以及词频被同音旧词压住的词（加班 被 甲板 压住），只能人工补并指定词频。

为什么要把两套词频混起来：单用 jieba 时「你好」只有 725 次，「号码」却有 8000 次，
于是 "nihaoma" 会被解成「你/号码」；口语语料里「你好」有 5 万次，混合后才符合直觉。

混合前做简繁归一：语料里的繁体计数（什麼/裏/後）归并到简体主形，异体词
（甚么）归并到规范写法，否则繁体字会带着十万级词频挤进候选前列。

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

MAX_SAME_KEY = 6            # 同一拼音最多保留多少候选（必须与 engine.rs 的 HITS 一致）
MAX_LEN_CLASSES = 3         # 同一键内优先保证几个不同字数各有一条
MAX_INITIALS_WORDS = 32000  # 简拼索引最多收录多少词条
MAX_INI_PER_KEY = 4         # 同一简拼串保留几个候选（nh -> 你好/南海/女孩/...）

# 各字数的收录词频门槛。
#
# 这些值以前是 {2: 400, 3: 250, 4: 500, 5: 1200, 6: 2500}，直接按语料计数卡，
# 结果是**所有 2014 年之后出现的词都进不来**：语料里「内卷/躺平/破防/充电宝/
# 直播间/小程序」的计数只有几十甚至 0，而它们的拼音读法在
# phrase-pinyin-data 里是齐的。门槛该挡的是长尾噪声，不是新词，所以把底压到很低。
MIN_FREQ = {2: 40, 3: 30, 4: 40, 5: 120, 6: 300}

# 语料里完全没有、但读音表里有的词，给的先验词频（低于真实高频词、高于噪声）
NO_CORPUS_FREQ = {2: 60, 3: 45, 4: 40, 5: 30, 6: 20}

# 「常用字」秩：仅靠读音表、没有语料证据的词，必须全部由常用字组成才收录。
#
# 门槛放低之后，读音表里的词条会整批涌进来，其中大量是cc-cedict 式的生僻词与
# 音译碎片（阿梨诃咃、暗灰鹃鵙、挨挨拶拶）。它们对输入法毫无价值，还会让词库
# 从 4.3MB 涨到 10.7MB。用「组词用字是否都在常用字表内」作筛子：充电宝/内卷/
# 直播间 全由常用字构成，照收；诃/咃/鹃/鵙/拶 这类字构成的词一律不要。
# 3500 对应《通用规范汉字表》一级字表的规模。
COMMON_CHAR_RANK = 3500

# 两套语料的权重：输入法更偏口语，但书面词也不能丢
# 词频源权重。前两套是 2014 年前后的存量语料（新闻 + 字幕），第三套是当期
# 维基百科（tools/mkmodernfreq.py 生成）。
#
# 三者不是线性混合，而是「存量语料管排序，现代语料只管提频」——原因是一个
# 实测出来的坑：线性混合（曾试过 0.28/0.32/0.40）会让百科体语料把口语词压下去。
# 例：「年假」在字幕源里 1487 次、在维基里 462 次，混合后输给了同音的「年甲」
# (751)，用户打 nianjia 首选变成垃圾词。维基是百科体，它的分布本来就不是
# 「人会输入什么」的模型，权重给多大都不合适。
#
# 现在的做法：先算存量语料的概率，现代语料只在更高时才覆盖它。
#   prob = max(prob_old, W_MODERN * prob_modern)
# 好处是单调——现代语料只能把词往前提，不可能把已有词挤下去，所以换语料
# 不会造成排序回归；新词（存量计数为 0）凭现代计数进来，这正是要的效果。
W_JIEBA = 0.45
W_SUB = 0.55
W_MODERN = 1.0
# 提频门槛：现代语料要明显更看好这个词才覆盖（至少 2 倍）。
# 卡掉的是「差一点点」的情况——那多半是百科体和口语体的题材差异，不是词真的
# 变常用。实测：不设门槛时「年甲」靠维基的 751 次越过字幕源的「年假」1487 次，
# 用户打 nianjia 首选就变成生僻词；设 2 倍门槛后这类小幅波动全部消失，
# 而存量语料完全没有计数的新词（内卷/充电宝/大模型）照进不误。
MODERN_MARGIN = 2.0
PROB_SCALE = 1e9            # 概率放大成整数词频
MODERN_FREQ = "modern_freq.txt"  # .cache/dict/ 下，由 tools/mkmodernfreq.py 生成

# 纯辅音音节（嗯/呒的方言读音）不做独立词条：输入 "m" 时应当补全成「吗」而不是「呒」
CONSONANT_ONLY = {"m", "n", "ng", "hm", "hng"}
# 常见但拼音特殊的字，挂到一个用户真正会敲的键上
KEY_ALIAS = {"嗯": "en"}

# ---- 简繁/异体清洗 ----------------------------------------------------------------
# 词频语料是简繁混杂的：OpenSubtitles 字幕约 3% 计数是繁体（什麼 57953、裏 17945…），
# jieba 里也有「甚么 r/著 n」这类异体词条。不做归一化的话，繁体计数会直接灌进词库，
# 候选窗里出现简繁混排（著 zhe 排到第 4 位、甚么排到第 2 位）。
#
# 三条规则（按顺序）：
# 1. t2s 归并：但不能裸用——OpenCC 的 t2s 会把「乾坤」误转成「干坤」（乾 qián 是
#    合法简体字），所以对 t2s 会改变的词再做 round-trip 校验：
#    s2t(t2s(w)) == s2t(w) → 真繁体，归并；否则 → t2s 误判，保留原形。
# 2. t2s 不认的同形异体字（著/甚）：逐字替换成规范字，但仅当替换形在语料里
#    计数占优才归并——「看著」→「看着」（字幕里的著几乎都是繁体的着）归并，
#    「著作」→「着作」（计数为 0）自动保留，无需手工维护词表。
#    注意 t2s(甚麼) = 甚么（「甚→什」的单字映射不存在），这步顺便把它纠到什么。

# 同形异体字替换表：字 → 规范简体字（配合计数占优判断，不是无差别替换）
CHAR_VARIANT = {"著": "着", "甚": "什"}

# 单字读音黑名单：这些读音下的字是繁体用法，不应进简体词库的单字候选。
# 「著」读 zhe/zhuo 时就是繁体「着」（看著/朝著）；读 zhu（著作）才是合法简体。
CHAR_READING_DROP = {"著": {"zhe", "zhuo"}}


def _opencc():
    try:
        import opencc
    except ImportError:
        sys.exit(
            "需要 python3-opencc 做词频简繁归一：Arch 上 sudo pacman -S python-opencc，"
            "其他平台 pip install opencc"
        )
    return opencc.OpenCC("t2s"), opencc.OpenCC("s2t")


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


def make_norm():
    """构造词形归一化闭包：繁体归并 + 同形异体替换，供词频与词组收录共用。"""
    t2s, s2t = _opencc()
    raw = defaultdict(int)
    for name in ("jieba_dict.txt", "zh_freq_50k.txt"):
        for w, c in load_counts(name).items():
            raw[w] += c

    def norm(w):
        s = t2s.convert(w)
        if s != w and s2t.convert(s) == s2t.convert(w):
            w = s  # 真繁体：归到简体主形
        if any(c in CHAR_VARIANT for c in w):
            alt = "".join(CHAR_VARIANT.get(c, c) for c in w)
            if alt != w and raw[alt] > raw[w]:
                w = alt
        return w

    return norm


def load_freq():
    """三套语料按概率混合，返回 {词: 整数词频}。

    混合前先做简繁归一：繁体形计数（什麼/裏/後…）合并到简体主形，
    同形异体（著/甚）在替换形计数占优时归并到规范写法。这样繁体词频
    不会虚高，词库里也几乎不再出现繁体形词条（gen-zh.py 的 trad 位图
    只剩零星兜底）。

    第三套是 modern_freq.txt（tools/mkmodernfreq.py 从维基百科最新 dump
    统计）。前两套都停在 2014 年前后，新词计数为 0 是结构性的；补上当期
    语料后，「内卷/躺平/大模型/直播间」凭真实出现次数就能进来，不必再靠
    extra-words.txt 人工估频。

    现代语料是**提频**不是混合（见 W_MODERN 处的注释）：只在新值更高时覆盖，
    保证换语料不会让任何已有词往下掉。文件不存在时退回 0.45/0.55 双源。
    """
    norm = make_norm()
    jieba = load_counts("jieba_dict.txt")
    sub = load_counts("zh_freq_50k.txt")
    modern = load_counts(MODERN_FREQ)

    tj = sum(jieba.values()) or 1
    ts = sum(sub.values()) or 1
    prob = defaultdict(float)
    for w, c in jieba.items():
        prob[norm(w)] += W_JIEBA * c / tj
    for w, c in sub.items():
        prob[norm(w)] += W_SUB * c / ts

    if modern:
        tm = sum(modern.values()) or 1
        # 两套语料的总量口径不同，直接比较概率会把现代语料整体抬高一截
        # （「年甲」751 次就压过了字幕源里 1487 次的「年假」）。刻度不靠拍常量，
        # 用两套语料都有的中频词做基准，取比值中位数当换算系数：
        # 这样「现代语料只是另一份语料」而不是「更权威的语料」。
        shared = []
        for w, c in modern.items():
            if c < 50:
                continue
            o = prob.get(norm(w), 0.0)
            if o > 0:
                shared.append(o / (c / tm))
        alpha = 1.0
        if len(shared) >= 500:
            shared.sort()
            alpha = shared[len(shared) // 2]
            print(f"  现代语料刻度系数 {alpha:.3f}（{len(shared)} 个共有词取中位数）")
        for w, c in modern.items():
            w = norm(w)
            v = W_MODERN * alpha * c / tm
            # 只提不压，且要明显更好才覆盖（见 MODERN_MARGIN）
            o = prob.get(w, 0.0)
            if o <= 0.0 or v > o * MODERN_MARGIN:
                prob[w] = v
    return {w: max(1, min(int(p * PROB_SCALE), 0xFFFFFFFF)) for w, p in prob.items()}


def is_han(ch):
    o = ord(ch)
    return 0x3400 <= o <= 0x4DBF or 0x4E00 <= o <= 0x9FFF or 0xF900 <= o <= 0xFAFF


EXTRA_WORDS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "extra-words.txt")
EXTRA_DEFAULT_FREQ = 50000

THUOCL_DIR = os.path.join(CACHE, "thuocl")

# 清华开放词表命中、但语料里没有计数的词，给这个先验词频。
# 取值落在「刚好进候选窗、压不过真实高频词」的区间（词库中位数约 254，
# 前 10% 约 2300）。领域词表只负责证明「这是个词」，排序仍交给语料。
THUOCL_FREQ = {2: 300, 3: 250, 4: 200, 5: 120, 6: 80}


def load_thuocl():
    """读清华开放词表（THUOCL，MIT License），返回 {词: 命中的词表个数}。

    它的价值不是词频——各表的计数是领域内排名，量级不可比（财经 190 万、
    汽车 1.7 万）。价值在于「这个字串确定是一个词」：IT 1.6 万、财经 3800、
    医学 1.9 万、法律 9900，这些词的读音在 phrase-pinyin-data 里往往齐全，
    只是语料里没计数。有了这份证据，build_rows 就不会把它们当长尾噪声丢掉。
    """
    if not os.path.isdir(THUOCL_DIR):
        return {}
    out = {}
    for name in sorted(os.listdir(THUOCL_DIR)):
        if not name.endswith(".txt"):
            continue
        with open(os.path.join(THUOCL_DIR, name), encoding="utf-8", errors="ignore") as f:
            for line in f:
                parts = line.split()
                if not parts:
                    continue
                w = parts[0].lstrip("\ufeff")
                if 2 <= len(w) <= 6 and all(is_han(c) for c in w):
                    out[w] = out.get(w, 0) + 1
    return out


def load_extra_words(pinyins, phrases):
    """读人工补充词表（extra-words.txt），格式：词 <TAB> 拼音 <TAB> 词频。

    为什么需要人工词表，两套语料都替不了：

    1. **没有**：小程序 / 蓝牙耳机 / 涨薪 这类词在读音表和语料里都不存在，
       任何自动推导都补不上。
    2. **有但排不上**：语料是 2014 年前的，新词只有几十次计数，同音的旧噪声词
       却有几千次（「加班」输给「甲板」、「简历」输给「建立」、「内卷」输给
       「内眷」）。计数口径无法区分「新词」与「罕见词」，只能人工给词频。

    拼音可省略（自动取词组读音表，再退回单字首选音），词频可省略（取
    EXTRA_DEFAULT_FREQ）。词频与已有词条取大，所以本表只能提频、不能降频——
    给一个常见词随手填个数不会把它的排名弄坏。
    """
    if not os.path.exists(EXTRA_WORDS):
        return []
    rows, warn = [], 0
    with open(EXTRA_WORDS, encoding="utf-8") as f:
        for ln, line in enumerate(f, 1):
            line = line.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = line.split("\t")
            word = parts[0].strip()
            L = len(word)
            if not word or not all(is_han(c) for c in word):
                print(f"  {EXTRA_WORDS}:{ln} 词形不合法，跳过：{line!r}", file=sys.stderr)
                warn += 1
                continue
            try:
                # 第 2、3 列顺序随意：纯数字当词频，其余当拼音
                pinyin, freq = "", EXTRA_DEFAULT_FREQ
                for extra in parts[1:]:
                    e = extra.strip()
                    if not e:
                        continue
                    if e.isdigit():
                        freq = int(e)
                    else:
                        pinyin = e
            except ValueError:
                print(f"  {EXTRA_WORDS}:{ln} 词频不是整数，跳过：{line!r}", file=sys.stderr)
                warn += 1
                continue
            if pinyin:
                readings = pinyin.replace(",", " ").split()
            elif word in phrases and len(phrases[word]) == L:
                readings = phrases[word]
            else:
                readings = [pinyins[c][0] for c in word if c in pinyins]
            if len(readings) != L:
                print(f"  {EXTRA_WORDS}:{ln} 「{word}」拿不到逐字读音，请手写第二列", file=sys.stderr)
                warn += 1
                continue
            keys = [toneless(r) for r in readings]
            if any(not re.match(r"^[a-z]{1,6}$", k) or k in CONSONANT_ONLY for k in keys):
                print(f"  {EXTRA_WORDS}:{ln} 「{word}」读音不合规：{keys}", file=sys.stderr)
                warn += 1
                continue
            rows.append(("".join(keys), word, freq, "".join(k[0] for k in keys), L))
    if warn:
        print(f"  人工词表：{len(rows)} 条可用，{warn} 条被跳过")
    return rows


def build_rows():
    pinyins = load_pinyin()
    phrases = load_phrases()
    freqs = load_freq()
    norm = make_norm()
    thuocl = load_thuocl()
    if thuocl:
        print(f"  领域词表 {len(thuocl)} 个词（THUOCL）")

    # 1) 单字：只在词频表里出现过的字才收录，避免生僻字把图集撑大
    char_freq = {w: n for w, n in freqs.items() if len(w) == 1 and is_han(w)}
    char_rows = []
    for ch, readings in pinyins.items():
        n = char_freq.get(ch)
        if not n:
            continue
        for rank, r in enumerate(readings[:3]):
            key = toneless(r)
            # 繁体用法的读音不进单字候选（著 zhe/zhuo = 繁体「着」）
            if key in CHAR_READING_DROP.get(ch, ()):
                continue
            if not re.match(r"^[a-z]{1,6}$", key):
                continue
            if key in CONSONANT_ONLY:
                key = KEY_ALIAS.get(ch, "")
                if not key:
                    continue
            char_rows.append((key, ch, max(1, n >> (rank * 2))))

    # 2) 词组：收 2~6 字。门槛见 MIN_FREQ——只挡长尾，不挡新词
    min_freq = MIN_FREQ

    # 常用字表：字频降序取前 COMMON_CHAR_RANK 个。用来筛掉纯读音表带来的生僻词。
    common_chars = set()
    for ch, _ in sorted(char_freq.items(), key=lambda kv: -kv[1])[:COMMON_CHAR_RANK]:
        common_chars.add(ch)

    def clean_word(word):
        """没有语料证据的词，必须全部由常用字组成；有证据的照收（可能是专名）。"""
        return all(c in common_chars for c in word)

    def word_freq(word, L):
        n = freqs.get(word, 0)
        floor = NO_CORPUS_FREQ.get(L, 10)
        # 领域词表命中：语料没计数时给一个「进得了候选窗」的底。
        # 只提频、不改收录条件——THUOCL 里的生僻字词（医学/法律长尾）
        # 仍要过 clean_word，否则图集会被这些字撑大。
        if word in thuocl:
            floor = max(floor, THUOCL_FREQ.get(L, 100))
        if n:
            # 语料里只要有计数就是"存在证据"，但 2014 年之后的词计数往往只有几十，
            # 低于门槛就被砍掉了。这里把计数抬到字数对应的底，让「内卷/躺平/验证码」
            # 这类词凭"出现过"进入词库，排序仍由真实计数决定。
            #
            # 底必须 >= 收录门槛，否则「有证据」也白搭：NO_CORPUS_FREQ 的 5/6 字档
            # 原本是 30/20，而 MIN_FREQ 是 120/300——「存款准备金」在维基里出现 4 次、
            # 抬到 30 之后仍然过不了 120，直接被丢掉。两处口径必须对齐。
            return max(n, floor, MIN_FREQ.get(L, 10))
        # 语料里没有的词（新词、专业词）用"最冷字的字频"做先验，
        # 否则「中国人」这类词会因为没进语料而输给「中国/任」这种偶然切分。
        # 全无先验时退回按字数给的固定底，保证「充电宝」这类后造词不会消失。
        known = [char_freq[c] for c in word if c in char_freq]
        if len(known) != len(word):
            return floor
        return max(floor, int(0.02 * min(known)))

    ph_rows = []
    for word, readings in phrases.items():
        # 词组读音表（phrase-pinyin-data）同样简繁/异体混杂（甚么/甚麼/看著…），
        # 收录前先归一到规范写法，读音逐字对应不受影响
        word = norm(word)
        L = len(word)
        if L < 2 or L > 6 or not all(is_han(c) for c in word):
            continue
        if len(readings) != L:
            continue
        n = word_freq(word, L)
        if n < min_freq[L]:
            continue
        # 光有读音、没有语料证据的词，还要求全由常用字组成（挡生僻词与音译碎片）
        if word not in freqs and not clean_word(word):
            continue
        keys = [toneless(r) for r in readings]
        if any(not re.match(r"^[a-z]{1,6}$", k) for k in keys):
            continue
        if any(k in CONSONANT_ONLY for k in keys):
            continue
        ph_rows.append(("".join(keys), word, n, "".join(k[0] for k in keys), L))

    # 3) 词组表没覆盖、但语料里有的词：用单字首选读音兜底。
    #    这条路径的读音是按单字首选音拼的，多音字会拼错（银行 -> yinxing），
    #    所以只收语料里有真实高频证据的词，且门槛不放水——低门槛只用于读音表路径。
    covered = set(phrases)
    # 人工词表里的词，读音由人给定，比单字首选音可靠。这里必须把它们排掉，
    # 否则同一个词会同时挂在两个键上：「重写」既按单字音进了 zhongxie（错，
    # 重=zhòng），又按人工读音进 chongxie（对），用户打 chongxie 反而找不到。
    manual = load_extra_words(pinyins, phrases)
    manual_forms = {row[1] for row in manual}
    extra_rows = []
    for word, n in freqs.items():
        L = len(word)
        if L < 2 or L > 4 or word in covered or word in manual_forms or n < min_freq[L]:
            continue
        if not all(is_han(c) for c in word):
            continue
        if not clean_word(word):
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
    merged.extend(manual)

    # 去重时按词频取大：人工词表给某词提频（如「简历」压过「建立」）要能生效，
    # 而不是被先出现的语料行遮掉
    best = {}
    for row in merged:
        ident = (row[0], row[1])
        if ident not in best or row[2] > best[ident][2]:
            best[ident] = row
    uniq = list(best.values())

    by_key = defaultdict(list)
    for row in uniq:
        by_key[row[0]].append(row)
    final = []
    for rows in by_key.values():
        rows.sort(key=lambda r: -r[2])
        # 同一拼音键内的「字数多样性」。
        #
        # 键是拼音串，"yue" 既可以是单字（月/越/约）也可以是双字词（余额/豫鄂）。
        # 单字的词频比词组高一个量级，纯按词频取前 N 条时整个键会被单字占满，
        # 余额这类词永远进不了候选——用户看到的就是「基本词没有」。
        # 所以先保证每个字数类各留一条最好的，再按词频补满剩余槽位。
        # 只对「同键存在多种字数」的情况生效：像 "yizhi" 全是双字词，行为不变。
        picked, have_len = [], set()
        for r in rows:
            if len(picked) >= MAX_SAME_KEY:
                break
            if r[4] not in have_len and len(have_len) < MAX_LEN_CLASSES:
                picked.append(r)
                have_len.add(r[4])
        for r in rows:
            if len(picked) >= MAX_SAME_KEY:
                break
            if r not in picked:
                picked.append(r)
        picked.sort(key=lambda r: -r[2])
        final.extend(picked[:MAX_SAME_KEY])
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
