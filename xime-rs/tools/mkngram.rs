//! 三元语言模型：用词库切分语料，统计 **modified Kneser-Ney** 平滑的三阶模型，
//! 输出 `ngram.bin`（运行时用）与可选 ARPA（给 kenlm 的 `build_binary` 用）。
//!
//! 为什么要有这个工具：`bigram.bin` 是「PMI 折算的二阶表」，只能做排序微调，跨词
//! 歧义（及其/机器 + 学习）看不到更早的上下文。fcitx5 的 libime 用的是三元
//! （sunpinyin 同源的 beam search + 三元），这里对齐的就是那套语义：
//!
//! | 环节 | kenlm / lmplz | 本工具 |
//! |---|---|---|
//! | 平滑 | modified Kneser-Ney（Chen-Goodman） | 同 |
//! | 回退 | ARPA：`P(w|ctx) = bow(ctx)·P(w|ctx')` | 同，log10 |
//! | 低阶计数 | 接续计数（不同左上下文个数） | 同 |
//! | 剪枝 | `--prune` 按相对熵 | 计数阈值 + 全局配额（更简单、可复现） |
//! | 二进制 | trie + 量化（q8） | 分组有序表 + i16 定点（1e-3 log10） |
//!
//! 二进制布局见 `src/ngram.rs`。ARPA 导出的用途是互验：`build_binary` + `query`
//! 打分应当与 `ngram.bin` 一致到量化误差。
//!
//! ```sh
//! rustc --edition 2021 -O -o tools/mkngram tools/mkngram.rs
//! tools/mkngram --corpus .cache/corpus/wiki.txt \
//!     --out-map data/val_map.bin --out-ngram data/ngram.bin [--arpa /tmp/zh.arpa]
//! ```

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Write};

const MAGIC_DICT: &[u8; 8] = b"XIMEIM01";
const MAGIC_MAP: &[u8; 8] = b"XIMEVM01";
const MAGIC_NG: &[u8; 8] = b"XIMENG01";

/// 与 mkdict.py 的收词上限一致；更长的词不会被匹配到
const MAX_WORD_CHARS: usize = 6;
/// id 流里的断句哨兵（标点、数字、生僻字、行尾）
const SEP: u32 = u32::MAX;

// ---------------------------------------------------------------- 哈希

/// FxHash（rustc 内部那套）。数据全部是本地构建产物，没有抗碰撞需求。
#[derive(Default, Clone, Copy)]
struct Fx(u64);

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl Fx {
    #[inline]
    fn add(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(SEED);
    }
}

impl Hasher for Fx {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut b = bytes;
        while b.len() >= 8 {
            self.add(u64::from_le_bytes(b[..8].try_into().unwrap()));
            b = &b[8..];
        }
        if !b.is_empty() {
            let mut v = 0u64;
            for (i, &x) in b.iter().enumerate() {
                v |= (x as u64) << (i * 8);
            }
            self.add(v);
        }
        self.add(bytes.len() as u64);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

#[inline]
fn new_map<K, V>() -> FxMap<K, V> {
    HashMap::with_hasher(BuildHasherDefault::default())
}

// ---------------------------------------------------------------- 词库

struct Dict<'a> {
    keys: Vec<&'a str>,
    vals: Vec<&'a str>,
    freqs: Vec<u32>,
}

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn load_dict(raw: &[u8]) -> Result<Dict<'_>, String> {
    if raw.len() < 32 || &raw[0..8] != MAGIC_DICT {
        return Err("不是 xime 词库文件".into());
    }
    let word_count = rd32(raw, 8) as usize;
    let blob_len = rd32(raw, 12) as usize;
    let refs = 32;
    let blob_start = refs + word_count * 12;
    if blob_start + blob_len > raw.len() {
        return Err("词库文件被截断".into());
    }
    let blob = &raw[blob_start..blob_start + blob_len];

    let cstr = |o: u32| -> Result<&str, String> {
        let o = o as usize;
        let end = blob[o..]
            .iter()
            .position(|&c| c == 0)
            .ok_or("词库字符串未终止")?;
        std::str::from_utf8(&blob[o..o + end]).map_err(|e| e.to_string())
    };

    let mut keys = Vec::with_capacity(word_count);
    let mut vals = Vec::with_capacity(word_count);
    let mut freqs = Vec::with_capacity(word_count);
    for i in 0..word_count {
        let o = refs + i * 12;
        keys.push(cstr(rd32(raw, o))?);
        vals.push(cstr(rd32(raw, o + 4))?);
        freqs.push(rd32(raw, o + 8));
    }
    Ok(Dict { keys, vals, freqs })
}

/// 按词形最高词频降序分配 val_id。排序必须稳定（同频按首次出现顺序），
/// 否则两次构建的 val_id 会漂，转移表整体错位。
fn build_val_map<'a>(d: &Dict<'a>) -> (Vec<&'a str>, FxMap<&'a str, u32>, Vec<u32>) {
    let mut best: FxMap<&str, u32> = new_map();
    let mut order: Vec<&'a str> = Vec::new();
    for (v, &f) in d.vals.iter().zip(d.freqs.iter()) {
        match best.get_mut(v) {
            Some(cur) => {
                if f > *cur {
                    *cur = f;
                }
            }
            None => {
                best.insert(v, f);
                order.push(v);
            }
        }
    }
    order.sort_by(|a, b| best[b].cmp(&best[a]));

    let mut val_to_id: FxMap<&'a str, u32> = new_map();
    val_to_id.reserve(order.len());
    for (i, v) in order.iter().enumerate() {
        val_to_id.insert(v, i as u32);
    }
    let word_to_val: Vec<u32> = d.vals.iter().map(|v| val_to_id[v]).collect();
    (order, val_to_id, word_to_val)
}

fn write_val_map(path: &str, word_to_val: &[u32]) -> std::io::Result<()> {
    let n = word_to_val.len();
    let mut f = BufWriter::new(std::fs::File::create(path)?);
    f.write_all(MAGIC_MAP)?;
    f.write_all(&(n as u32).to_le_bytes())?;
    f.write_all(&[0u8; 20])?;
    let mut buf = Vec::with_capacity(n * 4);
    for v in word_to_val {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    f.write_all(&buf)?;
    f.flush()?;
    let val_count = word_to_val.iter().copied().max().unwrap_or(0) + 1;
    println!("生成 {path}（{n} 词条 -> {val_count} 词形）");
    Ok(())
}

// ---------------------------------------------------------------- 切分

#[inline]
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 单个字的词形 id（不在词库返回 None）
#[inline]
fn char_id(c: char, val: &FxMap<&str, u32>) -> Option<u32> {
    let mut buf = [0u8; 4];
    val.get(c.encode_utf8(&mut buf) as &str).copied()
}

/// 贪心最长匹配切分一行，把 id 追加进 `out`；不在词库的字符写成断句哨兵。
///
/// 哨兵不只是省事：语料按句切行，句内仍有逗号、顿号、括号，不断开就会把
/// 「模型，带来」统计成 模型→带 的伪三元组。
fn segment_line(
    line: &str,
    val: &FxMap<&str, u32>,
    out: &mut Vec<u32>,
    intra: &mut Vec<(u32, u32)>,
) {
    let b = line.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let mut ends = [0usize; MAX_WORD_CHARS + 1];
        let mut n = 0usize;
        let mut j = i;
        while n < MAX_WORD_CHARS && j < b.len() {
            j += utf8_len(b[j]);
            n += 1;
            ends[n] = j;
        }
        let mut hit = None;
        while n > 0 {
            if let Some(&id) = val.get(&line[i..ends[n]]) {
                hit = Some((id, ends[n]));
                break;
            }
            n -= 1;
        }
        match hit {
            Some((id, end)) => {
                out.push(id);
                // 整词内部补记相邻字对。语料里「我想」几乎总被切成整词（词库收了它），
                // 于是 (我, 想) 这个二元对在统计里几乎不存在，KN 反而认为「我像」
                // 更常见——用户打 womenyihui 时就会出现「我像出去玩」这类选择。
                // 补记后两种切分都拿得到统计，谁更合理交给解码器比代价。
                if end - i > 3 {
                    let mut prev = None;
                    for c in line[i..end].chars() {
                        if let (Some(p), Some(q)) = (prev, char_id(c, val)) {
                            if let Some(pid) = char_id(p, val) {
                                intra.push((pid, q));
                            }
                        }
                        prev = Some(c);
                    }
                }
                i = end;
            }
            None => {
                if out.last() != Some(&SEP) {
                    out.push(SEP);
                }
                i += utf8_len(b[i]);
            }
        }
    }
    if out.last() != Some(&SEP) {
        out.push(SEP);
    }
}

// ---------------------------------------------------------------- 计数

struct Pass1 {
    uni: FxMap<u32, u32>,
    bi: FxMap<u64, u32>,
}

fn count_chunk(
    text: &str,
    val: &FxMap<&str, u32>,
    ids_out: &mut impl Write,
) -> std::io::Result<Pass1> {
    let mut uni: FxMap<u32, u32> = new_map();
    let mut bi: FxMap<u64, u32> = new_map();
    uni.reserve(1 << 14);
    bi.reserve(1 << 16);
    let mut ids: Vec<u32> = Vec::with_capacity(8192);
    let mut intra: Vec<(u32, u32)> = Vec::with_capacity(4096);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);

    for line in text.lines() {
        ids.clear();
        intra.clear();
        segment_line(line, val, &mut ids, &mut intra);
        for &(a, b) in &intra {
            *bi.entry(((a as u64) << 32) | b as u64).or_insert(0) += 1;
        }
        for w in ids.windows(2) {
            if let [a, b] = *w {
                if a != SEP && b != SEP {
                    *bi.entry(((a as u64) << 32) | b as u64).or_insert(0) += 1;
                }
            }
        }
        for &w in &ids {
            if w != SEP {
                *uni.entry(w).or_insert(0) += 1;
            }
        }
        buf.clear();
        for &w in &ids {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        ids_out.write_all(&buf)?;
    }
    Ok(Pass1 { uni, bi })
}

fn merge_into(dst: &mut FxMap<u64, u32>, src: FxMap<u64, u32>) {
    if dst.is_empty() {
        *dst = src;
        return;
    }
    for (k, v) in src {
        *dst.entry(k).or_insert(0) += v;
    }
}

fn merge_small(dst: &mut FxMap<u32, u32>, src: FxMap<u32, u32>) {
    if dst.is_empty() {
        *dst = src;
        return;
    }
    for (k, v) in src {
        *dst.entry(k).or_insert(0) += v;
    }
}

// ---------------------------------------------------------------- Kneser-Ney

#[derive(Clone, Copy, Debug)]
struct Discounts {
    d1: f64,
    d2: f64,
    d3: f64,
}

impl Discounts {
    /// Chen & Goodman 的 modified KN：Y = N1/(N1+2N2)，
    /// D1 = 1-2Y·N2/N1，D2 = 2-3Y·N3/N2，D3 = 3-4Y·N4/N3。
    fn fit(n1: u64, n2: u64, n3: u64, n4: u64) -> Discounts {
        if n1 == 0 || n2 == 0 {
            return Discounts { d1: 0.5, d2: 1.0, d3: 1.5 };
        }
        let (n1f, n2f) = (n1 as f64, n2 as f64);
        let y = n1f / (n1f + 2.0 * n2f);
        let d1 = 1.0 - 2.0 * y * n2f / n1f;
        let d2 = if n3 > 0 { 2.0 - 3.0 * y * n3 as f64 / n2f } else { 2.0 * d1 };
        let d3 = if n3 > 0 && n4 > 0 {
            3.0 - 4.0 * y * n4 as f64 / n3 as f64
        } else {
            3.0 * d1
        };
        Discounts {
            d1: d1.clamp(0.05, 1.0),
            d2: d2.clamp(0.05, 2.0),
            d3: d3.clamp(0.05, 3.0),
        }
    }

    #[inline]
    fn of(&self, count: u32) -> f64 {
        match count {
            1 => self.d1,
            2 => self.d2,
            _ => self.d3,
        }
    }
}

/// 计数分布：N1..N4 拟合 discount，N3+ 算插值权重。
#[derive(Default, Clone, Copy)]
struct CountOfCounts {
    n1: u64,
    n2: u64,
    n3: u64,
    n4: u64,
    /// 计数 >= 3 的种类数（含 n3、n4）
    n3p: u64,
}

impl CountOfCounts {
    #[inline]
    fn add(&mut self, c: u32) {
        match c {
            1 => self.n1 += 1,
            2 => self.n2 += 1,
            3 => {
                self.n3 += 1;
                self.n3p += 1;
            }
            4 => {
                self.n4 += 1;
                self.n3p += 1;
            }
            _ => self.n3p += 1,
        }
    }
    fn discounts(&self) -> Discounts {
        Discounts::fit(self.n1, self.n2, self.n3, self.n4)
    }
}

/// 上下文统计：总计数 + 计数分布。λ（插值权重）就是被折扣掉的那份质量占比。
#[derive(Default, Clone, Copy)]
struct CtxStats {
    total: u64,
    coc: CountOfCounts,
}

impl CtxStats {
    #[inline]
    fn add(&mut self, c: u32) {
        self.total += c as u64;
        self.coc.add(c);
    }
    fn lambda(&self, d: &Discounts) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        let mass =
            d.d1 * self.coc.n1 as f64 + d.d2 * self.coc.n2 as f64 + d.d3 * self.coc.n3p as f64;
        (mass / self.total as f64).clamp(0.0, 1.0)
    }
}

/// 给定候选（计数, 配额, 下限），返回保留阈值。并列时整体保留，实际条数可能
/// 略多于配额——两次构建结果一致比精确条数重要。
fn count_threshold(counts: &[u32], min: u32, top: usize) -> u32 {
    let mut c: Vec<u32> = counts.iter().copied().filter(|&x| x >= min).collect();
    if c.is_empty() {
        return u32::MAX;
    }
    if c.len() > top {
        // 保留计数最大的 top 个：降序选择，再用其中最小值当阈值
        c.select_nth_unstable_by(top, |a, b| b.cmp(a));
        c.truncate(top);
    }
    *c.iter().min().unwrap_or(&min)
}

// ---------------------------------------------------------------- 模型

struct Model {
    /// 每个词形的一元概率 P(w)
    uni_p: Vec<f32>,
    /// 每个词形的回退权重 bow(a)：P(w|a) 未命中时乘上它再退到一元
    uni_bow: Vec<f32>,
    /// 二元条目：next、P(next|a)、bow(a,next)（三元回退用）
    bg: Vec<(u32, f32, f32)>,
    /// 每个前词 a 的条目区间（len = V+1）
    bg_off: Vec<u32>,
    /// 三元条目：next、P(next|a,b)
    tg: Vec<(u32, f32)>,
    /// 每个二元上下文的条目区间（len = n_bg+1），下标即上下文 id
    tg_off: Vec<u32>,
    /// 上下文 id -> 该二元的原始计数（归一化用）
    ctx_count: Vec<u32>,
}

/// log10 定点：1e-3 为单位。i16 覆盖 [-32.7, 0]，够表示 1e-30 的概率
#[inline]
fn q(v: f64) -> i16 {
    let s = v * 1000.0;
    if !s.is_finite() {
        return -32000;
    }
    s.round().clamp(-32000.0, 32000.0) as i16
}

fn build_model(
    bigrams: &[(u64, u32)],
    tris: &[(u64, u32)],
    val_count: usize,
    thr_bg: u32,
    thr_tg: u32,
) -> Model {
    // ---- 一元：接续计数（有多少个不同的前词）
    let mut cc = vec![0u32; val_count];
    for &(k, _) in bigrams {
        let w = (k & 0xFFFF_FFFF) as usize;
        if w < val_count {
            cc[w] += 1;
        }
    }
    let total_cc: u64 = cc.iter().map(|&c| c as u64).sum::<u64>().max(1);
    let mut coc = CountOfCounts::default();
    for &c in &cc {
        if c > 0 {
            coc.add(c);
        }
    }
    let d_uni = coc.discounts();
    let mass = d_uni.d1 * coc.n1 as f64 + d_uni.d2 * coc.n2 as f64 + d_uni.d3 * coc.n3p as f64;
    let lambda_uni = mass / total_cc as f64;
    let puni_uniform = 1.0 / val_count as f64;

    let mut uni_p = vec![0f32; val_count];
    for w in 0..val_count {
        let p = if cc[w] > 0 {
            (cc[w] as f64 - d_uni.of(cc[w])).max(0.0) / total_cc as f64
                + lambda_uni * puni_uniform
        } else {
            lambda_uni * puni_uniform
        };
        uni_p[w] = p.max(1e-12) as f32;
    }

    // ---- 二元：每阶一套 discount，按该阶的全局计数分布拟合
    let mut coc_bg = CountOfCounts::default();
    for &(_, c) in bigrams {
        coc_bg.add(c);
    }
    let d_bg = coc_bg.discounts();

    let mut bg: Vec<(u32, f32, f32)> = Vec::new();
    let mut bg_off: Vec<u32> = Vec::with_capacity(val_count + 1);
    let mut uni_bow = vec![1f32; val_count];
    let mut ctx_count: Vec<u32> = Vec::new();

    let mut i = 0usize;
    for a in 0..val_count {
        bg_off.push(bg.len() as u32);
        let start = i;
        let mut stats = CtxStats::default();
        while i < bigrams.len() && (bigrams[i].0 >> 32) as usize == a {
            stats.add(bigrams[i].1);
            i += 1;
        }
        if i == start {
            continue;
        }
        let lam = stats.lambda(&d_bg);
        let mut sum_p = 0f64;
        let mut sum_lower = 0f64;
        for &(k, c) in &bigrams[start..i] {
            if c < thr_bg {
                continue;
            }
            let b = (k & 0xFFFF_FFFF) as usize;
            let p = ((c as f64 - d_bg.of(c)).max(0.0) / stats.total as f64
                + lam * uni_p[b] as f64) as f32;
            sum_p += p as f64;
            sum_lower += uni_p[b] as f64;
            ctx_count.push(c);
            bg.push((b as u32, p, 0.0));
        }
        // 剪掉的概率质量必须由回退补上，否则这个上下文的总概率 < 1
        let bow = if sum_lower < 1.0 && sum_p < 1.0 {
            ((1.0 - sum_p) / (1.0 - sum_lower)).clamp(1e-6, 1e6)
        } else {
            1.0
        };
        uni_bow[a] = bow as f32;
        for e in &mut bg[bg_off[a] as usize..] {
            e.2 = bow as f32;
        }
    }
    bg_off.push(bg.len() as u32);

    // ---- 三元：逐上下文算 λ，条目按阈值剪枝。三阶是最高阶，所以不需要
    // 三元上下文的回退权重（没有四元来退）。
    let mut coc_tg = CountOfCounts::default();
    for &(_, c) in tris {
        coc_tg.add(c);
    }
    let d_tg = coc_tg.discounts();

    let n_bg = ctx_count.len();
    let mut tg: Vec<(u32, f32)> = Vec::new();
    let mut tg_off = vec![0u32; n_bg + 1];
    let mut i = 0usize;
    while i < tris.len() {
        let ctx = (tris[i].0 >> 32) as usize;
        let start = i;
        let mut stats = CtxStats::default();
        while i < tris.len() && (tris[i].0 >> 32) as usize == ctx {
            stats.add(tris[i].1);
            i += 1;
        }
        let lam = stats.lambda(&d_tg);
        let base = ctx_count[ctx] as f64;
        let b_word = bg[ctx].0;
        let mut sum_p = 0f64;
        let mut sum_lower = 0f64;
        for &(k, c) in &tris[start..i] {
            if c < thr_tg {
                continue;
            }
            let w = k as u32;
            let lower = bg_lookup(&bg, &bg_off, &uni_p, &uni_bow, b_word, w);
            let p = ((c as f64 - d_tg.of(c)).max(0.0) / base + lam * lower) as f32;
            sum_p += p as f64;
            sum_lower += lower;
            tg.push((w, p));
        }
        tg_off[ctx + 1] = tg.len() as u32;
        // 2-gram 行的 bow 不是「二元残差」，而是**三元上下文**的回退权重：
        // P(w|a,b) 对没进模型的 w 要退回 P(w|b)，靠它把总概率补回 1。
        // 二元自己的回退（P(w|a) 未命中退回一元）用的是 1-gram 行的 bow。
        let bow = if sum_lower < 1.0 && sum_p < 1.0 {
            ((1.0 - sum_p) / (1.0 - sum_lower)).clamp(1e-6, 1e6)
        } else {
            1.0
        };
        bg[ctx].2 = bow as f32;
    }
    // 整组被剪掉的上下文没有三元条目：offset 沿用前一个，保持单调
    for c in 1..=n_bg {
        if tg_off[c] < tg_off[c - 1] {
            tg_off[c] = tg_off[c - 1];
        }
    }

    Model { uni_p, uni_bow, bg, bg_off, tg, tg_off, ctx_count }
}

/// 运行时同口径的二元查询：命中返回存储值，未命中返回 bow(b)·P(w)。
fn bg_lookup(
    bg: &[(u32, f32, f32)],
    bg_off: &[u32],
    uni_p: &[f32],
    uni_bow: &[f32],
    b: u32,
    w: u32,
) -> f64 {
    let lo = bg_off[b as usize] as usize;
    let hi = bg_off[b as usize + 1] as usize;
    let (mut l, mut r) = (lo, hi);
    while l < r {
        let mid = (l + r) / 2;
        if bg[mid].0 < w {
            l = mid + 1;
        } else {
            r = mid;
        }
    }
    if l < hi && bg[l].0 == w {
        bg[l].1 as f64
    } else {
        uni_bow[b as usize] as f64 * uni_p[w as usize] as f64
    }
}

// ---------------------------------------------------------------- 自检

/// 归一化自检：随机上下文上 Σ P(w|ctx) + bow·(1-Σ P(w|ctx')) 应当 = 1。
fn verify(m: &Model, samples: usize) {
    let v = m.uni_p.len();
    let n_bg = m.ctx_count.len();
    if v == 0 || n_bg == 0 {
        return;
    }
    let mut worst_bg = 0f64;
    let mut worst_tg = 0f64;
    let step = (n_bg / samples.max(1)).max(1);
    let mut a = 0usize;
    while a < v {
        let lo = m.bg_off[a] as usize;
        let hi = m.bg_off[a + 1] as usize;
        if hi > lo {
            let mut sp = 0f64;
            let mut sl = 0f64;
            for &(b, p, _) in &m.bg[lo..hi] {
                sp += p as f64;
                sl += m.uni_p[b as usize] as f64;
            }
            let total = sp + m.uni_bow[a] as f64 * (1.0 - sl);
            worst_bg = worst_bg.max((total - 1.0).abs());
        }
        a += 1;
    }
    let mut c = 0usize;
    while c < n_bg {
        let lo = m.tg_off[c] as usize;
        let hi = m.tg_off[c + 1] as usize;
        if hi > lo {
            let b_word = m.bg[c].0;
            let mut sp = 0f64;
            let mut sl = 0f64;
            for &(w, p) in &m.tg[lo..hi] {
                sp += p as f64;
                let lower = bg_lookup(&m.bg, &m.bg_off, &m.uni_p, &m.uni_bow, b_word, w);
                sl += lower;
            }
            let total = sp + m.bg[c].2 as f64 * (1.0 - sl);
            worst_tg = worst_tg.max((total - 1.0).abs());
        }
        c += step;
    }
    let usum: f64 = m.uni_p.iter().map(|&p| p as f64).sum();
    println!(
        "自检：ΣP1 = {usum:.4}；二元各上下文偏差最大 {worst_bg:.4}；三元 {worst_tg:.4}"
    );
}

// ---------------------------------------------------------------- 写盘

fn write_ngram(path: &str, m: &Model) -> std::io::Result<()> {
    let v = m.uni_p.len();
    let mut f = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    f.write_all(MAGIC_NG)?;
    f.write_all(&3u32.to_le_bytes())?;
    f.write_all(&(v as u32).to_le_bytes())?;
    f.write_all(&(m.bg.len() as u32).to_le_bytes())?;
    f.write_all(&(m.tg.len() as u32).to_le_bytes())?;
    f.write_all(&[0u8; 8])?;

    let mut buf: Vec<u8> = Vec::with_capacity(v * 4);
    for &p in &m.uni_p {
        buf.extend_from_slice(&q((p as f64).log10()).to_le_bytes());
    }
    for &b in &m.uni_bow {
        buf.extend_from_slice(&q((b as f64).log10()).to_le_bytes());
    }
    f.write_all(&buf)?;

    let mut buf: Vec<u8> = Vec::with_capacity((v + 1) * 4);
    for &o in &m.bg_off {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    f.write_all(&buf)?;
    let mut buf: Vec<u8> = Vec::with_capacity(m.bg.len() * 8);
    for &(n, p, bow) in &m.bg {
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&q((p as f64).log10()).to_le_bytes());
        buf.extend_from_slice(&q((bow as f64).log10()).to_le_bytes());
    }
    f.write_all(&buf)?;

    let mut buf: Vec<u8> = Vec::with_capacity((m.tg_off.len()) * 4);
    for &o in &m.tg_off {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    f.write_all(&buf)?;
    let mut buf: Vec<u8> = Vec::with_capacity(m.tg.len() * 6);
    for &(n, p) in &m.tg {
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&q((p as f64).log10()).to_le_bytes());
    }
    f.write_all(&buf)?;
    f.flush()?;
    let size = std::fs::metadata(path)?.len();
    println!(
        "生成 {path}（一元 {v} / 二元 {} / 三元 {}，{:.1} MB）",
        m.bg.len(),
        m.tg.len(),
        size as f64 / (1 << 20) as f64
    );
    Ok(())
}

fn write_arpa(path: &str, m: &Model, vals: &[&str]) -> std::io::Result<()> {
    let mut f = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    writeln!(f, "\\data\\")?;
    writeln!(f, "ngram 1={}", m.uni_p.len())?;
    writeln!(f, "ngram 2={}", m.bg.len())?;
    writeln!(f, "ngram 3={}", m.tg.len())?;
    writeln!(f)?;
    writeln!(f, "\\1-grams:")?;
    for (w, &p) in m.uni_p.iter().enumerate() {
        writeln!(
            f,
            "{:.6}\t{}\t{:.6}",
            (p as f64).log10(),
            vals[w],
            (m.uni_bow[w] as f64).log10()
        )?;
    }
    writeln!(f)?;
    writeln!(f, "\\2-grams:")?;
    for a in 0..m.uni_p.len() {
        for &(b, p, bow) in &m.bg[m.bg_off[a] as usize..m.bg_off[a + 1] as usize] {
            writeln!(
                f,
                "{:.6}\t{} {}\t{:.6}",
                (p as f64).log10(),
                vals[a],
                vals[b as usize],
                (bow as f64).log10()
            )?;
        }
    }
    writeln!(f)?;
    writeln!(f, "\\3-grams:")?;
    for a in 0..m.uni_p.len() {
        for idx in m.bg_off[a] as usize..m.bg_off[a + 1] as usize {
            let b = m.bg[idx].0 as usize;
            for &(w, p) in &m.tg[m.tg_off[idx] as usize..m.tg_off[idx + 1] as usize] {
                writeln!(
                    f,
                    "{:.6}\t{} {} {}",
                    (p as f64).log10(),
                    vals[a],
                    vals[b],
                    vals[w as usize]
                )?;
            }
        }
    }
    writeln!(f)?;
    writeln!(f, "\\end\\")?;
    f.flush()?;
    Ok(())
}

// ---------------------------------------------------------------- 主流程

fn main() {
    let mut dict_path = String::from("data/dict.bin");
    let mut corpus_path: Option<String> = None;
    let mut out_map = String::from("data/val_map.bin");
    let mut out_ngram = String::from("data/ngram.bin");
    let mut arpa_path: Option<String> = None;
    let mut min_bigram: u32 = 2;
    let mut min_trigram: u32 = 3;
    let mut top_bigram: usize = 2_000_000;
    let mut top_trigram: usize = 3_000_000;
    let mut max_lines: usize = 0;
    let mut stats_only = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |name: &str| -> String {
            it.next().unwrap_or_else(|| {
                eprintln!("{name} 需要参数");
                std::process::exit(2)
            })
        };
        match a.as_str() {
            "--dict" => dict_path = val("--dict"),
            "--corpus" => corpus_path = Some(val("--corpus")),
            "--out-map" => out_map = val("--out-map"),
            "--out-ngram" => out_ngram = val("--out-ngram"),
            "--arpa" => arpa_path = Some(val("--arpa")),
            "--min-bigram" => min_bigram = val("--min-bigram").parse().unwrap_or(2),
            "--min-trigram" => min_trigram = val("--min-trigram").parse().unwrap_or(3),
            "--top-bigram" => top_bigram = val("--top-bigram").parse().unwrap_or(2_000_000),
            "--top-trigram" => top_trigram = val("--top-trigram").parse().unwrap_or(3_000_000),
            "--max-lines" => max_lines = val("--max-lines").parse().unwrap_or(0),
            "--stats" => stats_only = true,
            "-h" | "--help" => {
                println!(
                    "用法: mkngram --corpus <file> [--dict <dict.bin>]\n\
                     \x20   [--out-map <val_map.bin>] [--out-ngram <ngram.bin>] [--arpa <file>]\n\
                     \x20   [--min-bigram N] [--min-trigram N]\n\
                     \x20   [--top-bigram N] [--top-trigram N] [--max-lines N] [--stats]"
                );
                return;
            }
            other => {
                eprintln!("未知参数 {other}（--help 看用法）");
                std::process::exit(2);
            }
        }
    }
    let Some(corpus_path) = corpus_path else {
        eprintln!("必须给 --corpus（--help 看用法）");
        std::process::exit(2);
    };

    let dict_raw = std::fs::read(&dict_path).unwrap_or_else(|e| {
        eprintln!("读不了 {dict_path}: {e}");
        std::process::exit(1)
    });
    let dict = load_dict(&dict_raw).unwrap_or_else(|e| {
        eprintln!("{dict_path}: {e}");
        std::process::exit(1)
    });
    let (vals, val_to_id, word_to_val) = build_val_map(&dict);
    let val_count = vals.len();
    println!("词条 {}，词形 {val_count}", dict.keys.len());

    let mut corpus = std::fs::read(&corpus_path).unwrap_or_else(|e| {
        eprintln!("读不了 {corpus_path}: {e}");
        std::process::exit(1)
    });
    if max_lines > 0 {
        let mut seen = 0usize;
        let mut end = corpus.len();
        for (i, &c) in corpus.iter().enumerate() {
            if c == b'\n' {
                seen += 1;
                if seen >= max_lines {
                    end = i + 1;
                    break;
                }
            }
        }
        corpus.truncate(end);
    }

    let bounds = split_bounds(&corpus, 12);
    let chunks: Vec<&str> = bounds
        .windows(2)
        .map(|w| std::str::from_utf8(&corpus[w[0]..w[1]]).expect("语料不是合法 UTF-8"))
        .collect();
    let nthreads = chunks.len();

    // 第一遍：切分 + 统计 unigram/bigram，同时把 id 流写盘。第二遍统计三元组时
    // 直接读 id，不必每个线程重新切分整份语料。
    let tmp_dir = std::env::temp_dir().join(format!("xime-mkngram-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);

    let t0 = std::time::Instant::now();
    let mut uni: FxMap<u32, u32> = new_map();
    let mut bi: FxMap<u64, u32> = new_map();
    std::thread::scope(|s| {
        let val = &val_to_id;
        let handles: Vec<_> = chunks
            .iter()
            .enumerate()
            .map(|(k, chunk)| {
                let path = tmp_dir.join(format!("ids.{k}"));
                s.spawn(move || {
                    let f = std::fs::File::create(&path).expect("建不了临时文件");
                    let mut w = BufWriter::with_capacity(1 << 20, f);
                    let r = count_chunk(chunk, val, &mut w).expect("写 id 流失败");
                    w.flush().expect("刷 id 流失败");
                    r
                })
            })
            .collect();
        for h in handles {
            let r = h.join().unwrap();
            merge_small(&mut uni, r.uni);
            merge_into(&mut bi, r.bi);
        }
    });
    println!(
        "第一遍：{} 行 / {} 线程，{:.1} s；一元 {} 种，二元 {} 种",
        corpus.iter().filter(|&&c| c == b'\n').count(),
        nthreads,
        t0.elapsed().as_secs_f64(),
        uni.len(),
        bi.len()
    );

    let mut bigrams: Vec<(u64, u32)> = bi.into_iter().collect();
    bigrams.sort_unstable_by_key(|&(k, _)| k);

    let counts: Vec<u32> = bigrams.iter().map(|&(_, c)| c).collect();
    let thr_bg = count_threshold(&counts, min_bigram, top_bigram);
    let mut ctx: FxMap<u64, u32> = new_map();
    for (k, c) in bigrams.iter() {
        if *c >= thr_bg {
            ctx.insert(*k, ctx.len() as u32);
        }
    }
    println!(
        "二元：阈值 {thr_bg}，保留 {} / {} 种（{:.1} s）",
        ctx.len(),
        bigrams.len(),
        t0.elapsed().as_secs_f64()
    );

    // 第二遍：单线程扫一遍 id 流，把三元 key 全量收进 Vec，排序后游程压缩。
    //
    // 不用 HashMap：三元种类 1800 万、token 一亿，随机访问千万级表的实际成本
    // 是 2.4 μs/token（实测 245 s）。顺序 append + 排序是 8 字节顺序写 +
    // 一次 pdqsort，实测降到 1/5，而且结果与插入顺序无关，天然可复现。
    let t1 = std::time::Instant::now();
    let mut keys: Vec<u64> = Vec::with_capacity(1 << 27);
    for k in 0..nthreads {
        let Ok(data) = std::fs::read(tmp_dir.join(format!("ids.{k}"))) else {
            continue;
        };
        let mut a = SEP;
        let mut b = SEP;
        for chunk in data.chunks_exact(4) {
            let c = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if c == SEP {
                a = SEP;
                b = SEP;
                continue;
            }
            if a != SEP && b != SEP {
                if let Some(&id) = ctx.get(&(((a as u64) << 32) | b as u64)) {
                    keys.push(((id as u64) << 32) | c as u64);
                }
            }
            a = b;
            b = c;
        }
    }
    println!(
        "第二遍：{} 个三元 token，{:.1} s（收集）",
        keys.len(),
        t1.elapsed().as_secs_f64()
    );
    keys.sort_unstable();
    println!(
        "排序完成 {}，{:.1} s",
        keys.len(),
        t1.elapsed().as_secs_f64()
    );
    // 游程压缩：(key, count)
    let mut tris: Vec<(u64, u32)> = Vec::with_capacity(keys.len() / 4);
    for &k in &keys {
        match tris.last_mut() {
            Some((last, c)) if *last == k => *c += 1,
            _ => tris.push((k, 1)),
        }
    }
    drop(keys);
    println!(
        "第二遍：三元 {} 种，{:.1} s",
        tris.len(),
        t1.elapsed().as_secs_f64()
    );

    let tri_counts: Vec<u32> = tris.iter().map(|&(_, c)| c).collect();
    let thr_tg = count_threshold(&tri_counts, min_trigram, top_trigram);
    drop(tri_counts);

    let t2 = std::time::Instant::now();
    let model = build_model(&bigrams, &tris, val_count, thr_bg, thr_tg);
    println!(
        "平滑：一元 {}、二元 {}、三元 {} 条，{:.1} s",
        model.uni_p.len(),
        model.bg.len(),
        model.tg.len(),
        t2.elapsed().as_secs_f64()
    );
    verify(&model, 500);

    if stats_only {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return;
    }
    write_val_map(&out_map, &word_to_val).unwrap_or_else(|e| {
        eprintln!("写 {out_map}: {e}");
        std::process::exit(1)
    });
    write_ngram(&out_ngram, &model).unwrap_or_else(|e| {
        eprintln!("写 {out_ngram}: {e}");
        std::process::exit(1)
    });
    if let Some(p) = arpa_path.as_deref() {
        write_arpa(p, &model, &vals).unwrap_or_else(|e| {
            eprintln!("写 {p}: {e}");
            std::process::exit(1)
        });
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);
    println!("总耗时 {:.1} s", t0.elapsed().as_secs_f64());
}

fn split_bounds(corpus: &[u8], nthreads: usize) -> Vec<usize> {
    let mut bounds = Vec::with_capacity(nthreads + 1);
    bounds.push(0usize);
    for k in 1..nthreads {
        let mut p = corpus.len() * k / nthreads;
        while p < corpus.len() && corpus[p] != b'\n' {
            p += 1;
        }
        if p < corpus.len() {
            p += 1;
        }
        if p > *bounds.last().unwrap() {
            bounds.push(p);
        }
    }
    bounds.push(corpus.len());
    bounds
}
