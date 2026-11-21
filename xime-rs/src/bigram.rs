//! 二元语言模型转移表：`P(词|前词)` 的 `-log10` 代价，构建期烘焙进二进制。
//!
//! 数据由 `wayland-ime/tools/mkbigram.py` 生成（`data/bigram.bin`），格式：
//!   header(32) + (n_prev+1) 个 u32 offset + n_pairs 个 (u32 next_val_id, i16 cost)
//! `offset[prev]..offset[prev+1]` 是 `prev` 这一前词的 (next, cost) 列表，组内
//! next 升序，查询时二分。
//!
//! 查不到（或数据未生成）返回 0 = backoff 到 unigram，即不付转移代价，与未引入
//! bigram 时行为一致——因此数据文件缺失/为空时程序自然退化，不会出错。

use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../data/bigram.bin");

const MAGIC: &[u8; 8] = b"XIMEBG01";
const PAIR: usize = 6; // (u32 next, i16 cost)

#[inline]
fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

struct Bigram {
    offsets: &'static [u32],
    /// pairs 区原始字节，每 PAIR 字节一条 (next u32, cost i16)
    pairs: &'static [u8],
    val_count: u32,
}

fn data() -> &'static Bigram {
    static B: OnceLock<Bigram> = OnceLock::new();
    B.get_or_init(|| {
        let empty = Bigram { offsets: &[], pairs: &[], val_count: 0 };
        if RAW.len() < 32 || &RAW[0..8] != MAGIC {
            return empty;
        }
        let val_count = rd32(RAW, 8);
        let n_pairs = rd32(RAW, 12) as usize;
        let off_len = val_count as usize + 1;
        let off_start = 32;
        let pairs_start = off_start + off_len * 4;
        // 截断/溢出防御：文件不完整时整体退化
        if pairs_start.checked_add(n_pairs * PAIR).map_or(true, |e| e > RAW.len()) {
            return empty;
        }

        let mut offsets = Vec::with_capacity(off_len);
        for i in 0..off_len {
            offsets.push(rd32(RAW, off_start + i * 4));
        }
        let offsets: &'static [u32] = Box::leak(offsets.into_boxed_slice());
        let pairs = &RAW[pairs_start..pairs_start + n_pairs * PAIR];
        Bigram { offsets, pairs, val_count }
    })
}

/// 未命中的二元对（太罕见、没进 top N）返回的默认惩罚。
/// PMI 口径下，未命中 ≈ count 趋近 0，即「比整体罕见得多」，给一个固定正惩罚，
/// 避免罕见搭配因无统计而比「有统计的常见搭配」更占便宜（否则退化为 0 会反噬）。
const BACKOFF: f32 = 2.0;

/// 前词 val_id 为 `prev`、当前词 val_id 为 `next` 的转移代价（PMI 口径）。
/// 值为正 = 惩罚（next 在 prev 后比整体更少见）、为负 = 奖励（更常见）。
/// 查不到（未命中）返回 `BACKOFF`；prev 越界（句首/用户词表哨兵）返回 0。
#[inline]
pub fn trans_cost(prev: u32, next: u32) -> f32 {
    let b = data();
    if prev >= b.val_count || b.offsets.is_empty() {
        return 0.0;
    }
    let lo = b.offsets[prev as usize] as usize;
    let hi = b.offsets[prev as usize + 1] as usize;
    // 组内二分 next（每 PAIR 字节一条，next 在前 4 字节）
    let (mut l, mut r) = (lo, hi);
    while l < r {
        let m = (l + r) / 2;
        if rd32(b.pairs, m * PAIR) < next {
            l = m + 1;
        } else {
            r = m;
        }
    }
    if l < hi && rd32(b.pairs, l * PAIR) == next {
        let c = i16::from_le_bytes([b.pairs[l * PAIR + 4], b.pairs[l * PAIR + 5]]) as f32;
        c / 100.0
    } else {
        BACKOFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_prev_degrades_to_zero() {
        // 前词越界（句首/用户词表哨兵 u32::MAX）不查转移，返回 0
        assert_eq!(trans_cost(u32::MAX, u32::MAX), 0.0);
        assert_eq!(trans_cost(u32::MAX, 0), 0.0);
    }
}
