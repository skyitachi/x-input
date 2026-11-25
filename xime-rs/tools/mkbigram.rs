//! 用词库贪心切分语料，统计词形 bigram，输出 `val_map.bin` 与 `bigram.bin`。
//!
//! > **已被 `tools/mkngram.rs` 取代**：`xime-rs` 的语言模型已升级到三阶
//! > （modified Kneser-Ney + ARPA 回退链），`scripts/gen-data.sh` 不再调用本工具，
//! > `data/bigram.bin` 也已从仓库移除。保留它的理由有两条：重建二元基线做 A/B 对比
//! > （二元 vs 三元到底带来多少收益），以及作为 `wayland-ime/tools/mkbigram.py` 的
//! > 互验参照——两者不限 `--top` 时逐字节相同。新功能不要往这里加。
//!
//! 这是 `wayland-ime/tools/mkbigram.py` 的 Rust 实现，输出格式与语义逐字节一致，
//! 区别只在速度与内存：
//!
//! | | Python 版 | 本版 |
//! |---|---|---|
//! | 切分 | 每位置切片出 `str` 再查 dict（每次命中都分配） | `&str` 零拷贝查表 |
//! | 计数 | `Counter[(p, n)]` 元组 | `HashMap<u64, u32>`，key 是 (prev<<32)\|next |
//! | 并行 | `Pool.starmap` 把每个分块的行列表 pickle 进管道 | 线程共享只读语料，分块按字节偏移，零序列化 |
//! | 语料 | 整个文件读成 `lines` 列表常驻 | 一次读入 `Vec<u8>`，线程各自切 `&str` |
//!
//! 编译（零依赖，不需要 Cargo）：
//! ```sh
//! rustc -O -o tools/mkbigram tools/mkbigram.rs
//! ```
//!
//! 用法与 Python 版一致：
//! ```sh
//! tools/mkbigram --dict ../xime-rs/data/dict.bin --corpus corpus.txt \
//!     --out-map ../xime-rs/data/val_map.bin --out-bigram ../xime-rs/data/bigram.bin \
//!     [--top 1000000] [--max-lines N]
//! ```
//!
//! 文件格式见 `wayland-ime/tools/mkbigram.py` 顶部注释：那边是格式的权威文档，
//! 本文件只复现它。

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Write};

const MAGIC_DICT: &[u8; 8] = b"XIMEIM01";
const MAGIC_MAP: &[u8; 8] = b"XIMEVM01";
const MAGIC_BG: &[u8; 8] = b"XIMEBG01";

/// 切分时尝试的最大词长。与 mkdict.py 收录的上限（6 字）一致；
/// 词库里有更长的词也不会被匹配到——Python 版同样写死 6。
const MAX_WORD_CHARS: usize = 6;

// ---------------------------------------------------------------- 哈希

/// FxHash（rustc 内部用的那套）：比默认 SipHash 快数倍，且本工具的数据全部来自
/// 本地构建产物，没有抗碰撞需求。零依赖，所以自己写十几行。
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
        // 长度参与散列：否则 "ab" 与 "ab\0" 在补零后同键
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

/// 按词形最高词频降序分配 val_id：高频词形 id 小，转移表的下标更紧凑。
///
/// 同一词形的多条词条（多音）取最高词频作排序依据。排序必须**稳定**且按词形
/// 首次出现顺序打平局——Python 版的 `sorted(dict.keys())` 就是这个行为，
/// 不这样做两次构建的 val_id 顺序会漂，`bigram.bin` 跟着整体错位。
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

// ---------------------------------------------------------------- 统计

type Counts = (FxMap<u64, u32>, FxMap<u32, u32>, FxMap<u32, u32>);

/// 一个 UTF-8 字符占几个字节（只看首字节）。
#[inline]
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 对一段语料做贪心最长匹配并统计转移。
///
/// 纯字节扫描 + `&str` 切片查表，全程零分配；token 之间不留中间数组，命中即
/// 累加 (prev, cur)。**不在词库的字符（标点、数字、生僻字）会断开转移链**：
/// 语料是按句切的，句内仍常有逗号、顿号、括号，不断开就会把「模型，带来」
/// 统计成 模型→带，让「带模型」凭一条跨标点伪搭配压过真词。
fn count_chunk(text: &str, val: &FxMap<&str, u32>) -> Counts {
    let mut pairs: FxMap<u64, u32> = new_map();
    let mut prev_count: FxMap<u32, u32> = new_map();
    let mut unigram: FxMap<u32, u32> = new_map();
    // 三个表的条目量级差很远，先给个粗估的容量，省掉几轮 rehash
    pairs.reserve(1 << 16);
    unigram.reserve(1 << 14);
    prev_count.reserve(1 << 14);

    for line in text.lines() {
        let b = line.as_bytes();
        let mut i = 0usize;
        let mut prev: Option<u32> = None;
        while i < b.len() {
            // 先量出从 i 起最多 6 个字符的边界，再从长到短试
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
                    *unigram.entry(id).or_insert(0) += 1;
                    if let Some(p) = prev {
                        *pairs
                            .entry(((p as u64) << 32) | id as u64)
                            .or_insert(0) += 1;
                        *prev_count.entry(p).or_insert(0) += 1;
                    }
                    prev = Some(id);
                    i = end;
                }
                // 不在词库的字符（标点、数字、生僻字）：跳过并**断开转移链**。
                // 不断开的话「…模型，带来…」会被统计成 模型→带 的一个高频伪搭配，
                // 解码时「带模型」就会因为这条跨标点转移的奖励压过「大模型」。
                // 断句信息在这里比「多统计几条」重要。
                None => {
                    prev = None;
                    i += utf8_len(b[i]);
                }
            }
        }
    }
    (pairs, prev_count, unigram)
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

// ---------------------------------------------------------------- 输出

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

/// 取 top N 个二元对，按 PMI 折算成 i16 代价，按 prev 分组、组内 next 升序写出。
///
/// 代价用 PMI 而非条件概率：`cost = -log10(P(next|prev)/P(next))`。条件概率口径下
/// 高频 prev（「是」）会把所有 next 稀释，合理搭配也背上惩罚；PMI 只看这个搭配
/// 比整体更常见还是更少见，常见搭配为负（奖励）。口径与 Python 版一致。
fn write_bigram(
    path: &str,
    val_count: u32,
    mut items: Vec<(u64, u32)>,
    prev_count: &FxMap<u32, u32>,
    unigram: &FxMap<u32, u32>,
    top: usize,
) -> std::io::Result<()> {
    if top > 0 && items.len() > top {
        // 只切出前 top 个，不做全排序（全量千万级时差一个数量级）
        items.select_nth_unstable_by(top, |a, b| b.1.cmp(&a.1));
        items.truncate(top);
    }
    items.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let total: u64 = unigram.values().map(|&v| v as u64).sum::<u64>().max(1);
    let mut groups: Vec<Vec<(u32, i16)>> = vec![Vec::new(); val_count as usize];
    for (k, c) in &items {
        let p = (k >> 32) as u32;
        let n = (*k & 0xFFFF_FFFF) as u32;
        let pc = *prev_count.get(&p).unwrap_or(&1) as f64;
        let uc = *unigram.get(&n).unwrap_or(&1) as f64;
        let p_cond = *c as f64 / pc;
        let p_uni = uc / total as f64;
        // round_ties_even 对齐 Python 的 round()（银行家舍入），否则 ±1 的代价差
        // 会让同一个语料两次构建产出不同的字节
        let cost = ((-p_cond.log10() + p_uni.log10()) * 100.0)
            .round_ties_even()
            .clamp(-32768.0, 32767.0) as i16;
        groups[p as usize].push((n, cost));
    }

    let mut offsets = vec![0u32; val_count as usize + 1];
    let mut running = 0u32;
    for prev in 0..val_count as usize {
        offsets[prev] = running;
        running += groups[prev].len() as u32;
    }
    offsets[val_count as usize] = running;

    let mut f = BufWriter::new(std::fs::File::create(path)?);
    f.write_all(MAGIC_BG)?;
    f.write_all(&val_count.to_le_bytes())?;
    f.write_all(&(items.len() as u32).to_le_bytes())?;
    f.write_all(&[0u8; 16])?;
    let mut buf = Vec::with_capacity(offsets.len() * 4);
    for o in &offsets {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    f.write_all(&buf)?;

    let mut buf = Vec::with_capacity(items.len() * 6);
    for g in groups.iter_mut() {
        g.sort_unstable_by_key(|&(n, _)| n);
        for &(n, cost) in g.iter() {
            buf.extend_from_slice(&n.to_le_bytes());
            buf.extend_from_slice(&cost.to_le_bytes());
        }
    }
    f.write_all(&buf)?;
    f.flush()?;
    println!("生成 {path}（{} 二元对 / {val_count} 词形）", items.len());
    Ok(())
}

// ---------------------------------------------------------------- 主流程

fn main() {
    let mut dict_path = String::from("../xime-rs/data/dict.bin");
    let mut corpus_path: Option<String> = None;
    let mut out_map = String::from("../xime-rs/data/val_map.bin");
    let mut out_bigram = String::from("../xime-rs/data/bigram.bin");
    let mut top: usize = 1_000_000;
    let mut max_lines: usize = 0;

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
            "--out-bigram" => out_bigram = val("--out-bigram"),
            "--top" => top = val("--top").parse().unwrap_or(1_000_000),
            "--max-lines" => max_lines = val("--max-lines").parse().unwrap_or(0),
            "-h" | "--help" => {
                println!(
                    "用法: mkbigram --corpus <file> [--dict <dict.bin>]\n\
                     \x20                [--out-map <val_map.bin>] [--out-bigram <bigram.bin>]\n\
                     \x20                [--top N] [--max-lines N]"
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
    println!("词条 {}，词形 {}", dict.keys.len(), vals.len());

    let mut corpus = std::fs::read(&corpus_path).unwrap_or_else(|e| {
        eprintln!("读不了 {corpus_path}: {e}");
        std::process::exit(1)
    });

    // --max-lines：截断到第 N 行末尾（调试用；不能按字节截，否则末行可能半个词）
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

    // 按字节均分，边界推到下一个换行之后。线程直接借用同一份只读语料，
    // 不需要像 Python 那样把行列表 pickle 进管道。
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(12)
        .max(1);
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

    let t0 = std::time::Instant::now();
    let chunks: Vec<&str> = bounds
        .windows(2)
        .map(|w| std::str::from_utf8(&corpus[w[0]..w[1]]).expect("语料不是合法 UTF-8"))
        .collect();

    let results: Vec<Counts> = std::thread::scope(|s| {
        let val = &val_to_id;
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| s.spawn(move || count_chunk(chunk, val)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut pairs: FxMap<u64, u32> = new_map();
    let mut prev_count: FxMap<u32, u32> = new_map();
    let mut unigram: FxMap<u32, u32> = new_map();
    for (p, c, u) in results {
        merge_into(&mut pairs, p);
        merge_small(&mut prev_count, c);
        merge_small(&mut unigram, u);
    }
    println!(
        "切分 + 统计 {} 行，{} 线程，{:.1} s",
        corpus.iter().filter(|&&c| c == b'\n').count(),
        chunks.len(),
        t0.elapsed().as_secs_f64()
    );
    println!("二元对 {} 种，前词计数 {} 种", pairs.len(), prev_count.len());

    write_val_map(&out_map, &word_to_val).unwrap_or_else(|e| {
        eprintln!("写 {out_map}: {e}");
        std::process::exit(1)
    });
    let items: Vec<(u64, u32)> = pairs.into_iter().collect();
    write_bigram(&out_bigram, vals.len() as u32, items, &prev_count, &unigram, top)
        .unwrap_or_else(|e| {
            eprintln!("写 {out_bigram}: {e}");
            std::process::exit(1)
        });
}
