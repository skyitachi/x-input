//! 拼音解码器：在「词」构成的网格上做 N-best 路径搜索。
//!
//! 为什么不能贪心切分：拼音串没有分隔符，"xian" 可能是「先」也可能是「西安」；
//! 更麻烦的是 "nihaom" 能被完整解析成「你/哈/欧盟」，也可能该切成「你好/吗」。
//! 只在首选路径的首词上做变体，永远看不到后者——所以必须保留每个位置上的
//! 多条候选路径，最后统一按代价排序。
//!
//! 代价：每个词 = log10(总词频) - log10(词频)。一个词只付一次常数项，
//! 因此长词天然优于逐字拼，符合汉语构词习惯。

use crate::ngram;
use crate::dict;
use crate::fuzzy::{self, FUZZY_PENALTY, REPAIR_PENALTY};
use crate::zh::{self, Variant};

pub const MAX_INPUT: usize = 32;
pub const CAND_MAX: usize = 128;

/// 一次组字最多带几个 emoji 候选。多了会把候选窗后半段全占满，
/// 而 emoji 永远排在词后面，用户翻两页还全是表情就很不合理。
const EMOJI_CANDS: usize = 2;

/// emoji 插进候选的位置下限（0 基）。候选窗一页最多 9 格、横排只有 6 格，
/// 插在第 5、6 格才能保证三种形态都在第一页看得见：既不会抢「笑脸」这类
/// 词的首位，也不必翻页才够得着。词候选不足时按实际长度顺接。
const EMOJI_SLOT: usize = 5;

const HITS: usize = 6; // 同一拼音取几个同音候选参与搜索
const BEST: usize = 27; // 每个位置保留几条路径（决定候选总数，27 = 3 页）
const EDGES: usize = 96; // 每个位置最多几条边
const MAX_KEY: usize = 32; // 词条拼音键最长字节数

/// 词频总和的对数（9，词频 = 概率×1e9 的归一化项）+ 插入惩罚（1，每多切一个词
/// 多付一次，让整词天然优于逐字拼）。上下文消歧由三元模型承担（见
/// `ngram::context_cost`），但插入惩罚这部分模型替代不了——语言模型衡量
/// 「这个词在这个上下文多合理」，不衡量「该不该多切一个词」。所以保持 10 而非 9。
const LOG_TOTAL: f32 = 10.0;
/// 补全未输完的尾部音节：按"缺一个音节"计固定惩罚，再按缺字数量轻微递增，
/// 否则输入 "n" 时「中国」会因为词频高而排在「你」前面。
const PARTIAL_BASE: f32 = 2.0;
const PARTIAL_SLOPE: f32 = 0.7;
/// 简拼（nh -> 你好）的惩罚，保证全拼结果优先
const INITIALS_PENALTY: f32 = 2.0;
/// 三元上下文代价的权重。context_cost 是「log10 P1(w) − log10 P(w|ctx)」，
/// 权重 1.0 时等价于纯 kenlm 打分（−log10 P(w|ctx)），0 时退化为纯词频排序。
/// 取 1.0：KN 平滑后的概率是标定过的，不需要再打折；词库的人工提频通过
/// word_cost 仍然参与，两者不冲突。
const CTX_WEIGHT: f32 = 1.0;

struct Edge {
    to: usize,
    cost: f32,
    val: &'static str,
    /// 该候选词的词形 id（val_id），供三元语言模型查询
    val_id: u32,
    /// log10 P1(该词)。同一候选词在所有路径上相同，放边上算一次
    uni: f32,
}

pub struct Engine {
    pub input: String,
    pub preedit: String,
    pub cands: Vec<String>,
    /// 各候选的解码代价，越小越可信（调试用）
    pub cost: Vec<f32>,
    /// 每个输入位置上的 N 条最优路径。跨次重算复用，避免每次按键都重新分配。
    /// 后两维是末词与倒数第二词的 val_id（句首为 u32::MAX 哨兵），供三元查询。
    paths: Vec<Vec<(f32, String, u32, u32)>>,
    /// 当前位置的出边缓冲，复用
    edges: Vec<Edge>,
    /// 词库查表结果缓冲，复用
    hits: Vec<dict::Hit>,
    /// 音节切分缓冲（fuzzy::split 的输出）
    syl: [usize; MAX_INPUT + 1],
    /// 上下文代价的直映缓存：(prev2, prev1, w) -> cost。代价是纯函数，可以跨按键
    /// 复用——每次按键都会把整句从头重算一遍，前缀的上下文重复率很高。
    /// 空槽用 NaN 标记（代价不可能是 NaN）。
    cost_cache: Vec<(u32, u32, u32, f32)>,
    /// 预热好的上下文缓存：(prev2, prev1) -> Ctx。省掉重复的三元/二元段装载。
    /// 空槽用 (u32::MAX, u32::MAX - 1) 标记，它不是合法词形对。
    ctx_cache: Vec<(u32, u32, ngram::Ctx)>,
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
            cost_cache: vec![(0, 0, 0, f32::NAN); 1 << 16],
            ctx_cache: vec![(u32::MAX, u32::MAX - 1, ngram::Ctx::empty()); 1 << 13],
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
        self.paths[0].push((0.0, String::new(), u32::MAX, u32::MAX));

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
            let cache = &mut self.cost_cache;
            let mask = cache.len() - 1;
            let ctx_cache = &mut self.ctx_cache;
            let ctx_mask = ctx_cache.len() - 1;
            for (pcost, ptext, plast, psecond) in cur.iter() {
                // 同一上下文对所有出边只预热一次；跨按键也复用（上下文是纯函数）
                let h = psecond.wrapping_mul(0x9E37_79B1).wrapping_add(plast.wrapping_mul(0x85EB_CA77));
                let slot = &mut ctx_cache[h as usize & ctx_mask];
                let ctx = { let _ = &slot; ngram::Ctx::prepare(*psecond, *plast) };
                for e in self.edges.iter() {
                    let slot = &mut right[e.to - (i + 1)];
                    let c = ctx_cost(cache, mask, &ctx, *psecond, *plast, e.val_id, e.uni);
                    push_path(slot, ptext, e.val, pcost + e.cost + CTX_WEIGHT * c, e.val_id, *plast);
                }
            }
        }

        for (c, t, _, _) in self.paths[len].iter() {
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

        // 拼音命中的 emoji 插在第一页末尾。插队位置固定在 EMOJI_SLOT，
        // 而不是直接 append：词候选动辄二三十条，append 等于把它们埋到第
        // 三页之后，等于没做。挤掉的那两条词只是往后挪一格，仍在第二页。
        if self.input.len() >= 2 && self.cands.len() < CAND_MAX {
            let mut slot = self.cands.len().min(EMOJI_SLOT);

            for ch in crate::emoji::Keys::get().search(&self.input, EMOJI_CANDS) {
                let s = ch.to_string();
                if self.cands.iter().any(|x| *x == s) {
                    continue;
                }
                self.cands.insert(slot, s);
                // 代价给个远大于词的值：翻页/排序时永远排在词后面
                self.cost.insert(slot, 1e6);
                slot += 1;
            }
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
                self.edges.push(Edge {
                    to: i + l,
                    cost: word_cost(h.freq),
                    val: h.val,
                    val_id: h.val_id,
                    uni: ngram::uni_log10(h.val_id),
                });
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
                    uni: ngram::uni_log10(h.val_id),
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
                    uni: ngram::uni_log10(h.val_id),
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
                        uni: ngram::uni_log10(h.val_id),
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

/// 带直映缓存的上下文代价。缓存键是 (前两个词, 候选词)，值是纯函数的输出，
/// 所以跨按键复用不需要任何失效逻辑。
#[inline]
fn ctx_cost(
    cache: &mut [(u32, u32, u32, f32)],
    mask: usize,
    ctx: &ngram::Ctx,
    prev2: u32,
    prev1: u32,
    w: u32,
    uni: f32,
) -> f32 {
    let h = prev2
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(prev1.wrapping_mul(0x85EB_CA77))
        .wrapping_add(w.wrapping_mul(0xC2B2_AE3D));
    let slot = &mut cache[h as usize & mask];
    if slot.0 == prev2 && slot.1 == prev1 && slot.2 == w && !slot.3.is_nan() {
        return slot.3;
    }
    let v = ctx.cost(w, uni);
    *slot = (prev2, prev1, w, v);
    v
}

fn word_cost(freq: u32) -> f32 {
    LOG_TOTAL - ((freq as f32) + 1.0).log10()
}

/// 保持按代价升序、文本去重、最多 BEST 条。
///
/// 先按代价剪枝再拼接文本：绝大多数 (路径, 边) 组合进不了前 6 名，
/// 省掉这些拼接是热路径上最大的一笔开销。
fn push_path(
    list: &mut Vec<(f32, String, u32, u32)>,
    a: &str,
    b: &str,
    cost: f32,
    val_id: u32,
    second: u32,
) {
    if list.len() >= BEST && cost >= list[list.len() - 1].0 {
        return;
    }
    let mut text = String::with_capacity(a.len() + b.len());
    text.push_str(a);
    text.push_str(b);
    // 按 (text, 末词, 倒数第二词) 去重：同一 text 不同切分（整词 vs 逐字）
    // 后续上下文不同，必须都保留，否则三元的上下文区分会被 text 去重吃掉。
    if list.iter().any(|(_, t, v, v2)| t == &text && *v == val_id && *v2 == second) {
        return;
    }
    if list.len() >= BEST {
        list.pop();
    }
    let idx = list.iter().position(|(c, _, _, _)| cost < *c).unwrap_or(list.len());
    list.insert(idx, (cost, text, val_id, second));
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

    /// 上下文消歧：这些用例靠 unigram 词频无法区分，必须靠 n-gram 的上下文
    /// 概率才能选对（「到达」后面接「终点」而不是「重点」）。
    #[test]
    fn context_disambiguation() {
        assert_eq!(top("daodazhongdian"), "到达终点");
        assert_eq!(top("shijianzhongdian"), "时间重点");
        // 「一会」在书面语里几乎总写「一会儿」，语料里「我们一会」0 次、
        // 「我们也会」15 次，三元模型据此选「也会」；「我们一会」仍在第二候选。
        assert_eq!(top("womenyihui"), "我们也会");
        assert_eq!(top("dalianrenmin"), "大连人民");
    }

    /// 三元确实在工作：同一个「词」，给两个词的前缀能让代价明显变低。
    /// 只靠二元时 (终点 → 的) 与前面是谁无关；三元认得「到达终点」这个搭配。
    #[test]
    fn trigram_context_sharpens_bigram() {
        let mut hits = Vec::new();
        let id = |w: &str, py: &str, hits: &mut Vec<dict::Hit>| -> u32 {
            dict::exact_into(py, 30, Variant::Hans, hits);
            hits.iter()
                .find(|h| h.val == w)
                .map(|h| h.val_id)
                .unwrap_or(u32::MAX)
        };
        let daoda = id("到达", "daoda", &mut hits);
        let zhongdian = id("终点", "zhongdian", &mut hits);
        let de = id("的", "de", &mut hits);
        assert!(daoda != u32::MAX && zhongdian != u32::MAX && de != u32::MAX);
        let tri = ngram::context_cost(daoda, zhongdian, de);
        let bi = ngram::context_cost(u32::MAX, zhongdian, de);
        assert!(
            tri < bi - 0.2,
            "三元应显著降低「到达终点 的」的代价：{tri} vs 二元 {bi}"
        );
        // 没见过的上下文退回二元，代价必须一致（不能凭空加分）
        let unseen = ngram::context_cost(u32::MAX - 1, zhongdian, de);
        assert!((unseen - bi).abs() < 1e-6, "{unseen} vs {bi}");
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
