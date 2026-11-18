//! 英文输入：前缀补全 + 候选窗，常用度取自一万高频词。
//!
//! 英文不需要「解码」——用户敲的就是前缀，要做的事只有一件：查出以它开头的词，
//! 按常用度排序。所以这一层没有词网格、没有代价函数，只有一次二分加一段线性扫描，
//! 与中文的 `engine.rs` 完全独立（拼写规则和排名依据都不一样）。
//!
//! 数据 `data/en.bin` 由 `scripts/gen-en.py` 在构建期生成：词表按**字母序**存放
//! 供二分定位，同时保留**词频排名**供排序。两者缺一不可——字母序负责"找得到"，
//! 排名负责"第一个候选是 hello 而不是 helicopter"。

use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../data/en.bin");

const MAGIC: &[u8; 8] = b"XIMEEN01";
const HDR: usize = 16;
/// 每条索引：u32 词串偏移 + u16 词频排名
const ENTRY: usize = 6;

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn cstr_at(b: &'static [u8], off: usize) -> &'static str {
    let tail = &b[off..];
    let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
    // 词表在生成期已过滤为纯 ASCII 小写字母
    unsafe { std::str::from_utf8_unchecked(&tail[..end]) }
}

struct En {
    idx: &'static [u8],
    blob: &'static [u8],
    n: usize,
}

impl En {
    fn word(&self, i: usize) -> &'static str {
        cstr_at(self.blob, rd32(self.idx, i * ENTRY) as usize)
    }

    /// 词频排名，0 是最常用的词
    fn rank(&self, i: usize) -> u16 {
        rd16(self.idx, i * ENTRY + 4)
    }

    /// 第一个 >= prefix 的下标（字母序二分）
    fn lower_bound(&self, p: &[u8]) -> usize {
        let (mut lo, mut hi) = (0usize, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.word(mid).as_bytes() < p {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// 补全：收集所有以 `prefix` 开头的词，按词频排序后取前 `cap` 条
    ///
    /// 扫描是线性的而不是停在 cap 条——字母序靠后的词可能比靠前的常用得多，
    /// 截断扫描会让 "hello" 输给 "helicopter"。一万词量级下全扫也就几十微秒。
    fn complete(&self, prefix: &str, cap: usize, out: &mut Vec<(u16, &'static str)>) {
        out.clear();
        if cap == 0 || prefix.is_empty() {
            return;
        }
        let p = prefix.as_bytes();
        let mut i = self.lower_bound(p);
        while i < self.n {
            let w = self.word(i);
            if !w.as_bytes().starts_with(p) {
                break;
            }
            out.push((self.rank(i), w));
            i += 1;
        }
        out.sort_unstable_by_key(|(r, _)| *r);
        out.truncate(cap);
    }

    fn count(&self) -> usize {
        self.n
    }
}

fn data() -> &'static En {
    static D: OnceLock<En> = OnceLock::new();
    D.get_or_init(|| {
        assert_eq!(&RAW[..8], MAGIC, "data/en.bin 格式不符（重新运行 scripts/gen-en.py）");
        let n = rd32(RAW, 8) as usize;
        let bl = rd32(RAW, 12) as usize;
        let idx = &RAW[HDR..HDR + n * ENTRY];
        let blob = &RAW[HDR + n * ENTRY..HDR + n * ENTRY + bl];
        En { idx, blob, n }
    })
}

/// 词表词数（`--check` 展示用）
pub fn word_count() -> usize {
    data().count()
}

/// 大小写形态
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Case {
    /// 全小写
    Lower,
    /// 首字母大写（句首、或 Shift+首字母）
    Capitalize,
    /// 全大写（Caps Lock）
    Upper,
}

/// 按形态改写一个词
pub fn apply_case(w: &str, mode: Case) -> String {
    match mode {
        Case::Lower => w.to_ascii_lowercase(),
        Case::Upper => w.to_ascii_uppercase(),
        Case::Capitalize => {
            let mut s = w.to_ascii_lowercase();
            if let Some(c) = s.chars().next() {
                let up = c.to_ascii_uppercase();
                s.replace_range(..c.len_utf8(), &up.to_string());
            }
            s
        }
    }
}

/// 英文组字状态
pub struct EnEngine {
    /// 已输入的字母，保留用户敲的原始大小写
    pub buf: String,
    /// 候选（已按大小写形态改写），第一条即空格上屏项
    pub cands: Vec<String>,
    /// 第一个字母是大写（Shift+字母），用于句首大写
    cap_first: bool,
    /// Caps Lock 生效：整词大写
    upper_all: bool,
    /// 补全结果复用缓冲，避免每次按键分配
    scratch: Vec<(u16, &'static str)>,
}

impl EnEngine {
    pub fn new() -> EnEngine {
        EnEngine {
            buf: String::with_capacity(32),
            cands: Vec::with_capacity(9),
            cap_first: false,
            upper_all: false,
            scratch: Vec::with_capacity(64),
        }
    }

    pub fn active(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn preedit(&self) -> &str {
        &self.buf
    }

    /// 每次按键前由主循环同步 Caps Lock 状态
    pub fn set_caps(&mut self, on: bool) {
        self.upper_all = on;
    }

    /// 大小写形态的中文标签（调试输出用）
    pub fn case_mode_label(&self) -> &'static str {
        match self.case_mode() {
            Case::Lower => "小写",
            Case::Capitalize => "首字母大写",
            Case::Upper => "全大写",
        }
    }

    pub fn case_mode(&self) -> Case {
        if self.upper_all {
            Case::Upper
        } else if self.cap_first {
            Case::Capitalize
        } else {
            Case::Lower
        }
    }

    pub fn push_char(&mut self, ch: char, cap: usize) {
        if self.buf.is_empty() {
            self.cap_first = ch.is_uppercase();
        }
        self.buf.push(ch);
        self.recompute(cap);
    }

    pub fn backspace(&mut self, cap: usize) {
        self.buf.pop();
        if self.buf.is_empty() {
            self.cap_first = false;
            self.cands.clear();
        } else {
            self.recompute(cap);
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.cands.clear();
        self.cap_first = false;
    }

    /// 原样上屏的文本（用户敲的字母本身）
    pub fn raw(&self) -> String {
        self.buf.clone()
    }

    pub fn recompute(&mut self, cap: usize) {
        self.cands.clear();
        if self.buf.is_empty() {
            return;
        }
        // 留一条位给末尾的「原样」，否则补全占满时用户没法直接把字母送出去。
        // cap=1 时留不出位子，就只给补全——原样还能用回车拿。
        let want = cap.saturating_sub(1).max(1).min(cap);
        let lower = self.buf.to_ascii_lowercase();
        let mode = self.case_mode();

        data().complete(&lower, want, &mut self.scratch);
        for (_, w) in &self.scratch {
            self.cands.push(apply_case(w, mode));
        }

        // 缓冲区本身也是候选：没有补全时它就是唯一选项，Space 不会把字吞掉
        let raw = self.buf.clone();
        if self.cands.len() < cap && !self.cands.iter().any(|c| c.eq_ignore_ascii_case(&raw)) {
            self.cands.push(raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str, cap: usize) -> EnEngine {
        let mut e = EnEngine::new();
        for ch in s.chars() {
            e.push_char(ch, cap);
        }
        e
    }

    #[test]
    fn 词表规模与排序() {
        assert_eq!(word_count(), 10000, "词表应为 10000 词");
        let d = data();
        // 字母序：任意两个相邻词必须非降序
        for i in 1..d.count() {
            assert!(d.word(i - 1) <= d.word(i), "第 {i} 个词不是字母序");
        }
    }

    #[test]
    fn 前缀补全按词频排序() {
        // 直接查数据层，避免被「末尾附加原样」干扰
        let mut out = Vec::new();
        data().complete("hel", 9, &mut out);
        assert!(out.len() >= 5, "hel 的补全太少: {out:?}");

        // 排名必须单调非降：这是「首选是 help 而不是 helena」的全部依据
        for w in out.windows(2) {
            assert!(w[0].0 <= w[1].0, "候选未按词频排序: {out:?}");
        }
        let words: Vec<&str> = out.iter().map(|(_, w)| *w).collect();
        assert!(words.contains(&"hello"), "hel 应能补全出 hello: {words:?}");
        assert!(words.contains(&"help"), "hel 应能补全出 help: {words:?}");
        // 低频词不该挤进前九（helicopter 排名远在 hello 之后）
        assert!(!words.contains(&"helicopter"), "低频词不该进前九: {words:?}");
    }

    #[test]
    fn 补全结果是真前缀且不重复() {
        for p in ["a", "th", "con", "pre", "zz"] {
            let e = typed(p, 9);
            let mut seen = std::collections::HashSet::new();
            for c in &e.cands {
                assert!(
                    c.to_ascii_lowercase().starts_with(p),
                    "{p} 的候选 {c} 不是前缀匹配"
                );
                assert!(seen.insert(c.to_ascii_lowercase()), "{p} 的候选出现重复: {c}");
            }
        }
    }

    #[test]
    fn 无匹配时保留原样可上屏() {
        let e = typed("zzzq", 6);
        assert_eq!(e.cands.len(), 1, "无补全时应只剩原样: {:?}", e.cands);
        assert_eq!(e.cands[0], "zzzq");
    }

    #[test]
    fn 原样永远在候选里() {
        // 已经是个完整词时也要能被原样上屏（大小写不同也算）
        let e = typed("hello", 6);
        assert!(e.cands.iter().any(|c| c.eq_ignore_ascii_case("hello")));
        assert!(e.cands.len() <= 6, "候选数不得超过上限: {:?}", e.cands);
    }

    #[test]
    fn 大小写形态() {
        let e = typed("hel", 9);
        assert_eq!(e.case_mode(), Case::Lower);

        let e = typed("Hel", 9);
        assert_eq!(e.case_mode(), Case::Capitalize);
        let first = &e.cands[0];
        assert!(first.starts_with('H'), "句首应大写: {first}");
        assert!(
            first.chars().skip(1).all(|c| c.is_lowercase()),
            "只大写首字母，其余保持小写: {first}"
        );
        assert_eq!(first.to_lowercase(), "help", "候选内容应与小写输入一致");

        let mut e = typed("Hel", 9);
        e.set_caps(true);
        e.recompute(9);
        assert_eq!(e.case_mode(), Case::Upper);
        assert!(
            e.cands[0].chars().all(|c| c.is_uppercase() || !c.is_alphabetic()),
            "Caps Lock 下应整词大写: {}",
            e.cands[0]
        );
    }

    #[test]
    fn 退格与复位() {
        let mut e = typed("hel", 9);
        e.backspace(9);
        assert_eq!(e.buf, "he");
        assert!(e.cands.iter().all(|c| c.to_ascii_lowercase().starts_with("he")));
        e.backspace(9);
        e.backspace(9);
        assert!(!e.active());
        assert!(e.cands.is_empty());
        assert_eq!(e.case_mode(), Case::Lower, "清空后不该残留大写状态");
    }

    #[test]
    fn 候选数不超过上限() {
        for cap in [1, 2, 6, 9] {
            let e = typed("a", cap);
            assert!(e.cands.len() <= cap.max(1), "cap={cap} 时出了 {} 条", e.cands.len());
        }
    }
}
