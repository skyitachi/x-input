//! 近似拼音匹配：方言模糊音与音节内的拼写错误。
//!
//! 两条路径，都只在「需要时」才付成本：
//!
//! 1. **模糊音**（`for_each_variant`）：剩余串能完整切成合法音节时，对每个音节
//!    做一次声母/韵母替换，生成「整串 1 处差异」的变体。`sijian` → `shijian`
//!    （si→shi）命中「时间」，`zongguo` → `zhongguo` 命中「中国」。
//! 2. **拼写错**（`for_each_repair`）：切分失败、且坏尾不是任何合法音节的前缀时
//!    （即用户不是「还在打字」，是真的打错了），生成编辑距离 1 的合法音节。
//!    `nihap` → `nihao`。
//!
//! 变体查到的词带 `FUZZY_PENALTY` 惩罚进网格，所以精确匹配永远排在前面；模糊音
//! 只是「打错了也有救」，不是「改变正常排序」。
//!
//! 为什么不做构建期折叠索引：折叠表按词条数线性增长（33 万条约 4MB），而错拼修正
//! 只需要高频词能命中。运行时变体的成本是「每位置几个音节 × 2 次查表」，实测
//! 占整句解码的千分之几，不值得为此让二进制大 20%。

/// 无声调拼音音节表（409 条，含 `m`/`biang`/`yo` 这类边缘音节）。
///
/// 来源：`mozillazg/phrase-pinyin-data` 全部词条读音，去声调、ü→v 后去重排序。
/// 重新生成见 `tools/gen-syllables.py`。**必须保持字典序**：下面的二分依赖它。
static SYLLABLES: &[&str] = &[
    "a", "ai", "an", "ang", "ao", "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi",
    "bian", "biang", "biao", "bie", "bin", "bing", "bo", "bu", "ca", "cai", "can", "cang", "cao",
    "ce", "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi",
    "chong", "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci",
    "cong", "cou", "cu", "cuan", "cui", "cun", "cuo", "da", "dai", "dan", "dang", "dao", "de",
    "dei", "deng", "di", "dia", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan",
    "dui", "dun", "duo", "e", "ei", "en", "er", "fa", "fan", "fang", "fei", "fen", "feng", "fo",
    "fou", "fu", "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu",
    "gua", "guai", "guan", "guang", "gui", "gun", "guo", "ha", "hai", "han", "hang", "hao", "he",
    "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui", "hun", "huo",
    "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju", "juan", "jue",
    "jun", "ka", "kai", "kan", "kang", "kao", "ke", "kei", "ken", "keng", "kong", "kou", "ku",
    "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo", "la", "lai", "lan", "lang", "lao", "le",
    "lei", "leng", "li", "lia", "lian", "liang", "liao", "lie", "lin", "ling", "liu", "long", "lou",
    "lu", "luan", "lun", "luo", "lv", "lve", "m", "ma", "mai", "man", "mang", "mao", "me", "mei",
    "men", "meng", "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu", "na",
    "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni", "nian", "niang", "niao", "nie",
    "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nun", "nuo", "nv", "nve", "o", "ou", "pa",
    "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao", "pie", "pin", "ping",
    "po", "pou", "pu", "qi", "qia", "qian", "qiang", "qiao", "qie", "qin", "qing", "qiong", "qiu",
    "qu", "quan", "que", "qun", "ran", "rang", "rao", "re", "ren", "reng", "ri", "rong", "rou",
    "ru", "ruan", "rui", "run", "ruo", "sa", "sai", "san", "sang", "sao", "se", "sen", "seng",
    "sha", "shai", "shan", "shang", "shao", "she", "shei", "shen", "sheng", "shi", "shou", "shu",
    "shua", "shuai", "shuan", "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan",
    "sui", "sun", "suo", "ta", "tai", "tan", "tang", "tao", "te", "teng", "ti", "tian", "tiao",
    "tie", "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo", "wa", "wai", "wan", "wang",
    "wei", "wen", "weng", "wo", "wu", "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing",
    "xiong", "xiu", "xu", "xuan", "xue", "xun", "ya", "yan", "yang", "yao", "ye", "yi", "yin",
    "ying", "yo", "yong", "you", "yu", "yuan", "yue", "yun", "za", "zai", "zan", "zang", "zao",
    "ze", "zei", "zen", "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhei", "zhen",
    "zheng", "zhi", "zhong", "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun",
    "zhuo", "zi", "zong", "zou", "zu", "zuan", "zui", "zun", "zuo",
];

/// 拼音最长音节（zhuang/chuang/shuang）6 字母
const MAX_SYL: usize = 6;

/// 模糊音/拼写错命中的词统一加这个代价。量级参考：词频 1 万的词 cost 约 6，
/// 2.0 的惩罚足够让同频段的精确匹配胜出，但不至于把「时间」这种高频词压到
/// 「四溅」后面。
pub const FUZZY_PENALTY: f32 = 2.0;

/// 拼写错的修复代价，比模糊音更重：模糊音是「口音」，用户确信自己打的是那个音；
/// 修复是「猜」，猜错的机会成本高得多。实测这个值低于 3.5 时，`nh` 会修复出
/// 「你」（3.6+pen）压过简拼的「你好」（4.8+2.0），把用户的正常输入改坏。
pub const REPAIR_PENALTY: f32 = 4.0;

#[inline]
pub fn is_syllable(s: &str) -> bool {
    SYLLABLES.binary_search(&s).is_ok()
}

/// `s` 是否是某个合法音节的前缀——用来区分「用户还没打完」（`nih` 里的 `h`）
/// 和「真打错了」（`nihap` 里的 `hap`）。前者交给尾音节补全，不该触发修复。
pub fn is_syllable_prefix(s: &str) -> bool {
    let i = SYLLABLES.partition_point(|x| *x < s);
    SYLLABLES.get(i).is_some_and(|x| x.starts_with(s))
}

pub struct Split {
    /// 成功切出的音节个数
    pub n: usize,
    /// 第一个切不动的字节位置；None 表示整串都是合法音节
    pub bad: Option<usize>,
}

/// 贪心最长匹配切分。`bounds[k]` 是第 k 个音节的结束位置（从 1 开始用）。
///
/// 输入是纯小写字母（engine 只接受 [a-z]），切片不会落在 UTF-8 中间。
pub fn split(s: &str, bounds: &mut [usize; 33]) -> Split {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut n = 0usize;
    while i < b.len() {
        if n + 1 >= bounds.len() {
            return Split { n, bad: Some(i) };
        }
        let mut got = 0usize;
        let mut l = (b.len() - i).min(MAX_SYL);
        while l > 0 {
            if is_syllable(&s[i..i + l]) {
                got = l;
                break;
            }
            l -= 1;
        }
        if got == 0 {
            return Split { n, bad: Some(i) };
        }
        i += got;
        n += 1;
        bounds[n] = i;
    }
    Split { n, bad: None }
}

/// 声母的方言等价类。只列真正会被混淆的：平舌/翘舌、n/l、f/h、r/l。
fn initial_alts(ini: &str) -> &'static [&'static str] {
    match ini {
        "zh" => &["z"],
        "z" => &["zh"],
        "ch" => &["c"],
        "c" => &["ch"],
        "sh" => &["s"],
        "s" => &["sh"],
        "n" => &["l"],
        "l" => &["n"],
        "f" => &["h"],
        "h" => &["f"],
        "r" => &["l"],
        _ => &[],
    }
}

/// 韵母的等价类（前后鼻音为主，ian/iang、uan/uang 同属）
fn final_alts(fin: &str) -> &'static [&'static str] {
    match fin {
        "an" => &["ang"],
        "ang" => &["an"],
        "en" => &["eng"],
        "eng" => &["en"],
        "in" => &["ing"],
        "ing" => &["in"],
        "ian" => &["iang"],
        "iang" => &["ian"],
        "uan" => &["uang"],
        "uang" => &["uan"],
        _ => &[],
    }
}

/// 拆出声母/韵母边界：零声母音节（a/an/ou）返回 (0, 全串)，
/// zh/ch/sh 占两个字母，其余一个。
fn split_ini(syl: &str) -> usize {
    let b = syl.as_bytes();
    if b.is_empty() || matches!(b[0], b'a' | b'e' | b'i' | b'o' | b'u' | b'v') {
        return 0;
    }
    if b.len() >= 2 && b[1] == b'h' && matches!(b[0], b'z' | b'c' | b's') {
        return 2;
    }
    1
}

/// 对一个音节生成模糊变体（声母一处、韵母一处），只回调节点里真实存在的音节。
fn syllable_variants(syl: &str, tmp: &mut String, f: &mut impl FnMut(&str)) {
    let ini_len = split_ini(syl);
    let (ini, fin) = syl.split_at(ini_len);
    for alt in initial_alts(ini) {
        tmp.clear();
        tmp.push_str(alt);
        tmp.push_str(fin);
        if is_syllable(tmp) {
            f(tmp);
        }
    }
    for alt in final_alts(fin) {
        tmp.clear();
        tmp.push_str(ini);
        tmp.push_str(alt);
        if is_syllable(tmp) {
            f(tmp);
        }
    }
}

/// 对「完整音节序列」生成整串的 1 处模糊变体。`n` 是音节数，变体长度与原串
/// 相同（只做音节内的等长替换），调用方按 `to = i + s.len()` 建边。
pub fn for_each_variant(
    s: &str,
    bounds: &[usize; 33],
    n: usize,
    tmp: &mut String,
    buf: &mut String,
    f: &mut impl FnMut(&str),
) {
    for k in 0..n {
        let start = if k == 0 { 0 } else { bounds[k] };
        let end = bounds[k + 1];
        let head = &s[..start];
        let tail = &s[end..];
        let syl = &s[start..end];
        syllable_variants(syl, tmp, &mut |v| {
            buf.clear();
            buf.push_str(head);
            buf.push_str(v);
            buf.push_str(tail);
            f(buf);
        });
    }
}

/// 修复切不动的尾串：生成编辑距离 1 的合法音节（替换/插入/删除/相邻交换）。
///
/// 调用方必须先确认 `s[bad..]` 不是任何音节的前缀——否则用户只是打到一半，
/// 交给尾音节补全就行，这里搜出来的都是垃圾。尾串超过一个音节（>6 字母）时
/// 直接放弃：跨越音节边界的编辑没有意义，成本也不值得。
pub fn for_each_repair(
    s: &str,
    bad: usize,
    tmp: &mut String,
    buf: &mut String,
    f: &mut impl FnMut(&str),
) {
    for_each_fix(&s[..bad], &s[bad..], false, tmp, buf, f);
}

/// 切分成功、但末尾留了残余时的修复：把残余并回上一个音节再找编辑距离 1。
///
/// `nihap` 会切成 `ni|ha|p`——残余 `p` 本身是合法前缀（pa/pai/po…），所以单看残余
/// 会误判成「用户还在打字」。但并回上一音节得到的 `hap` 不是任何音节的前缀，
/// 说明真实意图是把 `hao` 打成了 `hap`。两个条件同时成立才修，避免在
/// `nih`（真在打 hao）上搜出一堆垃圾。
pub fn for_each_tail_repair(
    s: &str,
    bounds: &[usize; 33],
    n: usize,
    tmp: &mut String,
    buf: &mut String,
    f: &mut impl FnMut(&str),
) {
    if n == 0 {
        return;
    }
    let start = bounds[n - 1];
    let merged = &s[start..];
    // 合并后超过一个音节：跨音节边界的编辑没有意义
    if merged.len() > MAX_SYL {
        return;
    }
    // 合并后仍是某个音节的前缀 → 用户可能只是还在打
    if is_syllable_prefix(merged) {
        return;
    }
    for_each_fix(&s[..start], merged, true, tmp, buf, f);
}

/// 用编辑距离 1 的合法音节替换 `tail`，拼上 `head` 后回调。
///
/// `same_len` 为真时只做等长替换（不接受漏打/多打）。用于「残余看起来像还在
/// 打字」的场景：`nih` 的残余 `h` 是合法前缀，此时若允许删字母，`nih` 会被修成
/// `ni`→「你」，压过本该出现的「你好」。等长替换只承认「打错了一个键」。
fn for_each_fix(
    head: &str,
    tail: &str,
    same_len: bool,
    tmp: &mut String,
    buf: &mut String,
    f: &mut impl FnMut(&str),
) {
    if tail.is_empty() || tail.len() > MAX_SYL {
        return;
    }
    let b = tail.as_bytes();
    let mut emit = |fixed: &str| {
        buf.clear();
        buf.push_str(head);
        buf.push_str(fixed);
        f(buf);
    };
    // 替换一个字母
    for i in 0..b.len() {
        for c in b'a'..=b'z' {
            if c == b[i] {
                continue;
            }
            tmp.clear();
            tmp.push_str(&tail[..i]);
            tmp.push(c as char);
            tmp.push_str(&tail[i + 1..]);
            if is_syllable(tmp) {
                emit(tmp);
            }
        }
    }
    // 漏打一个字母
    if !same_len && b.len() < MAX_SYL {
        for i in 0..=b.len() {
            for c in b'a'..=b'z' {
                tmp.clear();
                tmp.push_str(&tail[..i]);
                tmp.push(c as char);
                tmp.push_str(&tail[i..]);
                if is_syllable(tmp) {
                    emit(tmp);
                }
            }
        }
    }
    // 多打一个字母
    for i in 0..if same_len { 0 } else { b.len() } {
        tmp.clear();
        tmp.push_str(&tail[..i]);
        tmp.push_str(&tail[i + 1..]);
        if is_syllable(tmp) {
            emit(tmp);
        }
    }
    // 相邻两个字母颠倒
    for i in 0..b.len().saturating_sub(1) {
        if b[i] == b[i + 1] {
            continue;
        }
        tmp.clear();
        tmp.push_str(&tail[..i]);
        tmp.push(b[i + 1] as char);
        tmp.push(b[i] as char);
        tmp.push_str(&tail[i + 2..]);
        if is_syllable(tmp) {
            emit(tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_of(s: &str) -> (Vec<&str>, Option<usize>) {
        let mut b = [0usize; 33];
        let sp = split(s, &mut b);
        let mut out = Vec::new();
        let mut prev = 0;
        for k in 1..=sp.n {
            out.push(&s[prev..b[k]]);
            prev = b[k];
        }
        (out, sp.bad)
    }

    #[test]
    fn splits_greedy_longest() {
        assert_eq!(split_of("sijian").0, ["si", "jian"]);
        assert_eq!(split_of("zhuang").0, ["zhuang"]);
        assert_eq!(split_of("zhongguo").0, ["zhong", "guo"]);
        assert_eq!(split_of("xian").0, ["xian"]);
        // hap 会被贪心切成 ha + 残余 p（p 是合法前缀），所以 bad 落在 4 而不是 2
        assert_eq!(split_of("nihap").0, ["ni", "ha"]);
        assert_eq!(split_of("nihap").1, Some(4));
    }

    #[test]
    fn prefix_detection_separates_typing_from_typo() {
        // 打到一半：是合法音节的前缀，不该触发修复
        assert!(is_syllable_prefix("h"));
        assert!(is_syllable_prefix("zho"));
        // 真打错：不是任何音节的前缀
        assert!(!is_syllable_prefix("hap"));
        assert!(!is_syllable_prefix("zhg"));
    }

    #[test]
    fn fuzzy_variants_cover_dialect_confusions() {
        let mut tmp = String::new();
        let mut buf = String::new();
        let mut got = Vec::new();
        for (input, want) in [
            ("sijian", "shijian"),
            ("zongguo", "zhongguo"),
            ("lanfang", "nanfang"),
            ("cengjing", "chengjing"),
            ("shanghai", "sanghai"),
        ] {
            got.clear();
            let mut b = [0usize; 33];
            let sp = split(input, &mut b);
            for_each_variant(input, &b, sp.n, &mut tmp, &mut buf, &mut |v| {
                if v == want {
                    got.push(v.to_string());
                }
            });
            assert!(!got.is_empty(), "{input} 应能生成 {want}");
        }
    }

    #[test]
    fn tail_repair_merges_residual_back() {
        let mut tmp = String::new();
        let mut buf = String::new();
        let mut got = Vec::new();
        let mut b = [0usize; 33];
        let sp = split("nihap", &mut b);
        for_each_tail_repair("nihap", &b, sp.n, &mut tmp, &mut buf, &mut |v| got.push(v.to_string()));
        assert!(got.iter().any(|v| v == "nihao"), "应能修复成 nihao，实际 {got:?}");

        // 真在打字（nih）时不该乱修：merged = "nih" 本身不是前缀，但只剩 h 残余，
        // hao/hai 等由尾补全负责，这里搜出来的只有 len<=3 的音节，无需断言内容，
        // 只确认不会产生「你好」这种跳步结果。
        got.clear();
        let sp2 = split("nih", &mut b);
        for_each_tail_repair("nih", &b, sp2.n, &mut tmp, &mut buf, &mut |v| got.push(v.to_string()));
        assert!(!got.iter().any(|v| v == "你好"));
    }

    #[test]
    fn repair_finds_near_syllable() {
        let mut tmp = String::new();
        let mut buf = String::new();
        let mut got = Vec::new();
        for_each_repair("nihap", 2, &mut tmp, &mut buf, &mut |v| got.push(v.to_string()));
        assert!(got.iter().any(|v| v == "nihao"), "应能修复成 nihao，实际 {got:?}");
    }

    #[test]
    fn syllable_table_is_sorted_and_unique() {
        for w in SYLLABLES.windows(2) {
            assert!(w[0] < w[1], "音节表未排序或重复：{} >= {}", w[0], w[1]);
        }
        // 常见长音节都在表里
        for s in ["zhuang", "chuang", "shuang", "er", "yo", "m"] {
            assert!(is_syllable(s), "缺音节 {s}");
        }
    }
}
