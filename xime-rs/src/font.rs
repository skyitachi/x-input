//! 字形图集：构建期由 `tools/mkfont` 把「词库字符集 + ASCII」光栅化成 8bpp alpha 图集，
//! 运行时零字体依赖——不加载 TTF、不依赖 freetype/fontconfig。
//!
//! 二进制布局：magic(8) + 6×u32 头 + glyph_count×8（cp, cell, adv）+ 图集像素。
//! 图集本身是一张 cols×cell_w 宽的大图，格子按行优先排列。

use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../data/atlas.bin");

const MAGIC: &[u8; 8] = b"XIMEAT01";
const HDR: usize = 32;
const GLYPH_SIZE: usize = 8;

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

pub struct Atlas {
    pub cell_w: i32,
    pub cell_h: i32,
    pub cols: i32,
    pub baseline: i32,
    pub font_px: i32,
    glyphs: &'static [u8],
    pixels: &'static [u8],
    pixel_stride: usize,
}

impl Atlas {
    fn parse() -> Atlas {
        assert!(RAW.len() > HDR, "图集文件被截断");
        assert_eq!(&RAW[0..8], MAGIC, "图集文件标记不匹配，请重新执行 make font");

        let cell_w = rd32(RAW, 8) as i32;
        let cell_h = rd32(RAW, 12) as i32;
        let cols = rd32(RAW, 16) as i32;
        let baseline = rd32(RAW, 20) as i32;
        let font_px = rd32(RAW, 24) as i32;
        let n = rd32(RAW, 28) as usize;

        let goff = HDR;
        let glyphs = &RAW[goff..goff + n * GLYPH_SIZE];
        let pixels = &RAW[goff + n * GLYPH_SIZE..];

        Atlas {
            cell_w,
            cell_h,
            cols,
            baseline,
            font_px,
            glyphs,
            pixels,
            pixel_stride: (cols * cell_w) as usize,
        }
    }

    pub fn get() -> &'static Atlas {
        static A: OnceLock<Atlas> = OnceLock::new();
        A.get_or_init(Atlas::parse)
    }

    pub fn count(&self) -> usize {
        self.glyphs.len() / GLYPH_SIZE
    }

    /// 二分查找码点。glyph 表按码点升序生成。
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

    pub fn advance(&self, cp: u32) -> i32 {
        if let Some((_, adv)) = self.lookup(cp) {
            return adv as i32;
        }
        if cp == ' ' as u32 {
            return self.font_px / 2;
        }
        self.font_px / 2 + 2
    }

    /// 取某格的一行像素（cell 内的第 gy 行），长度 cell_w
    pub fn glyph_row(&self, cell: u16, gy: i32) -> &'static [u8] {
        let cx = (cell as i32 % self.cols) * self.cell_w;
        let cy = (cell as i32 / self.cols) * self.cell_h + gy;
        let off = cy as usize * self.pixel_stride + cx as usize;
        &self.pixels[off..off + self.cell_w as usize]
    }
}
