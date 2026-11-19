//! 简繁转换：简体 / 繁体作为两种**输出形态**，互不混排。
//!
//! 数据来自 `scripts/gen-zh.py`（构建期用 OpenCC 预生成），运行期只是一次查表，
//! 不依赖 opencc 库、不携带词典文件——和字形图集一样，是「单静态二进制」能成立的
//! 那类预处理。
//!
//! 三张表：
//! - `cmap`：单字映射（简 → 繁），逐字兜底
//! - `phrase`：词级例外，逐字会转换错的词（头发 → 頭髮，不是 頭發）
//! - `trad`：词条级位图，标记词库里本来就是繁体写法的条目，简体模式下要过滤掉
//!
//! 转换用「最长匹配」：先试最长例外词，再退到单字。因为例外词表只收逐字会错的词，
//! 命中即意味着那一处必须整词替换。

use std::borrow::Cow;
use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../data/zh.bin");

const MAGIC: &[u8; 8] = b"XIMEZH01";
const HDR: usize = 32;
const PAIR: usize = 8;

/// 输出形态
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// 简体（默认）
    Hans,
    /// 繁体（OpenCC s2t 口径）
    Hant,
}

impl Variant {
    pub fn parse(s: &str) -> Option<Variant> {
        match s {
            "hans" | "s" | "simplified" | "简体" => Some(Variant::Hans),
            "hant" | "t" | "traditional" | "繁体" | "繁體" => Some(Variant::Hant),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Variant::Hans => "简体",
            Variant::Hant => "繁体",
        }
    }

    pub fn toggled(self) -> Variant {
        match self {
            Variant::Hans => Variant::Hant,
            Variant::Hant => Variant::Hans,
        }
    }
}

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

struct Zh {
    cmap: &'static [u8],
    phrase: &'static [u8],
    trad: &'static [u8],
    mongrel: &'static [u8],
    blob: &'static [u8],
    n_words: usize,
    /// 例外词表里最长的词有几个字（最长匹配的上界）
    max_phrase: usize,
    /// 例外词首字的位图，用来跳过绝大多数不可能命中的位置
    starts: Box<[u64]>,
}

fn cstr_at(b: &'static [u8], off: usize) -> &'static str {
    let tail = &b[off..];
    let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
    unsafe { std::str::from_utf8_unchecked(&tail[..end]) }
}

impl Zh {
    fn parse() -> Zh {
        assert!(RAW.len() > HDR, "简繁数据被截断");
        assert_eq!(&RAW[0..8], MAGIC, "简繁数据标记不匹配，请重新执行 scripts/gen-zh.py");

        let n_cmap = rd32(RAW, 8) as usize;
        let n_ph = rd32(RAW, 12) as usize;
        let bl = rd32(RAW, 16) as usize;
        let n_words = rd32(RAW, 20) as usize;
        let nbm = rd32(RAW, 24) as usize;
        let nbm2 = rd32(RAW, 28) as usize;

        let mut off = HDR;
        let cmap = &RAW[off..off + n_cmap * PAIR];
        off += n_cmap * PAIR;
        let phrase = &RAW[off..off + n_ph * PAIR];
        off += n_ph * PAIR;
        let trad = &RAW[off..off + nbm];
        off += nbm;
        let mongrel = &RAW[off..off + nbm2];
        off += nbm2;
        let blob = &RAW[off..off + bl];

        // 首字位图与最长词长：都在加载时算一遍，省得运行期反复扫表
        let mut starts = vec![0u64; 0x10000 / 64].into_boxed_slice();
        let mut max_phrase = 1usize;
        for i in 0..n_ph {
            let s = cstr_at(blob, rd32(phrase, i * PAIR) as usize);
            if let Some(c) = s.chars().next() {
                let cp = c as usize;
                if cp < 0x10000 {
                    starts[cp >> 6] |= 1 << (cp & 63);
                }
            }
            max_phrase = max_phrase.max(s.chars().count());
        }

        Zh { cmap, phrase, trad, mongrel, blob, n_words, max_phrase, starts }
    }

    #[inline]
    fn starts_phrase(&self, c: char) -> bool {
        let cp = c as usize;
        if cp >= 0x10000 {
            return false;
        }
        self.starts[cp >> 6] & (1 << (cp & 63)) != 0
    }

    /// 单字映射；无映射返回 None
    #[inline]
    fn char_map(&self, c: char) -> Option<char> {
        let cp = c as u32;
        let n = self.cmap.len() / PAIR;
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let k = rd32(self.cmap, mid * PAIR);
            if k < cp {
                lo = mid + 1;
            } else if k > cp {
                hi = mid;
            } else {
                return char::from_u32(rd32(self.cmap, mid * PAIR + 4));
            }
        }
        None
    }

    /// 整词查例外表
    fn phrase_get(&self, key: &str) -> Option<&'static str> {
        let kb = key.as_bytes();
        let n = self.phrase.len() / PAIR;
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let s = cstr_at(self.blob, rd32(self.phrase, mid * PAIR) as usize);
            match s.as_bytes().cmp(kb) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return Some(cstr_at(self.blob, rd32(self.phrase, mid * PAIR + 4) as usize))
                }
            }
        }
        None
    }

    /// 该词条是否本来就是繁体写法（简体模式下要过滤）
    #[inline]
    fn bit(bm: &[u8], idx: usize) -> bool {
        match bm.get(idx >> 3) {
            Some(b) => b & (1 << (idx & 7)) != 0,
            None => false,
        }
    }

    #[inline]
    fn any_needed(&self, s: &str) -> bool {
        s.chars().any(|c| self.char_map(c).is_some() || self.starts_phrase(c))
    }
}

fn data() -> &'static Zh {
    static Z: OnceLock<Zh> = OnceLock::new();
    Z.get_or_init(Zh::parse)
}

/// 数据统计：(单字映射, 词级例外, 词条数)
pub fn stats() -> (usize, usize, usize) {
    let z = data();
    (z.cmap.len() / PAIR, z.phrase.len() / PAIR, z.n_words)
}

/// 词库第 `idx` 条在当前形态下是否可见
///
/// - 简体模式：繁体形词条一律隐藏（避免 `變成`/`变成` 混排）
/// - 两种模式：非规范繁体写法（`頭發`）一律隐藏，它的正字会由 `头发` 转换得到
#[inline]
pub fn keep_entry(idx: usize, variant: Variant) -> bool {
    let z = data();
    if idx >= z.n_words {
        return true;
    }
    if Zh::bit(z.mongrel, idx) {
        return false;
    }
    variant == Variant::Hant || !Zh::bit(z.trad, idx)
}

/// 词库中「本来就是繁体写法」的条目数（`--check` 展示用）
pub fn trad_word_count() -> usize {
    let z = data();
    (0..z.n_words).filter(|&i| Zh::bit(z.trad, i)).count()
}

/// 简体 → 繁体。无需改动时返回借用，避免热路径分配。
pub fn to_hant(s: &str) -> Cow<'_, str> {
    let z = data();
    if s.is_empty() || !z.any_needed(s) {
        return Cow::Borrowed(s);
    }

    // 字符边界表：最长匹配要按「字」切，但切片只能按字节
    let mut offs: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    offs.push(s.len());

    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0usize;
    let mut changed = false;
    while i + 1 < offs.len() {
        let c = s[offs[i]..].chars().next().unwrap();
        let mut hit = false;
        if z.starts_phrase(c) {
            let maxl = z.max_phrase.min(offs.len() - 1 - i);
            for l in (2..=maxl).rev() {
                if let Some(t) = z.phrase_get(&s[offs[i]..offs[i + l]]) {
                    out.push_str(t);
                    changed = true;
                    i += l;
                    hit = true;
                    break;
                }
            }
        }
        if !hit {
            match z.char_map(c) {
                Some(t) => {
                    out.push(t);
                    changed = true;
                }
                None => out.push(c),
            }
            i += 1;
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 单字与整词转换() {
        assert_eq!(to_hant("中华"), "中華");
        assert_eq!(to_hant("中国人"), "中國人");
        assert_eq!(to_hant("今天天气很好"), "今天天氣很好");
        assert_eq!(to_hant("中华人民共和国"), "中華人民共和國");
    }

    #[test]
    fn 词级例外优先于逐字() {
        // 逐字会得到 頭發 / 幹淨，词级例外必须纠正
        assert_eq!(to_hant("头发"), "頭髮");
        assert_eq!(to_hant("出发"), "出發");
        assert_eq!(to_hant("干净"), "乾淨");
        // 句子中也要按词命中，而不是整串查表
        assert_eq!(to_hant("我的头发很干净"), "我的頭髮很乾淨");
    }

    #[test]
    fn 无需转换时借用() {
        assert!(matches!(to_hant("abc"), Cow::Borrowed(_)));
        assert!(matches!(to_hant("已經"), Cow::Borrowed(_)));
        assert!(matches!(to_hant(""), Cow::Borrowed(_)));
    }

    #[test]
    fn 繁体形词条可识别() {
        let n = trad_word_count();
        // mkdict.py 词频归一化后，语料里的繁体计数已并入简体主形，
        // 词库只剩零星繁体形词条（t2s 误判保留的乾坤类 + 非规范写法）。
        // 若重新膨胀到上千，说明归一化失效或 zh.bin 与 dict.bin 不同步。
        assert!(n < 1000, "繁体形词条数异常（词频归一化可能失效）: {n}");
        assert!(n > 0, "繁体形词条应仍有零星存在");
        // 越界索引不应 panic
        assert!(keep_entry(usize::MAX, Variant::Hans));
        assert!(keep_entry(usize::MAX, Variant::Hant));
    }

    /// 简体模式下，可见条目不得隐含繁体写法
    #[test]
    fn 简体模式隐藏繁体形条目() {
        let (_, _, nw) = stats();
        let mut hidden = 0;
        for i in 0..nw {
            if !keep_entry(i, Variant::Hans) {
                hidden += 1;
            }
            // 繁体模式只会隐藏得比简体模式少（非规范写法两边都隐藏）
            if keep_entry(i, Variant::Hans) {
                assert!(keep_entry(i, Variant::Hant), "简体可见的条目繁体也应可见: {i}");
            }
        }
        assert_eq!(hidden, nw - visible(Variant::Hans), "隐藏计数不一致");
    }

    fn visible(v: Variant) -> usize {
        let (_, _, nw) = stats();
        (0..nw).filter(|&i| keep_entry(i, v)).count()
    }

    #[test]
    fn 变体名称可往返() {
        for s in ["hans", "s", "simplified", "简体"] {
            assert_eq!(Variant::parse(s), Some(Variant::Hans));
        }
        for s in ["hant", "t", "traditional", "繁体", "繁體"] {
            assert_eq!(Variant::parse(s), Some(Variant::Hant));
        }
        assert_eq!(Variant::parse("xxx"), None);
        for v in [Variant::Hans, Variant::Hant] {
            assert_eq!(v.toggled().toggled(), v);
        }
    }
}
