//! 拼音解码器：在「词」构成的网格上做 N-best 路径搜索。
//!
//! 为什么不能贪心切分：拼音串没有分隔符，"xian" 可能是「先」也可能是「西安」；
//! 更麻烦的是 "nihaom" 能被完整解析成「你/哈/欧盟」，也可能该切成「你好/吗」。
//! 只在首选路径的首词上做变体，永远看不到后者——所以必须保留每个位置上的
//! 多条候选路径，最后统一按代价排序。
//!
//! 代价：每个词 = log10(总词频) - log10(词频)。一个词只付一次常数项，
//! 因此长词天然优于逐字拼，符合汉语构词习惯。

use crate::bigram;
use crate::dict;
use crate::fuzzy::{self, FUZZY_PENALTY, REPAIR_PENALTY};
use crate::zh::{self, Variant};

pub const MAX_INPUT: usize = 32;
pub const CAND_MAX: usize = 128;

const HITS: usize = 6; // 同一拼音取几个同音候选参与搜索
const BEST: usize = 27; // 每个位置保留几条路径（决定候选总数，27 = 3 页）
const EDGES: usize = 96; // 每个位置最多几条边
const MAX_KEY: usize = 32; // 词条拼音键最长字节数

/// 词频总和的对数（9，词频 = 概率×1e9 的归一化项）+ 插入惩罚（1，每多切一个词
/// 多付一次，让整词天然优于逐字拼）。上下文消歧改由 bigram 转移项承担
/// （见 `bigram::trans_cost`），但插入惩罚这部分 bigram 替代不了——PMI 只衡量
/// 「搭配是否更常见」，不衡量「该不该多切一个词」。所以保持 10 而非降到 9。
const LOG_TOTAL: f32 = 10.0;
/// 补全未输完的尾部音节：按"缺一个音节"计固定惩罚，再按缺字数量轻微递增，
/// 否则输入 "n" 时「中国」会因为词频高而排在「你」前面。
const PARTIAL_BASE: f32 = 2.0;
const PARTIAL_SLOPE: f32 = 0.7;
/// 简拼（nh -> 你好）的惩罚，保证全拼结果优先
const INITIALS_PENALTY: f32 = 2.0;
/// bigram 转移代价的权重。PMI 对低频词（如「妳」，10 万次）会因统计稀疏而给出
/// 过大的奖励，压过 word_cost 的整词偏好；乘以 <1 的系数压低其影响，让 unigram
/// 主导排序、bigram 只做微调。
const TRANS_WEIGHT: f32 = 0.4;

struct Edge {
    to: usize,
    cost: f32,
    val: &'static str,
    /// 该候选词的词形 id（val_id），供 bigram 转移代价查询
    val_id: u32,
}

pub struct Engine {
    pub input: String,
    pub preedit: String,
    pub cands: Vec<String>,
    /// 各候选的解码代价，越小越可信（调试用）
    pub cost: Vec<f32>,
    /// 每个输入位置上的 N 条最优路径。跨次重算复用，避免每次按键都重新分配。
    /// 第三维是路径末词的 val_id（句首为 u32::MAX 哨兵），供 bigram 转移代价查询。
    paths: Vec<Vec<(f32, String, u32)>>,
    /// 当前位置的出边缓冲，复用
    edges: Vec<Edge>,
    /// 词库查表结果缓冲，复用
    hits: Vec<dict::Hit>,
    /// 音节切分缓冲（fuzzy::split 的输出）
    syl: [usize; MAX_INPUT + 1],
    /// 模糊变体拼接缓冲，两级（音节级 / 整串级），复用避免热路径分配
    fbuf: String,
    fbuf2: String,
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
            syl: [0; MAX_INPUT + 1],
            fbuf: String::with_capacity(MAX_INPUT),
            fbuf2: String::with_capacity(MAX_INPUT),
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
        self.paths[0].push((0.0, String::new(), u32::MAX));

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
            for (pcost, ptext, plast) in cur.iter() {
                for e in self.edges.iter() {
                    let slot = &mut right[e.to - (i + 1)];
                    push_path(
                        slot,
                        ptext,
                        e.val,
                        pcost + e.cost + TRANS_WEIGHT * bigram::trans_cost(*plast, e.val_id),
                        e.val_id,
                    );
                }
            }
        }

        for (c, t, _) in self.paths[len].iter() {
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
                self.edges.push(Edge { to: i + l, cost: word_cost(h.freq), val: h.val, val_id: h.val_id });
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
                    val_id: h.val_id,
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
                    val_id: h.val_id,
                });
            }
        }

        // 近似匹配：模糊音（sijian -> shijian）与拼写错（nihap -> nihao）。
        //
        // 只在两种情况下动：剩余串能完整切成合法音节（用户打完了，但可能打错），
        // 或者坏尾确实不是任何音节的前缀（用户不是「还在打字」，是真打错了）。
        // 所以正常输入——停在完整音节 `ni` 或半截音节 `zhon`——一行都不多跑。
        // 命中的词带 FUZZY_PENALTY，精确匹配永远排在前面。
        if rem >= 2 {
            let sub = &self.input[i..];
            let variant = self.variant;
            let mut syl = std::mem::replace(&mut self.syl, [0usize; MAX_INPUT + 1]);
            let sp = fuzzy::split(sub, &mut syl);
            let mut hits = std::mem::take(&mut self.hits);
            let mut fbuf = std::mem::take(&mut self.fbuf);
            let mut fbuf2 = std::mem::take(&mut self.fbuf2);
            let edges = &mut self.edges;
            // 模糊音是「口音」，修复是「猜」，两者惩罚不同（见 fuzzy.rs 的常量）。
            let pen = if sp.bad.is_none() { FUZZY_PENALTY } else { REPAIR_PENALTY };
            // 修复后的串长度可能与输入不同（漏打/多打一个字母），但都消费掉
            // 整个剩余输入，所以 to 恒为 len，不能按 v.len() 算。
            let mut push = |v: &str| {
                dict::exact_into(v, HITS, variant, &mut hits);
                for h in &hits {
                    edges.push(Edge {
                        to: len,
                        cost: word_cost(h.freq) + pen,
                        val: h.val,
                        val_id: h.val_id,
                    });
                }
            };
            match sp.bad {
                // 整串都是合法音节：每个音节试一次等长的声母/韵母替换
                None => fuzzy::for_each_variant(sub, &syl, sp.n, &mut fbuf, &mut fbuf2, &mut push),
                // 坏尾不像任何音节：整段当拼写错去修
                Some(bad) if !fuzzy::is_syllable_prefix(&sub[bad..]) => {
                    fuzzy::for_each_repair(sub, bad, &mut fbuf, &mut fbuf2, &mut push)
                }
                // 坏尾是音节前缀（像是还在打），但并回上一音节后就不像了：
                // 说明是上一音节打错，而不是正在输入（nihap -> ni|ha|p -> nihao）
                Some(_) if sp.n > 0 => {
                    fuzzy::for_each_tail_repair(sub, &syl, sp.n, &mut fbuf, &mut fbuf2, &mut push)
                }
                _ => {}
            }
            self.syl = syl;
            self.hits = hits;
            self.fbuf = fbuf;
            self.fbuf2 = fbuf2;
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
fn push_path(list: &mut Vec<(f32, String, u32)>, a: &str, b: &str, cost: f32, val_id: u32) {
    if list.len() >= BEST && cost >= list[list.len() - 1].0 {
        return;
    }
    let mut text = String::with_capacity(a.len() + b.len());
    text.push_str(a);
    text.push_str(b);
    // 按 (text, 末词 val_id) 去重：同一 text 不同末词（整词 vs 逐字切分）
    // 后续转移不同，必须都保留，否则 bigram 的上下文区分会被 text 去重吃掉。
    if list.iter().any(|(_, t, v)| t == &text && *v == val_id) {
        return;
    }
    if list.len() >= BEST {
        list.pop();
    }
    let idx = list.iter().position(|(c, _, _)| cost < *c).unwrap_or(list.len());
    list.insert(idx, (cost, text, val_id));
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
    fn common_words() {
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
    fn sentence_decode() {
        assert_eq!(top("woshiyigexuesheng"), "我是一个学生");
        assert_eq!(top("jintiantianqihenhao"), "今天天气很好");
        assert_eq!(top("zhonghuarenmingongheguo"), "中华人民共和国");
    }

    /// bigram 的上下文消歧：这些用例靠 unigram 词频无法区分，
    /// 必须靠「前词 → 当前词」的转移概率才能选对。
    #[test]
    fn context_disambiguation() {
        assert_eq!(top("daodazhongdian"), "到达终点");
        assert_eq!(top("shijianzhongdian"), "时间重点");
        assert_eq!(top("womenyihui"), "我们一会");
        assert_eq!(top("dalianrenmin"), "大连人民");
    }

    #[test]
    fn abbreviated_spelling() {
        assert_eq!(top("nh"), "你好");
        assert_eq!(top("zg"), "这个");
    }

    /// 现代词与人工词表：这些词的拼音读法在读音表里有，但语料停在 2014 年、
    /// 计数只有几十，曾被同音的旧噪声词压住（内眷/汤瓶/甲板/极小）。
    /// 词表在 wayland-ime/tools/extra-words.txt，改动后要重新生成 dict.bin。
    #[test]
    fn modern_vocabulary() {
        for (input, want) in [
            ("neijuan", "内卷"),
            ("tangping", "躺平"),
            ("chongdianbao", "充电宝"),
            ("yanzhengma", "验证码"),
            ("jiaban", "加班"),
            ("tiaoxiu", "调休"),
            ("kongtiao", "空调"),
            ("moyu", "摸鱼"),
            ("jixiao", "绩效"),
        ] {
            assert_eq!(top(input), want, "输入 {input}");
        }
    }

    /// 模糊音与拼写错：打错了也能选出相近的。
    ///
    /// 惩罚来自 fuzzy.rs（模糊音 2.0、拼写修复 4.0），所以精确匹配永远排前面——
    /// 这些用例断言的只是「首选落到对的词上」，不是「模糊结果插到最前」。
    #[test]
    fn fuzzy_and_typo_tolerance() {
        for (input, want) in [
            ("zongguo", "中国"),   // zh -> z
            ("sijian", "时间"),    // sh -> s
            ("lanfang", "南方"),   // n -> l
            ("chengjing", "曾经"), // ch -> c
            ("nihap", "你好"),     // 尾键打错
        ] {
            assert_eq!(top(input), want, "输入 {input}");
        }
        // 精确输入不受影响：模糊匹配只是追加带惩罚的边
        for (input, want) in [("zhongguo", "中国"), ("shijian", "时间"), ("nanfang", "南方")] {
            assert_eq!(top(input), want, "精确输入 {input}");
        }
    }

    /// 金融与计算机常用词。这两类专业词汇在 2014 年语料里几乎没有计数，
    /// 同音键上全是噪声（通掌/问当/连表/条式），以前打不出来或排在看不见的位置。
    /// 词表见 extra-words.txt 的「金融 / 投资」「计算机 / 研发」两段。
    #[test]
    fn domain_vocabulary() {
        for (input, want) in [
            ("gupiao", "股票"),
            ("zhangting", "涨停"),
            ("tongzhang", "通胀"),
            ("zhisun", "止损"),
            ("xiongshi", "熊市"),
            ("caibao", "财报"),
            ("wendang", "文档"),
            ("damoxing", "大模型"),
            ("tishici", "提示词"),
            ("weifuwu", "微服务"),
            ("haxibiao", "哈希表"),
        ] {
            assert_eq!(top(input), want, "输入 {input}");
        }
    }

    /// 「重写」「重载」在词组读音表里缺条目（重载的条目读 zhòng zài，是「重载列车」
    /// 那个意思），自动推导按「重」的单字首选音拼，用户打 chongxie 出不来。
    /// 人工词表补读音后这两个键才算通。
    ///
    /// 注意别断言 `zhongxie` 下没有「重写」——逐字合成路径本来就会给出「重(zhòng)
    /// + 写(xiě)」这个组合，那是字符级输入的正常结果，不是残留的错条目。
    #[test]
    fn polyphonic_word_reading_override() {
        assert_eq!(top("chongxie"), "重写");
        assert_eq!(top("chongzai"), "重载");
    }

    /// 单音节键的字数多样性：`yue` 既是单字（月/越/约）也是双字词（余额）。
    /// 纯按词频取前 N 条时单字会把键占满，「余额」永远进不了候选——这正是
    /// 用户看到的「基本词没有」。mkdict.py 的 MAX_LEN_CLASSES 负责保证这条。
    #[test]
    fn single_syllable_key_keeps_words() {
        let c = all("yue");
        assert!(c.iter().any(|x| x == "余额"), "单音节键被单字占满: {c:?}");
        assert_eq!(c[0], "月");
    }

    #[test]
    fn tail_completion() {
        assert_eq!(top("nih"), "你好");
        assert_eq!(top("wom"), "我们");
    }

    #[test]
    fn polyphonic_chars() {
        assert_eq!(top("yinhang"), "银行");
        assert_eq!(top("xingzou"), "行走");
        assert_eq!(top("chongqing"), "重庆");
        assert_eq!(top("zhongdian"), "重点");
    }

    #[test]
    fn umlaut_written_as_v() {
        assert_eq!(top("lvxing"), "旅行");
        assert_eq!(top("nvhai"), "女孩");
    }

    #[test]
    fn codepoint_direct_input() {
        assert_eq!(top("u4f60"), "你");
        assert_eq!(top("u597d"), "好");
    }

    #[test]
    fn backspace() {
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
    fn candidates_unique() {
        let c = all("nihao");
        let mut d = c.clone();
        d.sort();
        d.dedup();
        assert_eq!(c.len(), d.len(), "候选出现重复项");
    }

    #[test]
    fn input_length_limit() {
        crate::dict::init();
        let mut e = Engine::new();
        for _ in 0..100 {
            e.push_char('n');
        }
        assert!(e.input.len() <= MAX_INPUT, "输入缓冲溢出上限");
    }

    #[test]
    fn reset_clears_state() {
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
    fn ascii_not_composed() {
        crate::dict::init();
        let mut e = Engine::new();
        // 大写字母由调用方转小写后再进引擎，引擎本身只接受已确定的字符
        e.push_char('n');
        assert_eq!(e.preedit, "n");
        assert!(e.cands.len() <= CAND_MAX);
    }

    #[test]
    fn inline_candidates_numbered() {
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
    fn simplified_no_traditional() {
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
    fn traditional_word_level_conversion() {
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
    fn variant_switch_recomputes() {
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
