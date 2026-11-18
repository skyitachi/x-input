//! 拼音解码器：在「词」构成的网格上做 N-best 路径搜索。
//!
//! 为什么不能贪心切分：拼音串没有分隔符，"xian" 可能是「先」也可能是「西安」；
//! 更麻烦的是 "nihaom" 能被完整解析成「你/哈/欧盟」，也可能该切成「你好/吗」。
//! 只在首选路径的首词上做变体，永远看不到后者——所以必须保留每个位置上的
//! 多条候选路径，最后统一按代价排序。
//!
//! 代价：每个词 = log10(总词频) - log10(词频)。一个词只付一次常数项，
//! 因此长词天然优于逐字拼，符合汉语构词习惯。

use crate::dict;
use crate::zh::{self, Variant};

pub const MAX_INPUT: usize = 32;
pub const CAND_MAX: usize = 128;

const HITS: usize = 4; // 同一拼音取几个同音候选参与搜索
const BEST: usize = 9; // 每个位置保留几条路径
const EDGES: usize = 96; // 每个位置最多几条边
const MAX_KEY: usize = 32; // 词条拼音键最长字节数

/// 词频总和的对数，相当于"每多切一个词就多付一次"的常数项。
/// 词频是概率乘以 1e9，所以理论上取 9；再大一点是为了补偿纯 unigram 模型
/// 缺少上下文约束——否则「你/号」这种高频单字拼接会压过「你好」这种整词。
const LOG_TOTAL: f32 = 10.0;
/// 补全未输完的尾部音节：按"缺一个音节"计固定惩罚，再按缺字数量轻微递增，
/// 否则输入 "n" 时「中国」会因为词频高而排在「你」前面。
const PARTIAL_BASE: f32 = 2.0;
const PARTIAL_SLOPE: f32 = 0.7;
/// 简拼（nh -> 你好）的惩罚，保证全拼结果优先
const INITIALS_PENALTY: f32 = 2.0;

struct Edge {
    to: usize,
    cost: f32,
    val: &'static str,
}

pub struct Engine {
    pub input: String,
    pub preedit: String,
    pub cands: Vec<String>,
    /// 各候选的解码代价，越小越可信（调试用）
    pub cost: Vec<f32>,
    /// 每个输入位置上的 N 条最优路径。跨次重算复用，避免每次按键都重新分配。
    paths: Vec<Vec<(f32, String)>>,
    /// 当前位置的出边缓冲，复用
    edges: Vec<Edge>,
    /// 词库查表结果缓冲，复用
    hits: Vec<dict::Hit>,
    /// 输出形态：简体（默认）/ 繁体
    pub variant: Variant,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            input: String::with_capacity(MAX_INPUT),
            preedit: String::new(),
            cands: Vec::with_capacity(CAND_MAX),
            cost: Vec::with_capacity(CAND_MAX),
            paths: Vec::new(),
            edges: Vec::with_capacity(EDGES),
            hits: Vec::with_capacity(HITS),
            variant: Variant::Hans,
        }
    }

    /// 切换输出形态，并立即按新形态重算当前候选
    pub fn set_variant(&mut self, v: Variant) {
        if v == self.variant {
            return;
        }
        self.variant = v;
        if self.active() {
            self.recompute();
        }
    }

    pub fn reset(&mut self) {
        self.input.clear();
        self.preedit.clear();
        self.cands.clear();
        self.cost.clear();
    }

    pub fn active(&self) -> bool {
        !self.input.is_empty()
    }

    pub fn push_char(&mut self, c: char) -> bool {
        if self.input.len() >= MAX_INPUT {
            return false;
        }
        self.input.push(c);
        self.recompute();
        true
    }

    pub fn backspace(&mut self) -> bool {
        if self.input.pop().is_none() {
            return false;
        }
        self.recompute();
        true
    }

    /// 内联候选：把拼音与编号候选塞进 preedit，由应用字体渲染。
    /// 合成器不支持 popup surface 时作为降级路径。
    pub fn inline_preedit(&self) -> String {
        let mut s = self.preedit.clone();
        if !self.cands.is_empty() {
            s.push_str("  ");
            for (i, c) in self.cands.iter().enumerate() {
                s.push_str(&format!("{}.{c} ", i + 1));
            }
        }
        s
    }

    fn recompute(&mut self) {
        self.cands.clear();
        self.cost.clear();
        self.preedit.clear();
        if self.input.is_empty() {
            return;
        }

        // u4f60 -> 你
        if let Some((cp, text)) = parse_codepoint(&self.input) {
            self.preedit = format!("U+{cp:04X}");
            self.cands.push(text);
            self.cost.push(0.0);
            return;
        }

        self.preedit = self.input.clone();

        let len = self.input.len();
        // 复用路径表：容量在多次重算之间保留，避免每键都重新分配 len+1 个 Vec
        if self.paths.len() < len + 1 {
            self.paths.resize_with(len + 1, Vec::new);
        }
        for p in self.paths.iter_mut().take(len + 1) {
            p.clear();
        }
        self.paths[0].push((0.0, String::new()));

        // 前向 N-best：所有边都从已到达的位置向右延伸，按 i 递增扫描即拓扑序
        for i in 0..len {
            if self.paths[i].is_empty() {
                continue;
            }
            self.collect_edges(i);
            if self.edges.is_empty() {
                continue;
            }
            // 拆成「i 及之前」与「i+1 及之后」两半：前者只读、后者只写，
            // 这样不必克隆 paths[i]，也不触发借用冲突。
            let (left, right) = self.paths.split_at_mut(i + 1);
            let cur = &left[i];
            for (pcost, ptext) in cur.iter() {
                for e in self.edges.iter() {
                    let slot = &mut right[e.to - (i + 1)];
                    push_path(slot, ptext, e.val, pcost + e.cost);
                }
            }
        }

        for (c, t) in self.paths[len].iter() {
            if self.cands.len() >= CAND_MAX {
                break;
            }
            // 繁体模式下在输出环节转换：路径本身仍按简体词条排名，
            // 这样简体/繁体两套排库共用同一套词频，不会因为转换而改变候选顺序。
            let text = if self.variant == Variant::Hant {
                match zh::to_hant(t) {
                    std::borrow::Cow::Borrowed(_) => t.clone(),
                    std::borrow::Cow::Owned(s) => s,
                }
            } else {
                t.clone()
            };
            if self.cands.iter().any(|x| *x == text) {
                continue;
            }
            self.cands.push(text);
            self.cost.push(*c);
        }
    }

    /// 枚举从位置 i 出发的所有边，写入复用的 edges 缓冲
    fn collect_edges(&mut self, i: usize) {
        self.edges.clear();
        let len = self.input.len();
        let rem = len - i;

        for l in 1..=rem.min(MAX_KEY) {
            let Some(sub) = self.input.get(i..i + l) else { continue };
            dict::exact_into(sub, HITS, self.variant, &mut self.hits);
            for h in &self.hits {
                self.edges.push(Edge { to: i + l, cost: word_cost(h.freq), val: h.val });
            }
        }

        // 简拼：每个音节只打首字母，"nh" -> 你好/南海
        for l in 2..=rem.min(6) {
            let Some(sub) = self.input.get(i..i + l) else { continue };
            dict::initials_into(sub, HITS, self.variant, &mut self.hits);
            for h in &self.hits {
                self.edges.push(Edge {
                    to: i + l,
                    cost: word_cost(h.freq) + INITIALS_PENALTY,
                    val: h.val,
                });
            }
        }

        // 尾部音节没输完：用词条补全，代价随缺字数量增加
        if rem > 0 {
            let sub = &self.input[i..];
            dict::prefix_into(sub, 3, HITS, self.variant, &mut self.hits);
            for h in &self.hits {
                let extra = (h.key_len - rem) as f32;
                self.edges.push(Edge {
                    to: len,
                    cost: word_cost(h.freq) + PARTIAL_BASE + PARTIAL_SLOPE * (extra - 1.0),
                    val: h.val,
                });
            }
        }

        if self.edges.len() > EDGES {
            self.edges.truncate(EDGES);
        }
    }
}

fn word_cost(freq: u32) -> f32 {
    LOG_TOTAL - ((freq as f32) + 1.0).log10()
}

/// 保持按代价升序、文本去重、最多 BEST 条。
///
/// 先按代价剪枝再拼接文本：绝大多数 (路径, 边) 组合进不了前 6 名，
/// 省掉这些拼接是热路径上最大的一笔开销。
fn push_path(list: &mut Vec<(f32, String)>, a: &str, b: &str, cost: f32) {
    if list.len() >= BEST && cost >= list[list.len() - 1].0 {
        return;
    }
    let mut text = String::with_capacity(a.len() + b.len());
    text.push_str(a);
    text.push_str(b);
    if list.iter().any(|(_, t)| t == &text) {
        return;
    }
    if list.len() >= BEST {
        list.pop();
    }
    let idx = list.iter().position(|(c, _)| cost < *c).unwrap_or(list.len());
    list.insert(idx, (cost, text));
}

/// 解析 "u4f60" 形式的码位直输
fn parse_codepoint(input: &str) -> Option<(u32, String)> {
    let b = input.as_bytes();
    if b.len() < 5 || b[0] != b'u' {
        return None;
    }
    let mut cp: u32 = 0;
    for &c in &b[1..] {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        cp = cp * 16 + v as u32;
    }
    if cp == 0 || cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
        return None;
    }
    char::from_u32(cp).map(|ch| (cp, ch.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zh::Variant;

    fn all(input: &str) -> Vec<String> {
        crate::dict::init();
        let mut e = Engine::new();
        for c in input.chars() {
            e.push_char(c);
        }
        e.cands.clone()
    }

    fn top(input: &str) -> String {
        all(input).into_iter().next().unwrap_or_default()
    }

    #[test]
    fn 常用词() {
        for (input, want) in [
            ("nihao", "你好"),
            ("nihaoma", "你好吗"),
            ("zhongguo", "中国"),
            ("shurufa", "输入法"),
            ("zaijian", "再见"),
            ("xiexie", "谢谢"),
            ("mingtian", "明天"),
        ] {
            assert_eq!(top(input), want, "输入 {input}");
        }
    }

    #[test]
    fn 整句解码() {
        assert_eq!(top("woshiyigexuesheng"), "我是一个学生");
        assert_eq!(top("jintiantianqihenhao"), "今天天气很好");
        assert_eq!(top("zhonghuarenmingongheguo"), "中华人民共和国");
    }

    #[test]
    fn 简拼() {
        assert_eq!(top("nh"), "你好");
        assert_eq!(top("zg"), "这个");
    }

    #[test]
    fn 尾部补全() {
        assert_eq!(top("nih"), "你好");
        assert_eq!(top("wom"), "我们");
    }

    #[test]
    fn 多音字() {
        assert_eq!(top("yinhang"), "银行");
        assert_eq!(top("xingzou"), "行走");
        assert_eq!(top("chongqing"), "重庆");
        assert_eq!(top("zhongdian"), "重点");
    }

    #[test]
    fn ü写作v() {
        assert_eq!(top("lvxing"), "旅行");
        assert_eq!(top("nvhai"), "女孩");
    }

    #[test]
    fn 码位直输() {
        assert_eq!(top("u4f60"), "你");
        assert_eq!(top("u597d"), "好");
    }

    #[test]
    fn 退格() {
        crate::dict::init();
        let mut e = Engine::new();
        for c in "nihao".chars() {
            e.push_char(c);
        }
        assert!(e.active());
        assert_eq!(e.preedit, "nihao");
        assert!(e.backspace());
        assert_eq!(e.preedit, "niha");
        for _ in 0..4 {
            e.backspace();
        }
        assert!(!e.active());
        assert!(e.cands.is_empty());
        assert!(!e.backspace(), "空串再退格应返回 false");
    }

    #[test]
    fn 候选不重复() {
        let c = all("nihao");
        let mut d = c.clone();
        d.sort();
        d.dedup();
        assert_eq!(c.len(), d.len(), "候选出现重复项");
    }

    #[test]
    fn 输入长度上限() {
        crate::dict::init();
        let mut e = Engine::new();
        for _ in 0..100 {
            e.push_char('n');
        }
        assert!(e.input.len() <= MAX_INPUT, "输入缓冲溢出上限");
    }

    #[test]
    fn 重置清空状态() {
        crate::dict::init();
        let mut e = Engine::new();
        for c in "nihao".chars() {
            e.push_char(c);
        }
        e.reset();
        assert!(!e.active());
        assert!(e.cands.is_empty());
        assert!(e.preedit.is_empty());
    }

    #[test]
    fn 英文与数字不进组字() {
        crate::dict::init();
        let mut e = Engine::new();
        // 大写字母由调用方转小写后再进引擎，引擎本身只接受已确定的字符
        e.push_char('n');
        assert_eq!(e.preedit, "n");
        assert!(e.cands.len() <= CAND_MAX);
    }

    #[test]
    fn 内联候选包含编号() {
        crate::dict::init();
        let mut e = Engine::new();
        for c in "nihao".chars() {
            e.push_char(c);
        }
        let inl = e.inline_preedit();
        assert!(inl.starts_with("nihao"), "内联候选应以拼音开头: {inl}");
        assert!(inl.contains("1."), "内联候选应带编号: {inl}");
    }

    /// 简体模式下候选里不许出现繁体字：词库源数据里 `變成`/`变成` 混着存，
    /// 不过滤就会简繁混排（而且繁体那条词频更高，会排到第一）。
    #[test]
    fn 简体模式不混入繁体() {
        crate::dict::init();
        let hant = "變學國說後髮幹麵裡臺灣龍鳳愛寶藏";
        for input in ["biancheng", "toufa", "ganjing", "houlai", "taiwan", "xuexi", "shuofa"] {
            let c = all(input);
            assert!(!c.is_empty(), "{input} 无候选");
            for cand in &c {
                assert!(
                    !cand.chars().any(|ch| hant.contains(ch)),
                    "{input} 的候选 {cand} 混入了繁体字"
                );
            }
        }
    }

    #[test]
    fn 繁体模式按词转换() {
        crate::dict::init();
        let mut e = Engine::new();
        e.set_variant(Variant::Hant);
        for ch in "toufa".chars() {
            e.push_char(ch);
        }
        assert_eq!(e.cands[0], "頭髮", "词级例外优先于逐字: {:?}", e.cands);
        // 繁体模式下候选不得重复（变成 / 變成 会同时命中，转换后必须去重）
        let mut d = e.cands.clone();
        d.sort();
        d.dedup();
        assert_eq!(e.cands.len(), d.len(), "繁体候选出现重复: {:?}", e.cands);
    }

    #[test]
    fn 切换形态即时重算() {
        crate::dict::init();
        let mut e = Engine::new();
        for ch in "biancheng".chars() {
            e.push_char(ch);
        }
        assert_eq!(e.cands[0], "变成");
        e.set_variant(Variant::Hant);
        assert_eq!(e.cands[0], "變成", "切换后应立即按新形态重算");
        e.set_variant(Variant::Hans);
        assert_eq!(e.cands[0], "变成");
    }
}
