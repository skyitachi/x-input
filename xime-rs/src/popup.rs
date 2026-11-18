//! 候选窗布局与绘制：支持横排 / 竖排 / 网格三种形态，以及明暗两套配色。
//!
//! 形态划分对齐 fcitx5 的候选面板：横排单行条、竖排列表、网格多列。
//! 候选窗本身不是普通窗口：它必须通过 `zwp_input_popup_surface_v2` 挂到输入法上，
//! 由合成器负责贴着光标摆放。这里只负责"画多大、画什么"。

use crate::render::{argb, Canvas};

pub const PAD_X: i32 = 10;
pub const PAD_Y: i32 = 6;
pub const ITEM_PAD_X: i32 = 9;
pub const ITEM_GAP: i32 = 4;
pub const ROW_GAP: i32 = 2;
pub const RADIUS: i32 = 10;

/// 候选面板形态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// 单行横排，像一条贴边的横条（默认）
    Horizontal,
    /// 单列竖排，一候一行
    Vertical,
    /// 网格，多行多列
    Grid,
}

impl Layout {
    pub fn parse(s: &str) -> Option<Layout> {
        match s {
            "horizontal" | "h" | "bar" => Some(Layout::Horizontal),
            "vertical" | "v" | "list" => Some(Layout::Vertical),
            "grid" | "g" => Some(Layout::Grid),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Layout::Horizontal => "horizontal",
            Layout::Vertical => "vertical",
            Layout::Grid => "grid",
        }
    }
}

/// 候选窗样式：形态 + 网格列数 + 配色
pub struct Style {
    pub layout: Layout,
    pub cols: usize,
    pub dark: bool,
}

/// 输入模式：只影响强调色，用来一眼分清中英态
///
/// 没加文字标记也没改布局——候选内容本身（汉字 vs 英文单词）已经能区分，
/// 这里只是把选中块换个色，零尺寸风险。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Zh,
    En,
}

impl Default for Style {
    fn default() -> Self {
        Style { layout: Layout::Horizontal, cols: 3, dark: false }
    }
}

impl Style {
    /// 一次最多显示多少条：横排省地方，竖排/网格可以多给几条
    pub fn max_items(&self) -> usize {
        match self.layout {
            Layout::Horizontal => 6,
            Layout::Vertical => 9,
            Layout::Grid => (self.cols.max(1) * 3).clamp(3, 9),
        }
    }
}

pub struct Colors {
    pub bg: u32,
    pub border: u32,
    pub text: u32,
    pub accent: u32,
    pub sel_text: u32,
}

pub fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            bg: argb(242, 32, 33, 37),
            border: argb(56, 255, 255, 255),
            text: argb(255, 236, 237, 242),
            accent: argb(255, 59, 122, 245),
            sel_text: argb(255, 255, 255, 255),
        }
    } else {
        Colors {
            bg: argb(247, 251, 252, 254),
            border: argb(36, 0, 0, 0),
            text: argb(255, 23, 24, 28),
            accent: argb(255, 47, 107, 255),
            sel_text: argb(255, 255, 255, 255),
        }
    }
}

/// 候选项文字：横排/竖排带序号点，网格用紧凑的「序号 空格 词」
pub fn label(idx: usize, cand: &str, with_dot: bool) -> String {
    if with_dot {
        format!("{}. {cand}", idx + 1)
    } else {
        format!("{} {cand}", idx + 1)
    }
}

/// 计算候选窗尺寸
pub fn measure(c: &Canvas, cands: &[String], style: &Style) -> (i32, i32) {
    let lh = c.line_height();
    let n = cands.len() as i32;
    if n == 0 {
        return (0, 0);
    }

    match style.layout {
        Layout::Horizontal => {
            let mut width = PAD_X * 2;
            for (i, cand) in cands.iter().enumerate() {
                width += c.text_width(&label(i, cand, true)) + ITEM_PAD_X * 2;
                if i > 0 {
                    width += ITEM_GAP;
                }
            }
            (width, lh + PAD_Y * 2)
        }
        Layout::Vertical => {
            let mut item_w = 0;
            for (i, cand) in cands.iter().enumerate() {
                item_w = item_w.max(c.text_width(&label(i, cand, true)) + ITEM_PAD_X * 2);
            }
            (item_w + PAD_X * 2, n * lh + (n - 1) * ROW_GAP + PAD_Y * 2)
        }
        Layout::Grid => {
            let cols = (style.cols.max(1) as i32).min(n);
            let rows = (n + cols - 1) / cols;
            let mut cell_w = 0;
            for (i, cand) in cands.iter().enumerate() {
                cell_w = cell_w.max(c.text_width(&label(i, cand, false)) + ITEM_PAD_X * 2);
            }
            (
                cell_w * cols + ITEM_GAP * (cols - 1) + PAD_X * 2,
                rows * lh + (rows - 1) * ROW_GAP + PAD_Y * 2,
            )
        }
    }
}

/// 把候选窗画进画布；画布尺寸必须与 measure 的结果一致
pub fn paint(c: &mut Canvas, cands: &[String], style: &Style, mode: Mode) {
    let mut col = colors(style.dark);
    if mode == Mode::En {
        // 青色：英文态标识。只换选中块颜色，不动任何尺寸计算
        col.accent = if style.dark {
            argb(255, 20, 160, 133)
        } else {
            argb(255, 13, 148, 136)
        };
    }
    let lh = c.line_height();
    c.clear(0);

    // 底 + 1px 描边
    c.round_rect(0, 0, c.w, c.h, RADIUS, col.border);
    c.round_rect(1, 1, c.w - 2, c.h - 2, RADIUS - 1, col.bg);

    // 选中项统一用圆角高亮块 + 反白文字
    let item = |c: &mut Canvas, x: i32, y: i32, w: i32, text: &str, sel: bool| {
        if sel {
            c.round_rect(x, y - 1, w, lh + 2, 7, col.accent);
            c.text(x + ITEM_PAD_X, y, text, col.sel_text);
        } else {
            c.text(x + ITEM_PAD_X, y, text, col.text);
        }
    };

    match style.layout {
        Layout::Horizontal => {
            let mut x = PAD_X;
            for (i, cand) in cands.iter().enumerate() {
                let l = label(i, cand, true);
                let item_w = c.text_width(&l) + ITEM_PAD_X * 2;
                item(c, x, PAD_Y, item_w, &l, i == 0);
                x += item_w + ITEM_GAP;
            }
        }
        Layout::Vertical => {
            let item_w = c.w - PAD_X * 2;
            for (i, cand) in cands.iter().enumerate() {
                let y = PAD_Y + i as i32 * (lh + ROW_GAP);
                let l = label(i, cand, true);
                item(c, PAD_X, y, item_w, &l, i == 0);
            }
        }
        Layout::Grid => {
            let n = cands.len() as i32;
            let cols = (style.cols.max(1) as i32).min(n);
            let cell_w = (c.w - PAD_X * 2 - ITEM_GAP * (cols - 1)) / cols;
            for (i, cand) in cands.iter().enumerate() {
                let i = i as i32;
                let x = PAD_X + (i % cols) * (cell_w + ITEM_GAP);
                let y = PAD_Y + (i / cols) * (lh + ROW_GAP);
                let l = label(i as usize, cand, false);
                item(c, x, y, cell_w, &l, i == 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands() -> Vec<String> {
        vec!["你好".into(), "你号".into(), "呢好".into(), "妮好".into(), "泥壕".into(), "拟好".into()]
    }

    fn style(layout: Layout) -> Style {
        Style { layout, cols: 3, dark: false }
    }

    #[test]
    fn 三种形态高度符合行数() {
        let c = Canvas::new(1, 1);
        let lh = c.line_height();
        let v = cands();

        let (_, h) = measure(&c, &v, &style(Layout::Horizontal));
        assert_eq!(h, lh + PAD_Y * 2);

        let (_, h) = measure(&c, &v, &style(Layout::Vertical));
        assert_eq!(h, 6 * lh + 5 * ROW_GAP + PAD_Y * 2);

        let (_, h) = measure(&c, &v, &style(Layout::Grid));
        assert_eq!(h, 2 * lh + ROW_GAP + PAD_Y * 2);
    }

    #[test]
    fn 竖排比横排窄且更高() {
        let c = Canvas::new(1, 1);
        let v = cands();
        let (hw, hh) = measure(&c, &v, &style(Layout::Horizontal));
        let (vw, vh) = measure(&c, &v, &style(Layout::Vertical));
        assert!(vw < hw, "竖排应当更窄: {vw} vs {hw}");
        assert!(vh > hh, "竖排应当更高: {vh} vs {hh}");
    }

    #[test]
    fn 网格列数受候选数约束() {
        let c = Canvas::new(1, 1);
        let two = vec!["你好".to_string(), "你号".to_string()];
        let s = Style { layout: Layout::Grid, cols: 5, dark: false };
        let (w, h) = measure(&c, &two, &s);
        // 只有 2 条时候选集不该按 5 列撑宽
        let (w2, _) = measure(&c, &two, &Style { layout: Layout::Grid, cols: 2, dark: false });
        assert_eq!(w, w2);
        assert_eq!(h, c.line_height() + PAD_Y * 2);
    }

    #[test]
    fn 绘制不越界且各形态可复用() {
        for layout in [Layout::Horizontal, Layout::Vertical, Layout::Grid] {
            for dark in [false, true] {
                let s = Style { layout, cols: 3, dark };
                let v = cands();
                let probe = Canvas::new(1, 1);
                let (w, h) = measure(&probe, &v, &s);
                let mut c = Canvas::new(w, h);
                paint(&mut c, &v, &s, Mode::Zh);
                // 底板上应当已经有像素（bg 不是全透明）
                assert!(c.px.iter().any(|p| *p != 0), "{layout:?} 画了空画布");
            }
        }
    }

    #[test]
    fn 英文态强调色不同但尺寸一致() {
        let s = Style::default();
        let v = vec!["help".to_string(), "held".to_string(), "hello".to_string()];
        let probe = Canvas::new(1, 1);
        let (w, h) = measure(&probe, &v, &s);

        let mut zh = Canvas::new(w, h);
        let mut en = Canvas::new(w, h);
        paint(&mut zh, &v, &s, Mode::Zh);
        paint(&mut en, &v, &s, Mode::En);

        assert_ne!(zh.px, en.px, "英文态应换强调色，否则分不出中英");
        // 模式只影响颜色，绝不能影响布局——否则切换时候选窗会跳位
        assert_eq!((w, h), measure(&probe, &v, &s));
    }

    #[test]
    fn 显示条数随形态变化() {
        assert_eq!(style(Layout::Horizontal).max_items(), 6);
        assert_eq!(style(Layout::Vertical).max_items(), 9);
        assert_eq!(Style { layout: Layout::Grid, cols: 3, dark: false }.max_items(), 9);
        assert_eq!(Style { layout: Layout::Grid, cols: 9, dark: false }.max_items(), 9);
        assert_eq!(Style { layout: Layout::Grid, cols: 2, dark: false }.max_items(), 6);
    }

    #[test]
    fn 形态名称可往返解析() {
        for l in [Layout::Horizontal, Layout::Vertical, Layout::Grid] {
            assert_eq!(Layout::parse(l.name()), Some(l));
        }
        assert_eq!(Layout::parse("h"), Some(Layout::Horizontal));
        assert_eq!(Layout::parse("v"), Some(Layout::Vertical));
        assert_eq!(Layout::parse("g"), Some(Layout::Grid));
        assert_eq!(Layout::parse("xxx"), None);
    }
}
