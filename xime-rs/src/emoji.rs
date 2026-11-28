//! 彩色 emoji：词表检索 + ARGB 图集。
//!
//! 正文走 `font.rs` 的 8bpp alpha 图集，emoji 走这里单独的 32bpp 彩色图集：
//! emoji 字形是 CBDT 位图（NotoColorEmoji），颜色信息压不进 alpha 掩码，
//! 只能原样存成 ARGB 再按预乘直接合成。
//!
//! 两份数据都由构建期工具生成，运行时零依赖：
//! - `data/emoji.bin`      `tools/mkemoji.c` 用 FreeType 烘焙
//! - `data/emoji-keys.bin` `scripts/gen-emoji.py` 生成的字母序键索引
//!
//! 词表检索按「前缀」做：拼音键（笑脸 -> xiaolian）与英文名（fire）在同一张表里，
//! 都是小写 ASCII，所以一次二分就能定位，再往后线性收同前缀的条目。

use std::sync::OnceLock;

static ATLAS_RAW: &[u8] = include_bytes!("../data/emoji.bin");
static KEYS_RAW: &[u8] = include_bytes!("../data/emoji-keys.bin");

const ATLAS_MAGIC: &[u8; 8] = b"XIMEEJ01";
const KEYS_MAGIC: &[u8; 8] = b"XIMEEK01";
const ATLAS_HDR: usize = 32;
const GLYPH_SIZE: usize = 8;
const KEY_SIZE: usize = 8;

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

// ---------------------------------------------------------------------------
// 图集
// ---------------------------------------------------------------------------

pub struct Glyphs {
    /// 格子边长（正方形，24 表示 24×24）
    pub cell: i32,
    cols: i32,
    glyphs: &'static [u8],
    pixels: &'static [u8],
    /// 一行像素的字节数。图集是一整张大图，跨过一格要继续走完整行宽
    stride: usize,
}

impl Glyphs {
    fn parse() -> Glyphs {
        assert!(ATLAS_RAW.len() > ATLAS_HDR, "emoji 图集被截断");
        assert_eq!(&ATLAS_RAW[0..8], ATLAS_MAGIC, "emoji 图集标记不匹配，请重新跑 make emoji");

        let cell = rd32(ATLAS_RAW, 8) as i32;
        let cols = rd32(ATLAS_RAW, 16) as i32;
        let n = rd32(ATLAS_RAW, 28) as usize;
        let goff = ATLAS_HDR;
        let poff = goff + n * GLYPH_SIZE;

        Glyphs {
            cell,
            cols: cols.max(1),
            glyphs: &ATLAS_RAW[goff..poff],
            pixels: &ATLAS_RAW[poff..],
            stride: cols as usize * cell as usize * 4,
        }
    }

    pub fn get() -> &'static Glyphs {
        static G: OnceLock<Glyphs> = OnceLock::new();
        G.get_or_init(Glyphs::parse)
    }

    fn count(&self) -> usize {
        self.glyphs.len() / GLYPH_SIZE
    }

    /// 二分查码点。索引按码点升序生成（与 font.rs 同一套约定）。
    pub fn lookup(&self, cp: u32) -> Option<(u16, u16)> {
        let (mut lo, mut hi) = (0i32, self.count() as i32 - 1);
        while lo <= hi {
            let mid = (lo + hi) / 2;
            let base = mid as usize * GLYPH_SIZE;
            let gcp = rd32(self.glyphs, base);
            if gcp == cp {
                return Some((rd16(self.glyphs, base + 4), rd16(self.glyphs, base + 6)));
            }
            if gcp < cp {
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        None
    }

    /// 取格子内 (x, y) 处的像素，已是预乘 ARGB8888，可直接交给 `blend_into`
    pub fn pixel(&self, cell: u16, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.cell || y >= self.cell {
            return 0;
        }
        let cx = (cell as i32 % self.cols) * self.cell;
        let cy = (cell as i32 / self.cols) * self.cell;
        let off = (cy + y) as usize * self.stride + (cx + x) as usize * 4;
        u32::from_le_bytes([
            self.pixels[off],
            self.pixels[off + 1],
            self.pixels[off + 2],
            self.pixels[off + 3],
        ])
    }
}

// ---------------------------------------------------------------------------
// 键索引
// ---------------------------------------------------------------------------

pub struct Keys {
    n: usize,
    tbl: &'static [u8],
    blob: &'static [u8],
}

impl Keys {
    fn parse() -> Keys {
        assert!(KEYS_RAW.len() > ATLAS_HDR, "emoji 键索引被截断");
        assert_eq!(&KEYS_RAW[0..8], KEYS_MAGIC, "emoji 键索引标记不匹配");

        let n = rd32(KEYS_RAW, 8) as usize;
        let blen = rd32(KEYS_RAW, 12) as usize;
        let toff = 32;
        let boff = toff + n * KEY_SIZE;

        Keys {
            n,
            tbl: &KEYS_RAW[toff..toff + n * KEY_SIZE],
            blob: &KEYS_RAW[boff..boff + blen],
        }
    }

    pub fn get() -> &'static Keys {
        static K: OnceLock<Keys> = OnceLock::new();
        K.get_or_init(Keys::parse)
    }

    fn off_at(&self, i: usize) -> usize {
        rd32(self.tbl, i * KEY_SIZE) as usize
    }

    fn cp_at(&self, i: usize) -> u32 {
        rd32(self.tbl, i * KEY_SIZE + 4)
    }

    /// 第 i 条的键。blob 里是 NUL 结尾的 ASCII，直接当 &str 用。
    fn key_at(&self, i: usize) -> &'static str {
        let o = self.off_at(i);
        let end = self.blob[o..].iter().position(|&b| b == 0).map_or(self.blob.len(), |p| o + p);
        // blob 是 ASCII 且内容来自我们自己的生成器，越界即数据损坏
        std::str::from_utf8(&self.blob[o..end]).unwrap_or("")
    }

    /// 前缀检索：返回命中的 emoji 字符，按词表顺序、去重、最多 max 个。
    ///
    /// 键表按字母序排，所以先二分到第一个 >= prefix 的位置，再往后收同前缀项。
    /// 一个 emoji 常有多条键（face / grinning / grinningface），命中多次只留第一次。
    pub fn search(&self, prefix: &str, max: usize) -> Vec<char> {
        if prefix.is_empty() || max == 0 {
            return Vec::new();
        }
        let (mut lo, mut hi) = (0usize, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key_at(mid) < prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        let mut out: Vec<char> = Vec::with_capacity(max);
        let mut i = lo;
        while i < self.n && out.len() < max {
            let k = self.key_at(i);
            if !k.starts_with(prefix) {
                break;
            }
            if let Some(ch) = char::from_u32(self.cp_at(i)) {
                if !out.contains(&ch) {
                    out.push(ch);
                }
            }
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_parses_and_has_colour() {
        let g = Glyphs::get();
        assert_eq!(g.cell, 24);
        let c = g.lookup(0x1F600).expect("图集里应有 😀");
        // 中间偏上的像素通常落在脸上，应当有颜色而不是全透明
        let mut opaque = 0;
        for y in 0..g.cell {
            for x in 0..g.cell {
                if g.pixel(c.0, x, y) >> 24 != 0 {
                    opaque += 1;
                }
            }
        }
        assert!(opaque > 100, "😀 的不透明像素太少: {opaque}");
    }

    #[test]
    fn pinyin_key_finds_emoji() {
        let v = Keys::get().search("xiaolian", 4);
        assert!(!v.is_empty(), "xiaolian 应能命中笑脸类 emoji");
    }

    #[test]
    fn english_key_finds_emoji() {
        let v = Keys::get().search("fire", 8);
        assert!(v.contains(&'🔥'), "fire 应命中 🔥: {v:?}");
    }

    #[test]
    fn prefix_match_is_bounded_and_deduped() {
        let v = Keys::get().search("grin", 6);
        assert!(v.len() <= 6);
        let mut d = v.clone();
        d.dedup();
        assert_eq!(v.len(), d.len(), "候选出现重复: {v:?}");
    }

    #[test]
    fn empty_or_unknown_input_yields_nothing() {
        assert!(Keys::get().search("", 4).is_empty());
        assert!(Keys::get().search("zzzzqqq", 4).is_empty());
    }
}
