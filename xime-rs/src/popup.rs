//! 候选窗布局与绘制：支持横排 / 竖排 / 网格三种形态，以及明暗两套配色。
//!
//! 形态划分对齐 fcitx5 的候选面板：横排单行条、竖排列表、网格多列。
//! 候选窗本身不是普通窗口：它必须通过 `zwp_input_popup_surface_v2` 挂到输入法上，
//! 由合成器负责贴着光标摆放。这里只负责"画多大、画什么"。

use crate::render::{argb, Canvas};

// 内边距/圆角随字号一起放大：图集现在是 32px 字（`FONT_PX`），
// 按 24px 那套数字排版会显得又挤又小。这些值只在 measure/paint 里用，
// 两边一致就不会错位。
pub const PAD_X: i32 = 11;
pub const PAD_Y: i32 = 6;
pub const ITEM_PAD_X: i32 = 9;
pub const ITEM_GAP: i32 = 4;
pub const ROW_GAP: i32 = 2;
pub const RADIUS: i32 = 11;

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

/// 候选窗样式：形态 + 网格列数 + 一套主题（颜色 + 度量）
pub struct Style {
    pub layout: Layout,
    pub cols: usize,
    pub dark: bool,
    pub theme: crate::config::Theme,
}

impl Style {
    /// 度量取当前主题的
    #[inline]
    fn m(&self) -> &crate::config::Metrics {
        &self.theme.metrics
    }
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
        Style {
            layout: Layout::Horizontal,
            cols: 3,
            dark: false,
            theme: crate::config::Theme::light(),
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// 行高：文字行高 + 高亮块上下外扩。`hl_pad_y` 为默认值 1 时结果等于文字行高，
/// 保证不配置任何东西时渲染与旧版本逐字节一致。
fn row_h(c: &Canvas, style: &Style) -> i32 {
    let extra = (style.m().hl_pad_y - 1).max(0) * 2;
    c.line_height() + extra
}

/// 计算候选窗尺寸
pub fn measure(c: &Canvas, cands: &[String], style: &Style) -> (i32, i32) {
    let lh = c.line_height();
    let rh = row_h(c, style);
    let m = style.m();
    let n = cands.len() as i32;
    if n == 0 {
        return (0, 0);
    }

    match style.layout {
        Layout::Horizontal => {
            let mut width = m.pad_x * 2;
            for (i, cand) in cands.iter().enumerate() {
                width += c.text_width(&label(i, cand, true)) + m.item_pad_x * 2;
                if i > 0 {
                    width += m.item_gap;
                }
            }
            // 高亮块比文字行高时，面板要跟着长高，否则选中块会被裁掉
            let h = (lh + m.pad_y * 2).max(lh + m.hl_pad_y * 2);
            (width, h)
        }
        Layout::Vertical => {
            let mut item_w = 0;
            for (i, cand) in cands.iter().enumerate() {
                item_w = item_w.max(c.text_width(&label(i, cand, true)) + m.item_pad_x * 2);
            }
            (item_w + m.pad_x * 2, n * rh + (n - 1) * m.row_gap + m.pad_y * 2)
        }
        Layout::Grid => {
            let cols = (style.cols.max(1) as i32).min(n);
            let rows = (n + cols - 1) / cols;
            let mut cell_w = 0;
            for (i, cand) in cands.iter().enumerate() {
                cell_w = cell_w.max(c.text_width(&label(i, cand, false)) + m.item_pad_x * 2);
            }
            (
                cell_w * cols + m.item_gap * (cols - 1) + m.pad_x * 2,
                rows * rh + (rows - 1) * m.row_gap + m.pad_y * 2,
            )
        }
    }
}

/// 把候选窗画进画布；画布尺寸必须与 measure 的结果一致
pub fn paint(c: &mut Canvas, cands: &[String], style: &Style, mode: Mode) {
    let m = *style.m();
    let mut col = style.theme.colors;
    if mode == Mode::En {
        // 青色：英文态标识。只换选中块颜色，不动任何尺寸计算
        col.accent = if style.theme.dark {
            argb(255, 20, 160, 133)
        } else {
            argb(255, 13, 148, 136)
        };
    }
    let lh = c.line_height();
    let rh = row_h(c, style);
    let text_pad = (rh - lh) / 2; // 高亮块外扩后文字要居中
    c.clear(0);

    // 底 + 描边（border_w 可为 0，此时只画底）
    if m.border_w > 0 {
        c.round_rect(0, 0, c.w, c.h, m.radius, col.border);
        let r = (m.radius - m.border_w).max(0);
        c.round_rect(
            m.border_w,
            m.border_w,
            c.w - m.border_w * 2,
            c.h - m.border_w * 2,
            r,
            col.bg,
        );
    } else {
        c.round_rect(0, 0, c.w, c.h, m.radius, col.bg);
    }

    // 选中项统一用圆角高亮块 + 反白文字
    let item = |c: &mut Canvas, x: i32, y: i32, w: i32, text: &str, sel: bool| {
        if sel {
            c.round_rect(
                x,
                y - m.hl_pad_y,
                w,
                lh + m.hl_pad_y * 2,
                m.hl_radius,
                col.accent,
            );
            c.text(x + m.item_pad_x, y, text, col.sel_text);
        } else {
            c.text(x + m.item_pad_x, y, text, col.text);
        }
    };

    match style.layout {
        Layout::Horizontal => {
            let mut x = m.pad_x;
            let y = m.pad_y + text_pad;
            for (i, cand) in cands.iter().enumerate() {
                let l = label(i, cand, true);
                let item_w = c.text_width(&l) + m.item_pad_x * 2;
                item(c, x, y, item_w, &l, i == 0);
                x += item_w + m.item_gap;
            }
        }
        Layout::Vertical => {
            let item_w = c.w - m.pad_x * 2;
            for (i, cand) in cands.iter().enumerate() {
                let y = m.pad_y + i as i32 * (rh + m.row_gap) + text_pad;
                let l = label(i, cand, true);
                item(c, m.pad_x, y, item_w, &l, i == 0);
            }
        }
        Layout::Grid => {
            let n = cands.len() as i32;
            let cols = (style.cols.max(1) as i32).min(n);
            let cell_w = (c.w - m.pad_x * 2 - m.item_gap * (cols - 1)) / cols;
            for (i, cand) in cands.iter().enumerate() {
                let i = i as i32;
                let x = m.pad_x + (i % cols) * (cell_w + m.item_gap);
                let y = m.pad_y + (i / cols) * (rh + m.row_gap) + text_pad;
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
        Style { layout, cols: 3, dark: false, ..Style::default() }
    }

    #[test]
    fn three_shapes_height_matches_rows() {
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
    fn vertical_narrower_and_taller() {
        let c = Canvas::new(1, 1);
        let v = cands();
        let (hw, hh) = measure(&c, &v, &style(Layout::Horizontal));
        let (vw, vh) = measure(&c, &v, &style(Layout::Vertical));
        assert!(vw < hw, "竖排应当更窄: {vw} vs {hw}");
        assert!(vh > hh, "竖排应当更高: {vh} vs {hh}");
    }

    #[test]
    fn grid_cols_bounded_by_candidates() {
        let c = Canvas::new(1, 1);
        let two = vec!["你好".to_string(), "你号".to_string()];
        let s = Style { layout: Layout::Grid, cols: 5, dark: false, ..Style::default() };
        let (w, h) = measure(&c, &two, &s);
        // 只有 2 条时候选集不该按 5 列撑宽
        let (w2, _) = measure(&c, &two, &Style { layout: Layout::Grid, cols: 2, dark: false, ..Style::default() });
        assert_eq!(w, w2);
        assert_eq!(h, c.line_height() + PAD_Y * 2);
    }

    #[test]
    fn draw_within_bounds_and_reusable() {
        for layout in [Layout::Horizontal, Layout::Vertical, Layout::Grid] {
            for dark in [false, true] {
                let s = Style { layout, cols: 3, dark, ..Style::default() };
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
    fn english_accent_differs_but_size_same() {
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
    fn visible_count_varies_by_shape() {
        assert_eq!(style(Layout::Horizontal).max_items(), 6);
        assert_eq!(style(Layout::Vertical).max_items(), 9);
        assert_eq!(Style { layout: Layout::Grid, cols: 3, dark: false, ..Style::default() }.max_items(), 9);
        assert_eq!(Style { layout: Layout::Grid, cols: 9, dark: false, ..Style::default() }.max_items(), 9);
        assert_eq!(Style { layout: Layout::Grid, cols: 2, dark: false, ..Style::default() }.max_items(), 6);
    }

    #[test]
    fn shape_name_roundtrip_parse() {
        for l in [Layout::Horizontal, Layout::Vertical, Layout::Grid] {
            assert_eq!(Layout::parse(l.name()), Some(l));
        }
        assert_eq!(Layout::parse("h"), Some(Layout::Horizontal));
        assert_eq!(Layout::parse("v"), Some(Layout::Vertical));
        assert_eq!(Layout::parse("g"), Some(Layout::Grid));
        assert_eq!(Layout::parse("xxx"), None);
    }
}
