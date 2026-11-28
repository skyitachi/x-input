//! 软光栅渲染：直接往 ARGB8888 缓冲写像素，该缓冲可原样交给 wl_shm。
//!
//! 只有圆角矩形和字形 alpha 叠加两种操作，所以不引入任何 2D 库——
//! 这也是"运行时零依赖"的一部分。

use crate::emoji::Glyphs;
use crate::font::Atlas;

pub struct Canvas {
    pub px: Vec<u32>,
    pub w: i32,
    pub h: i32,
}

/// 预乘 alpha 的颜色
pub fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    let ar = r as u32 * a as u32 / 255;
    let ag = g as u32 * a as u32 / 255;
    let ab = b as u32 * a as u32 / 255;
    ((a as u32) << 24) | (ar << 16) | (ag << 8) | ab
}

#[inline]
fn blend_into(dst: &mut u32, src_pm: u32, ga: u32) {
    let sa = ((src_pm >> 24) & 0xFF) * ga / 255;
    let sr = ((src_pm >> 16) & 0xFF) * ga / 255;
    let sg = ((src_pm >> 8) & 0xFF) * ga / 255;
    let sb = (src_pm & 0xFF) * ga / 255;
    let ia = 255 - sa;
    let dr = (*dst >> 16) & 0xFF;
    let dg = (*dst >> 8) & 0xFF;
    let db = *dst & 0xFF;
    let r = sr + dr * ia / 255;
    let g = sg + dg * ia / 255;
    let b = sb + db * ia / 255;
    *dst = (sa.min(255) << 24) | (r << 16) | (g << 8) | b;
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Canvas {
        Canvas { px: vec![0; (w.max(1) * h.max(1)) as usize], w: w.max(1), h: h.max(1) }
    }

    pub fn clear(&mut self, color: u32) {
        self.px.iter_mut().for_each(|p| *p = color);
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for yy in y..y + h {
            if yy < 0 || yy >= self.h {
                continue;
            }
            let row = (yy * self.w) as usize;
            for xx in x..x + w {
                if xx < 0 || xx >= self.w {
                    continue;
                }
                blend_into(&mut self.px[row + xx as usize], color, 255);
            }
        }
    }

    pub fn round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
        if r <= 0 || w <= 0 || h <= 0 {
            self.rect(x, y, w, h, color);
            return;
        }
        let mut r = r;
        if r * 2 > h {
            r = h / 2;
        }
        if r * 2 > w {
            r = w / 2;
        }

        for yy in 0..h {
            let row_y = y + yy;
            if row_y < 0 || row_y >= self.h {
                continue;
            }
            let dy = if yy < r {
                r - yy - 1
            } else if yy >= h - r {
                yy - (h - r)
            } else {
                0
            };
            let inset = if dy > 0 {
                let d = (r * r - dy * dy) as f64;
                if d > 0.0 {
                    (r as f64 - d.sqrt()) as i32
                } else {
                    r
                }
            } else {
                0
            };
            let row = (row_y * self.w) as usize;
            for xx in (x + inset)..(x + w - inset) {
                if xx < 0 || xx >= self.w {
                    continue;
                }
                blend_into(&mut self.px[row + xx as usize], color, 255);
            }
        }
    }

    pub fn line_height(&self) -> i32 {
        Atlas::get().cell_h
    }

    pub fn text_width(&self, s: &str) -> i32 {
        let a = Atlas::get();
        let e = Glyphs::get();
        s.chars()
            .map(|c| {
                let cp = c as u32;
                // emoji 走彩色图集，宽度取它自己的 advance
                if let Some((_, adv)) = e.lookup(cp) {
                    return adv as i32;
                }
                a.advance(cp)
            })
            .sum()
    }

    /// 以 (x, y_top) 为左上角绘制一行文本
    pub fn text(&mut self, x: i32, y_top: i32, s: &str, color: u32) {
        let a = Atlas::get();
        let e = Glyphs::get();
        let mut cursor = x;

        for ch in s.chars() {
            let cp = ch as u32;
            // 彩色 emoji：整格像素是预乘 ARGB，直接叠上去，不经过 alpha 图集
            if let Some((cell, adv)) = e.lookup(cp) {
                for gy in 0..e.cell {
                    let py = y_top + gy;
                    if py < 0 || py >= self.h {
                        continue;
                    }
                    let row = (py * self.w) as usize;
                    for gx in 0..e.cell {
                        let px = cursor + gx;
                        if px < 0 || px >= self.w {
                            continue;
                        }
                        let src = e.pixel(cell, gx, gy);
                        if src >> 24 == 0 {
                            continue;
                        }
                        blend_into(&mut self.px[row + px as usize], src, 255);
                    }
                }
                cursor += adv as i32;
                continue;
            }
            if let Some((cell, _)) = a.lookup(cp) {
                for gy in 0..a.cell_h {
                    let py = y_top + gy;
                    if py < 0 || py >= self.h {
                        continue;
                    }
                    let src = a.glyph_row(cell, gy);
                    let row = (py * self.w) as usize;
                    for gx in 0..a.cell_w {
                        let alpha = src[gx as usize] as u32;
                        if alpha == 0 {
                            continue;
                        }
                        let px = cursor + gx;
                        if px < 0 || px >= self.w {
                            continue;
                        }
                        blend_into(&mut self.px[row + px as usize], color, alpha);
                    }
                }
            } else if ch != ' ' {
                // 缺字：画一个占位方块，方便一眼看出图集漏字
                self.rect(
                    cursor + 1,
                    y_top + a.baseline - a.font_px + 1,
                    a.font_px - 2,
                    2,
                    color,
                );
                self.rect(cursor + 1, y_top + a.baseline - 1, a.font_px - 2, 2, color);
            }
            cursor += a.advance(cp);
        }
    }

    /// 写成 24 位 BMP，用于构建期/调试期肉眼校验
    pub fn write_bmp(&self, path: &str) -> std::io::Result<()> {
        use std::io::Write;

        let row_size = ((self.w * 3 + 3) & !3) as usize;
        let img_size = row_size * self.h as usize;
        let mut hdr = [0u8; 54];
        hdr[0] = b'B';
        hdr[1] = b'M';
        hdr[2..6].copy_from_slice(&((54 + img_size) as u32).to_le_bytes());
        hdr[10..14].copy_from_slice(&54u32.to_le_bytes());
        hdr[14..18].copy_from_slice(&40u32.to_le_bytes());
        hdr[18..22].copy_from_slice(&self.w.to_le_bytes());
        hdr[22..26].copy_from_slice(&self.h.to_le_bytes());
        hdr[26..28].copy_from_slice(&1u16.to_le_bytes());
        hdr[28..30].copy_from_slice(&24u16.to_le_bytes());
        hdr[34..38].copy_from_slice(&(img_size as u32).to_le_bytes());

        let mut f = std::fs::File::create(path)?;
        f.write_all(&hdr)?;
        let pad = row_size - (self.w * 3) as usize;
        for y in (0..self.h).rev() {
            for x in 0..self.w {
                let p = self.px[(y * self.w + x) as usize];
                f.write_all(&[(p & 0xFF) as u8, ((p >> 8) & 0xFF) as u8, ((p >> 16) & 0xFF) as u8])?;
            }
            for _ in 0..pad {
                f.write_all(&[0])?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_is_drawn_in_colour() {
        let mut c = Canvas::new(40, 40);
        c.text(0, 0, "😀", argb(255, 255, 255, 255));

        let mut opaque = 0;
        let mut warm = 0;
        for p in &c.px {
            let a = p >> 24;
            if a == 0 {
                continue;
            }
            opaque += 1;
            // 预乘后亮度被 alpha 压过，按 alpha 还原再判断色相
            let r = ((p >> 16) & 0xFF) * 255 / a;
            let g = ((p >> 8) & 0xFF) * 255 / a;
            let b = (p & 0xFF) * 255 / a;
            if r > 200 && g > 150 && b < 120 {
                warm += 1;
            }
        }
        assert!(opaque > 100, "笑脸几乎没画出来: {opaque}");
        assert!(warm > 50, "笑脸不是黄色系（彩色渲染没生效）: warm={warm}");
    }

    #[test]
    fn emoji_width_comes_from_its_own_advance() {
        let c = Canvas::new(1, 1);
        assert_eq!(c.text_width("😀"), 24, "emoji 宽度应取图集 advance");
        assert!(c.text_width("你") < 24, "汉字宽度不应被 emoji 影响");
    }
}
