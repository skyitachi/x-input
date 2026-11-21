//! 词库：构建期烘焙的二进制静态词表 + 运行期可选用户词表。
//!
//! `data/dict.bin` 由 `tools/mkdict.py --bin` 生成，通过 include_bytes! 直接映射进
//! 只读段，运行期零解析、零拷贝：所有条目按固定宽度排布，查表时按偏移切片即可。
//!
//! 查表方式与 C 版一致：词条按拼音字节序排列，同音词按词频降序连续存放。
//! 因此「精确查」是二分定位后顺扫同前缀区间，「前缀查」复用同一段区间。

use crate::zh::{self, Variant};
use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../data/dict.bin");
/// 词条 id -> 词形 id（val_id）映射，供 bigram 转移表按「词形」查转移代价。
static VAL_RAW: &[u8] = include_bytes!("../data/val_map.bin");

const MAGIC: &[u8; 8] = b"XIMEIM01";
const VAL_MAGIC: &[u8; 8] = b"XIMEVM01";
const HDR: usize = 32;
const WORD_SIZE: usize = 12;
const INI_SIZE: usize = 8;
/// 用户词表词频固定高于内置词，保证自造词/纠错优先命中
const USER_FREQ: u32 = 1_000_000;

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// 从 blob 的某个偏移读出以 NUL 结尾的字符串。
/// 数据是自己生成的，UTF-8 必然合法，故用 unchecked 省掉逐条校验开销。
fn cstr_at(b: &'static [u8], off: usize) -> &'static str {
    let tail = &b[off..];
    let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
    unsafe { std::str::from_utf8_unchecked(&tail[..end]) }
}

pub struct Tables {
    words: &'static [u8],
    blob: &'static [u8],
    ini: &'static [u8],
    ini_blob: &'static [u8],
    _syl: &'static [u8],
    pub word_count: usize,
    pub ini_count: usize,
    /// 词条 id -> val_id（词形 id）。数据未生成时为 empty，val_id() 返回哨兵。
    val_map: &'static [u32],
}

impl Tables {
    fn parse() -> Tables {
        assert!(RAW.len() > HDR, "词库文件被截断");
        assert_eq!(&RAW[0..8], MAGIC, "词库文件标记不匹配，请重新执行 make dict");

        let wc = rd32(RAW, 8) as usize;
        let bl = rd32(RAW, 12) as usize;
        let ic = rd32(RAW, 16) as usize;
        let ibl = rd32(RAW, 20) as usize;
        let sl = rd32(RAW, 24) as usize;

        let mut off = HDR;
        let words = &RAW[off..off + wc * WORD_SIZE];
        off += wc * WORD_SIZE;
        let blob = &RAW[off..off + bl];
        off += bl;
        let ini = &RAW[off..off + ic * INI_SIZE];
        off += ic * INI_SIZE;
        let ini_blob = &RAW[off..off + ibl];
        off += ibl;
        let syl = &RAW[off..off + sl];

        Tables {
            words,
            blob,
            ini,
            ini_blob,
            _syl: syl,
            word_count: wc,
            ini_count: ic,
            val_map: parse_val_map(),
        }
    }

    #[inline]
    fn key_off(&self, i: usize) -> u32 {
        rd32(self.words, i * WORD_SIZE)
    }

    #[inline]
    fn val_off(&self, i: usize) -> u32 {
        rd32(self.words, i * WORD_SIZE + 4)
    }

    #[inline]
    fn freq(&self, i: usize) -> u32 {
        rd32(self.words, i * WORD_SIZE + 8)
    }

    #[inline]
    fn key(&self, i: usize) -> &'static str {
        cstr_at(self.blob, self.key_off(i) as usize)
    }

    /// key 的字节长度。blob 内 key 与 value 紧邻存放，无需扫描字符串。
    #[inline]
    fn key_len(&self, i: usize) -> usize {
        (self.val_off(i) - self.key_off(i) - 1) as usize
    }

    /// key 是否以 p 开头（不依赖 NUL 扫描后的整体比较，只比前 p.len() 字节）
    #[inline]
    fn starts_with(&self, i: usize, p: &str) -> bool {
        let k = self.key(i);
        k.len() >= p.len() && k.as_bytes()[..p.len()] == *p.as_bytes()
    }

    #[inline]
    fn val(&self, i: usize) -> &'static str {
        cstr_at(self.blob, self.val_off(i) as usize)
    }

    /// 二分定位第一个「以 p 为前缀」的条目
    fn lower_bound(&self, p: &str) -> usize {
        let pb = p.as_bytes();
        let (mut lo, mut hi) = (0usize, self.word_count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if cmp_prefix(self.key(mid), pb) < 0 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn lower_bound_ini(&self, p: &str) -> usize {
        let pb = p.as_bytes();
        let (mut lo, mut hi) = (0usize, self.ini_count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let k = cstr_at(self.ini_blob, rd32(self.ini, mid * INI_SIZE + 4) as usize);
            if cmp_prefix(k, pb) < 0 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// 解析 val_map.bin：词条 id -> val_id。文件缺失/不完整时返回空 slice，
/// 此时 val_id() 对任意 word_id 返回哨兵 u32::MAX（无转移，退化回 unigram）。
fn parse_val_map() -> &'static [u32] {
    if VAL_RAW.len() < 32 || &VAL_RAW[0..8] != VAL_MAGIC {
        return &[];
    }
    let n = rd32(VAL_RAW, 8) as usize;
    if 32 + n * 4 > VAL_RAW.len() {
        return &[];
    }
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(rd32(VAL_RAW, 32 + i * 4));
    }
    Box::leak(v.into_boxed_slice())
}

/// 比较 key 与查询串 p：0 表示 key 以 p 开头；<0 表示 key 字典序更小
#[inline]
fn cmp_prefix(key: &str, p: &[u8]) -> i32 {
    let kb = key.as_bytes();
    for (i, &b) in p.iter().enumerate() {
        let a = kb.get(i).copied().unwrap_or(0);
        if a != b {
            return a as i32 - b as i32;
        }
        if a == 0 {
            return 0;
        }
    }
    0
}

/// 一条候选
#[derive(Clone, Copy)]
pub struct Hit {
    pub val: &'static str,
    pub freq: u32,
    pub key_len: usize,
    /// 当前输出形态下是否可见（简体模式下隐藏繁体形词条）
    pub keep: bool,
    /// 词形 id（val_id），供 bigram 转移表查转移代价；用户词表条目为哨兵 u32::MAX
    pub val_id: u32,
}

/// 保持按词频降序、按候选串去重、最多 cap 条
///
/// 词库源数据里 `變成`/`变成` 同时存在且繁体那条词频更高，不过滤就会在候选窗里
/// 简繁混排——所以简体模式下这些问题条目在进候选前就被丢掉了。
fn push_hit(out: &mut Vec<Hit>, cap: usize, h: Hit) {
    if !h.keep {
        return;
    }
    if out.iter().any(|x| x.val == h.val) {
        return;
    }
    if out.len() < cap {
        out.push(h);
    } else if h.freq <= out.last().map(|x| x.freq).unwrap_or(0) {
        return;
    } else {
        out.pop();
        out.push(h);
    }
    // 插入排序：cap 很小（≤4），比 std sort 更划算
    let mut i = out.len() - 1;
    while i > 0 && out[i].freq > out[i - 1].freq {
        out.swap(i, i - 1);
        i -= 1;
    }
}

// ---------------------------------------------------------------- 对外接口

struct UserWord {
    key: &'static str,
    val: &'static str,
    freq: u32,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(Tables::parse)
}

fn users() -> &'static Vec<UserWord> {
    static U: OnceLock<Vec<UserWord>> = OnceLock::new();
    U.get_or_init(|| {
        let path = std::env::var("XIME_USER_DICT")
            .unwrap_or_else(|_| "/usr/share/xime/dict.txt".to_string());
        load_user_dict(&path)
    })
}

/// 用户词表：每行「拼音<TAB>候选」。用于自造词与纠错，优先级高于内置词。
fn load_user_dict(path: &str) -> Vec<UserWord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('\t') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || v.is_empty() || !k.bytes().all(|b| b.is_ascii_lowercase()) {
            continue;
        }
        // 词表在进程生命周期内不变，leak 成 'static 以统一候选串的生命周期
        let k: &'static str = Box::leak(k.to_string().into_boxed_str());
        let v: &'static str = Box::leak(v.to_string().into_boxed_str());
        out.push(UserWord { key: k, val: v, freq: USER_FREQ });
    }
    out
}

/// 加载内置词库并合并用户词表，返回内置词条数
pub fn init() -> usize {
    let n = tables().word_count;
    users();
    n
}

pub fn word_count() -> usize {
    tables().word_count
}

/// 精确查表（复用调用方缓冲，避免热路径分配）
///
/// 这里有个容易踩的性能坑：如果按「前缀匹配」扫描，查 "n" 这种单字母键时
/// 会线性扫掉所有以 n 开头的词条（几千条），实测 34μs/次——因为拼音是逐键输入的，
/// 这个开销会被放大几百倍。
///
/// 但词条按拼音字节序排列，同一个 key 的条目是**连续**的，且长度可以从偏移量
/// 直接算出来（`val_off - key_off - 1`）。于是只需二分定位后检查：
/// 长度不等于查询串 → key 必然不是 p，立即结束。
pub fn exact_into(p: &str, cap: usize, variant: Variant, out: &mut Vec<Hit>) {
    out.clear();
    let t = tables();
    let plen = p.len();
    let mut i = t.lower_bound(p);
    if i < t.word_count && t.key_len(i) == plen && t.key(i) == p {
        while i < t.word_count && t.key_len(i) == plen {
            let k = t.key(i);
            if k != p {
                break;
            }
            push_hit(
                out,
                cap,
                Hit {
                    val: t.val(i),
                    freq: t.freq(i),
                    key_len: plen,
                    keep: zh::keep_entry(i, variant),
                    val_id: t.val_map.get(i).copied().unwrap_or(u32::MAX),
                },
            );
            if out.len() == cap {
                break;
            }
            i += 1;
        }
    }
    for u in users() {
        if u.key == p {
            // 用户词表由用户自己写，不做简繁分类
            push_hit(out, cap, Hit { val: u.val, freq: u.freq, key_len: plen, keep: true, val_id: u32::MAX });
        }
    }
}

/// 前缀查表（复用调用方缓冲）
pub fn prefix_into(p: &str, max_extra: usize, cap: usize, variant: Variant, out: &mut Vec<Hit>) {
    out.clear();
    let t = tables();
    let plen = p.len();
    let max_len = plen + max_extra;
    let mut i = t.lower_bound(p);
    while i < t.word_count {
        let kl = t.key_len(i);
        // 长度不在补全范围内：只需确认是否已越过「以 p 开头」的区间
        if kl <= plen || kl > max_len {
            if !t.starts_with(i, p) {
                break;
            }
            i += 1;
            continue;
        }
        if !t.starts_with(i, p) {
            break;
        }
        push_hit(
            out,
            cap,
            Hit {
                val: t.val(i),
                freq: t.freq(i),
                key_len: kl,
                keep: zh::keep_entry(i, variant),
                val_id: t.val_map.get(i).copied().unwrap_or(u32::MAX),
            },
        );
        i += 1;
    }
    for u in users() {
        let kl = u.key.len();
        if kl > p.len() && kl - p.len() <= max_extra && u.key.starts_with(p) {
            push_hit(out, cap, Hit { val: u.val, freq: u.freq, key_len: kl, keep: true, val_id: u32::MAX });
        }
    }
}

/// 简拼（复用调用方缓冲）
pub fn initials_into(s: &str, cap: usize, variant: Variant, out: &mut Vec<Hit>) {
    out.clear();
    let t = tables();
    let mut i = t.lower_bound_ini(s);
    while i < t.ini_count {
        let off = rd32(t.ini, i * INI_SIZE + 4) as usize;
        let k = cstr_at(t.ini_blob, off);
        if cmp_prefix(k, s.as_bytes()) != 0 {
            break;
        }
        if k.len() == s.len() {
            let wid = rd32(t.ini, i * INI_SIZE) as usize;
            if wid < t.word_count {
                push_hit(
                    out,
                    cap,
                    Hit {
                        val: t.val(wid),
                        freq: t.freq(wid),
                        key_len: s.len(),
                        keep: zh::keep_entry(wid, variant),
                        val_id: t.val_map.get(wid).copied().unwrap_or(u32::MAX),
                    },
                );
            }
        }
        i += 1;
    }
}

