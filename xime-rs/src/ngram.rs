//! 三元语言模型（modified Kneser-Ney）：`log10 P(w | a, b)` + ARPA 式回退链。
//!
//! 数据由 `tools/mkngram.rs` 生成（`data/ngram.bin`），语义与 kenlm/ARPA 一致：
//!
//! ```text
//! P(w|a,b) = P3(w|a,b)                      三元命中
//!          = bow(a,b) · P(w|b)              三元未命中（bow 存在 2-gram 行上）
//! P(w|b)   = P2(w|b)                        二元命中
//!          = bow(b) · P1(w)                 二元未命中（bow 存在 1-gram 行上）
//! ```
//!
//! 二进制布局（全部小端，无对齐要求，直接按字节读）：
//!
//! ```text
//! 0    magic "XIMENG01"
//! 8    u32 order = 3
//! 12   u32 词形数 V
//! 16   u32 二元条目数
//! 20   u32 三元条目数
//! 24   8 字节 0
//! 32   i16[V]      log10 P1(w)
//! 32+2V i16[V]     log10 bow(w)
//! 32+4V u32[V+1]   每个前词的二元条目区间
//! ...  u8[n_bg*8]  二元条目 (next u32, log10 P2 i16, log10 bow i16)
//! ...  u32[n_bg+1] 每个二元上下文的三元条目区间
//! ...  u8[n_tg*6]  三元条目 (next u32, log10 P3 i16)
//! ```
//!
//! 概率存 i16 定点（1e-3 log10），与 `mkngram` 的 ARPA 输出差在 ±0.0005 以内。
//! 文件缺失/被截断时整体退化为零代价，不影响输入法其他功能。

use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../data/ngram.bin");

const MAGIC: &[u8; 8] = b"XIMENG01";
const BG_ENTRY: usize = 8;
const TG_ENTRY: usize = 6;

/// 句首/用户词表等「没有上下文」的哨兵；查表时当作无上下文处理
pub const SENTINEL: u32 = u32::MAX;

#[inline]
fn rd16(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// log10 定点的换算：存的是 1e-3 单位
#[inline]
fn val(b: &[u8], off: usize) -> f32 {
    rd16(b, off) as f32 / 1000.0
}

struct Model {
    v: u32,
    /// 二元/三元条目数。热路径用区间表定位，不需要这两个数；留着给自检与测试断言。
    #[allow(dead_code)]
    n_bg: u32,
    #[allow(dead_code)]
    n_tg: u32,
    /// 各段的字节起点
    uni_p: usize,
    uni_bow: usize,
    bg_off: usize,
    bg: usize,
    tg_off: usize,
    tg: usize,
}

fn empty() -> Model {
    Model { v: 0, n_bg: 0, n_tg: 0, uni_p: 0, uni_bow: 0, bg_off: 0, bg: 0, tg_off: 0, tg: 0 }
}

fn data() -> &'static Model {
    static M: OnceLock<Model> = OnceLock::new();
    M.get_or_init(|| {
        if RAW.len() < 32 || &RAW[0..8] != MAGIC || rd32(RAW, 8) != 3 {
            return empty();
        }
        let v = rd32(RAW, 12) as usize;
        let n_bg = rd32(RAW, 16) as usize;
        let n_tg = rd32(RAW, 20) as usize;
        let uni_p = 32;
        let uni_bow = uni_p + v * 2;
        let bg_off = uni_bow + v * 2;
        let bg = bg_off + (v + 1) * 4;
        let tg_off = bg + n_bg * BG_ENTRY;
        let tg = tg_off + (n_bg + 1) * 4;
        // 截断/溢出防御：文件不完整时整体退化
        let end = tg.checked_add(n_tg * TG_ENTRY).unwrap_or(usize::MAX);
        if end > RAW.len() {
            return empty();
        }
        Model {
            v: v as u32,
            n_bg: n_bg as u32,
            n_tg: n_tg as u32,
            uni_p,
            uni_bow,
            bg_off,
            bg,
            tg_off,
            tg,
        }
    })
}

/// 1-gram 日志概率 log10 P(w)。越界返回一个很小的值而不是 0（0 表示概率 1）。
#[inline]
pub fn uni_log10(w: u32) -> f32 {
    let m = data();
    if w >= m.v || m.v == 0 {
        return -9.0;
    }
    val(RAW, m.uni_p + w as usize * 2)
}

/// 1-gram 回退权重 log10 bow(w)
#[inline]
fn uni_bow_log10(w: u32) -> f32 {
    let m = data();
    if w >= m.v || m.v == 0 {
        return 0.0;
    }
    val(RAW, m.uni_bow + w as usize * 2)
}

/// 二元条目下标区间
#[inline]
fn bg_range(m: &Model, a: u32) -> (usize, usize) {
    (
        rd32(RAW, m.bg_off + a as usize * 4) as usize,
        rd32(RAW, m.bg_off + (a as usize + 1) * 4) as usize,
    )
}

/// 查二元条目，返回下标（组内 next 升序，二分）
#[inline]
fn find_bg(m: &Model, a: u32, b: u32) -> Option<usize> {
    if a >= m.v {
        return None;
    }
    let (lo, hi) = bg_range(m, a);
    let (mut l, mut r) = (lo, hi);
    while l < r {
        let mid = (l + r) / 2;
        if rd32(RAW, m.bg + mid * BG_ENTRY) < b {
            l = mid + 1;
        } else {
            r = mid;
        }
    }
    if l < hi && rd32(RAW, m.bg + l * BG_ENTRY) == b {
        Some(l)
    } else {
        None
    }
}

#[inline]
fn bg_p(m: &Model, idx: usize) -> f32 {
    val(RAW, m.bg + idx * BG_ENTRY + 4)
}

/// 三元上下文（二元条目）的回退权重 log10 bow(a,b)
#[inline]
fn bg_bow(m: &Model, idx: usize) -> f32 {
    val(RAW, m.bg + idx * BG_ENTRY + 6)
}

/// 查三元条目：上下文为二元条目下标
///
/// 热路径不直接调它——`Ctx::prepare` 会把后继列表装进栈上数组，避免每条候选都二分。
/// 留着是为了保持一份「逐次查询」的参照实现，测试用它验证预热版与朴素版等价。
#[allow(dead_code)]
#[inline]
fn find_tg(m: &Model, bi: usize, w: u32) -> Option<f32> {
    let lo = rd32(RAW, m.tg_off + bi * 4) as usize;
    let hi = rd32(RAW, m.tg_off + (bi + 1) * 4) as usize;
    let (mut l, mut r) = (lo, hi);
    while l < r {
        let mid = (l + r) / 2;
        if rd32(RAW, m.tg + mid * TG_ENTRY) < w {
            l = mid + 1;
        } else {
            r = mid;
        }
    }
    if l < hi && rd32(RAW, m.tg + l * TG_ENTRY) == w {
        Some(val(RAW, m.tg + l * TG_ENTRY + 4))
    } else {
        None
    }
}

/// log10 P(w|b)：命中存储值，否则 bow(b) + log10 P1(w)
///
/// 同上，只由 `log10_p` 这条朴素路径使用（热路径走 `Ctx`）。
#[allow(dead_code)]
#[inline]
fn bg_log10(m: &Model, b: u32, w: u32) -> f32 {
    if b == SENTINEL || b >= m.v {
        return uni_log10(w);
    }
    match find_bg(m, b, w) {
        Some(i) => bg_p(m, i),
        None => uni_bow_log10(b) + uni_log10(w),
    }
}

/// log10 P(w | prev2, prev1)，ARPA 回退链。任一前词是哨兵就少退一阶。
///
/// 单次查询的参照实现。引擎热路径调 `Ctx::prepare` + `Ctx::log10_p`——同一份语义，
/// 但把上下文两段查表提到循环外，候选多时快一个数量级。
#[allow(dead_code)]
#[inline]
pub fn log10_p(prev2: u32, prev1: u32, w: u32) -> f32 {
    let m = data();
    if m.v == 0 {
        return 0.0;
    }
    if prev2 == SENTINEL || prev1 == SENTINEL || prev1 >= m.v {
        return bg_log10(m, prev1, w);
    }
    match find_bg(m, prev2, prev1) {
        Some(bi) => match find_tg(m, bi, w) {
            Some(p) => p,
            None => bg_bow(m, bi) + bg_log10(m, prev1, w),
        },
        None => bg_log10(m, prev1, w),
    }
}

/// 内联缓存的条目数。二元上下文的后继数中位数是 1~3，八个槽位覆盖九成以上；
/// 装不下的退回二分，不影响正确性。
const INLINE: usize = 4;

/// 一个「前两个词」的上下文，预热成栈上结构。
///
/// 为什么要预热：解码时同一个上下文要对十几个候选词各查一次，每次都去 25MB 的
/// 表里随机跳（二元段 + 三元段 + 一元表约七次访存）实测 0.9 μs/次，37 键整句
/// 多出 8 ms。预热只付一次，之后每个候选词在栈上线性扫，热路径不再碰大表。
#[derive(Clone, Copy)]
pub struct Ctx {
    ok: bool,
    /// 三元上下文存在（(prev2, prev1) 是个二元条目）
    has_tg_ctx: bool,
    /// 该上下文的回退权重 log10 bow(prev2, prev1)
    bow: f32,
    tg_lo: usize,
    tg_hi: usize,
    tg_n: u8,
    tg_inline: [(u32, f32); INLINE],
    /// 前一个词（回退目标）的二元段
    bg_lo: usize,
    bg_hi: usize,
    bg_n: u8,
    bg_inline: [(u32, f32); INLINE],
    /// 前一个词的一元回退权重 log10 bow(prev1)
    bg_bow: f32,
}

impl Ctx {
    /// 空上下文（模型未加载或没有前词时用）
    pub const fn empty() -> Ctx {
        Ctx {
            ok: false,
            has_tg_ctx: false,
            bow: 0.0,
            tg_lo: 0,
            tg_hi: 0,
            tg_n: 0,
            tg_inline: [(0, 0.0); INLINE],
            bg_lo: 0,
            bg_hi: 0,
            bg_n: 0,
            bg_inline: [(0, 0.0); INLINE],
            bg_bow: 0.0,
        }
    }

    pub fn prepare(prev2: u32, prev1: u32) -> Ctx {
        let m = data();
        let mut c = Ctx {
            ok: m.v > 0 && prev1 != SENTINEL && prev1 < m.v,
            has_tg_ctx: false,
            bow: 0.0,
            tg_lo: 0,
            tg_hi: 0,
            tg_n: 0,
            tg_inline: [(0, 0.0); INLINE],
            bg_lo: 0,
            bg_hi: 0,
            bg_n: 0,
            bg_inline: [(0, 0.0); INLINE],
            bg_bow: 0.0,
        };
        if !c.ok {
            return c;
        }
        c.bg_bow = uni_bow_log10(prev1);
        // 二元段
        let (lo, hi) = bg_range(m, prev1);
        c.bg_lo = lo;
        c.bg_hi = hi;
        let n = hi - lo;
        if n <= INLINE {
            c.bg_n = n as u8;
            for i in 0..n {
                let off = m.bg + (lo + i) * BG_ENTRY;
                c.bg_inline[i] = (rd32(RAW, off), val(RAW, off + 4));
            }
        } else {
            c.bg_n = u8::MAX;
        }
        // 三元上下文（prev2, prev1）
        if prev2 == SENTINEL || prev2 >= m.v {
            return c;
        }
        let Some(bi) = find_bg(m, prev2, prev1) else {
            return c;
        };
        c.has_tg_ctx = true;
        c.bow = bg_bow(m, bi);
        let lo = rd32(RAW, m.tg_off + bi * 4) as usize;
        let hi = rd32(RAW, m.tg_off + (bi + 1) * 4) as usize;
        c.tg_lo = lo;
        c.tg_hi = hi;
        let n = hi - lo;
        if n <= INLINE {
            c.tg_n = n as u8;
            for i in 0..n {
                let off = m.tg + (lo + i) * TG_ENTRY;
                c.tg_inline[i] = (rd32(RAW, off), val(RAW, off + 4));
            }
        } else {
            c.tg_n = u8::MAX;
        }
        c
    }

    /// log10 P(w | prev2, prev1)，回退链与 `log10_p` 完全一致，只是数据已就位
    #[inline]
    pub fn log10_p(&self, w: u32) -> f32 {
        if !self.ok {
            return uni_log10(w);
        }
        if let Some(p) = self.find_tg(w) {
            return p;
        }
        let lower = self.find_bg(w);
        if self.has_tg_ctx {
            self.bow + lower
        } else {
            lower
        }
    }

    #[inline]
    fn find_tg(&self, w: u32) -> Option<f32> {
        match self.tg_n {
            0 => None,
            n if n <= INLINE as u8 => {
                for i in 0..n as usize {
                    let (id, p) = self.tg_inline[i];
                    if id == w {
                        return Some(p);
                    }
                    if id > w {
                        break;
                    }
                }
                None
            }
            _ => {
                let m = data();
                let (mut l, mut r) = (self.tg_lo, self.tg_hi);
                while l < r {
                    let mid = (l + r) / 2;
                    if rd32(RAW, m.tg + mid * TG_ENTRY) < w {
                        l = mid + 1;
                    } else {
                        r = mid;
                    }
                }
                if l < self.tg_hi && rd32(RAW, m.tg + l * TG_ENTRY) == w {
                    Some(val(RAW, m.tg + l * TG_ENTRY + 4))
                } else {
                    None
                }
            }
        }
    }

    /// log10 P(w|prev1)：命中存储值，否则 bow(prev1) + log10 P1(w)
    #[inline]
    fn find_bg(&self, w: u32) -> f32 {
        match self.bg_n {
            0 => self.bg_bow + uni_log10(w),
            n if n <= INLINE as u8 => {
                for i in 0..n as usize {
                    let (id, p) = self.bg_inline[i];
                    if id == w {
                        return p;
                    }
                    if id > w {
                        break;
                    }
                }
                self.bg_bow + uni_log10(w)
            }
            _ => {
                let m = data();
                let (mut l, mut r) = (self.bg_lo, self.bg_hi);
                while l < r {
                    let mid = (l + r) / 2;
                    if rd32(RAW, m.bg + mid * BG_ENTRY) < w {
                        l = mid + 1;
                    } else {
                        r = mid;
                    }
                }
                if l < self.bg_hi && rd32(RAW, m.bg + l * BG_ENTRY) == w {
                    bg_p(m, l)
                } else {
                    self.bg_bow + uni_log10(w)
                }
            }
        }
    }

    /// 上下文代价：`log10 P1(w) - log10 P(w|ctx)`。`uni_w` 由调用方按候选词
    /// 预计算（同一候选词在所有路径上相同），省掉一次查表。
    #[inline]
    pub fn cost(&self, w: u32, uni_w: f32) -> f32 {
        let m = data();
        if !self.ok || w >= m.v {
            return 0.0;
        }
        uni_w - self.log10_p(w)
    }
}

/// 上下文代价：`log10 P1(w) - log10 P(w|ctx)`，正数表示「这个词在这个上下文里
/// 比孤立出现更不可能」。乘上引擎的权重后加入路径代价。
///
/// 用「相对一元」的差值而不是直接用 `-log10 P`，是为了让词库词频继续主导排序
/// （词频里含人工提频与领域词表），三元只负责上下文的加减分。
///
/// 与 `log10_p` 一样是参照实现；热路径用 `Ctx::cost`。
#[allow(dead_code)]
#[inline]
pub fn context_cost(prev2: u32, prev1: u32, w: u32) -> f32 {
    let m = data();
    if m.v == 0 || w >= m.v {
        return 0.0;
    }
    uni_log10(w) - log10_p(prev2, prev1, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_is_loaded() {
        // 数据文件随源码入库；这里只确认格式没坏、表非空
        let m = data();
        assert!(m.v > 100_000, "词形数异常：{}", m.v);
        assert!(m.n_bg > 100_000, "二元条目数异常：{}", m.n_bg);
        assert!(m.n_tg > 100_000, "三元条目数异常：{}", m.n_tg);
    }

    #[test]
    fn probabilities_are_sane() {
        for w in [0u32, 1, 100, 1000] {
            let p = uni_log10(w);
            assert!(p <= 0.0 && p > -30.0, "词形 {w} 的 log10 P = {p}");
        }
    }

    #[test]
    fn ctx_matches_single_query() {
        // 预热版与单次查询版必须完全等价，否则解码结果会随缓存命中率漂移
        let mut checked = 0;
        for prev2 in [SENTINEL, 0u32, 7, 100, 5000] {
            for prev1 in [SENTINEL, 0u32, 3, 42, 900] {
                let ctx = Ctx::prepare(prev2, prev1);
                for w in [0u32, 1, 2, 33, 77, 1000, 5000] {
                    let a = context_cost(prev2, prev1, w);
                    let b = ctx.cost(w, uni_log10(w));
                    assert!(
                        (a - b).abs() < 1e-6,
                        "prev2={prev2} prev1={prev1} w={w}: {a} vs {b}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 100);
    }

    #[test]
    fn missing_context_degrades_quietly() {
        // 不存在的上下文：回退到一元，代价退化为 0（而不是 NaN/∞）
        let c = context_cost(SENTINEL, SENTINEL, 0);
        assert_eq!(c, 0.0);
        let c = context_cost(u32::MAX - 1, u32::MAX - 1, 0);
        assert!(c.is_finite());
    }
}
