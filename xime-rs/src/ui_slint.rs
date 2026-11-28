//! Slint 候选窗：Slint 负责布局与光栅，像素交给 wl_shm。
//!
//! 为什么不用 Slint 的 winit/femtovg 后端：Wayland 下候选窗必须挂在
//! `zwp_input_popup_surface_v2` 上，位置由合成器贴合光标决定；普通
//! `xdg_toplevel` 既不能自己定位，也不该抢焦点。所以这里只取 Slint 的
//! 软件渲染器，把界面画进一块内存缓冲，再把内容区裁出来转成 ARGB8888
//! 交给现有的 popup surface。
//!
//! 尺寸的确定：MinimalSoftwareWindow 不会根据内容自动撑开，而 .slint 的
//! preferred-width 对含 for 循环的布局求值不可靠（实测会给出 22px 这种
//! 明显偏小的值）。因此改为先在一块足够大的画布上渲染，再扫描 alpha 通道
//! 求非透明内容的包围盒——panel 是不透明的圆角矩形，包围盒就是它的范围。
//!
//! 形态与主题：layout（横排/竖排/网格）、列数、颜色、度量都从 popup::Style
//! 映射进 .slint 的 Theme global 与组件属性，配置热重载时经 reload 重新写入。

use std::rc::Rc;
use std::time::Instant;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, ModelRc, PhysicalSize, SharedString, VecModel};

use crate::popup::Style;

slint::include_modules!();

/// 逻辑画布尺寸（逻辑像素，scale=1 时的取值）。横排单行最宽也就十几条候选；
/// 竖排/网格最多 9 条，9×行高(26) + 间距 ≈ 262px 高，所以高给到 512 留足余量。
/// 物理画布 = 逻辑画布 × scale，在高 DPI 屏上字体/间距按比例放大而不裁切。
const LOGICAL_W: i32 = 1024;
const LOGICAL_H: i32 = 512;

struct SlintPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
}

impl Platform for SlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> std::time::Duration {
        self.start.elapsed()
    }
}

/// 预乘 ARGB8888 整体按 f/255 缩放 alpha 与 RGB（保持预乘一致性）。
fn dim_alpha(pm: u32, f: u32) -> u32 {
    let a = ((pm >> 24) & 0xFF) * f / 255;
    let r = ((pm >> 16) & 0xFF) * f / 255;
    let g = ((pm >> 8) & 0xFF) * f / 255;
    let b = (pm & 0xFF) * f / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// 预乘 ARGB8888（config 内部表示）→ Slint 颜色（反预乘回直通 alpha）。
fn to_slint_color(pm: u32) -> slint::Color {
    let a = ((pm >> 24) & 0xFF) as u32;
    let un = |v: u32| if a == 0 { 0 } else { (v * 255 / a).min(255) };
    slint::Color::from_argb_u8(
        a as u8,
        un((pm >> 16) & 0xFF) as u8,
        un((pm >> 8) & 0xFF) as u8,
        un(pm & 0xFF) as u8,
    )
}

/// 取一个 emoji 的彩色位图，转成 Slint 的 Image（RGBA，非预乘）。
///
/// Slint 的软件渲染器只认轮廓字体，彩色的 CBDT 位图交给它只会描出黑色外形，
/// 所以这里直接把自带图集里的像素搬进 Image，由 .slint 按段当图片画。
fn emoji_image(ch: char) -> Option<slint::Image> {
    let g = crate::emoji::Glyphs::get();
    let (cell, _) = g.lookup(ch as u32)?;
    let n = g.cell.max(1) as u32;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(n, n);
    let dst = buf.make_mut_bytes();

    for y in 0..n as i32 {
        for x in 0..n as i32 {
            let p = g.pixel(cell, x, y);
            let a = (p >> 24) & 0xFF;
            if a == 0 {
                continue; // 缓冲初始就是全 0
            }
            let un = |v: u32| ((v * 255 / a).min(255)) as u8;
            let i = ((y as u32 * n + x as u32) * 4) as usize;
            dst[i] = un((p >> 16) & 0xFF);
            dst[i + 1] = un((p >> 8) & 0xFF);
            dst[i + 2] = un(p & 0xFF);
            dst[i + 3] = a as u8;
        }
    }
    Some(slint::Image::from_rgba8(buf))
}

/// 把一条候选切成若干段：连续的普通字符合成一个文本段，emoji 各自成图片段。
/// 绝大多数候选不含 emoji，结果就是「一个文本段」，与改造前完全一样。
fn segments_of(text: &str) -> ModelRc<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    let mut run = String::new();

    fn flush(run: &mut String, segs: &mut Vec<Seg>) {
        if run.is_empty() {
            return;
        }
        segs.push(Seg {
            text: SharedString::from(run.as_str()),
            icon: Default::default(),
            is_icon: false,
        });
        run.clear();
    }

    for ch in text.chars() {
        match emoji_image(ch) {
            Some(img) => {
                flush(&mut run, &mut segs);
                segs.push(Seg {
                    text: SharedString::default(),
                    icon: img,
                    is_icon: true,
                });
            }
            None => run.push(ch),
        }
    }
    flush(&mut run, &mut segs);
    ModelRc::from(Rc::new(VecModel::from(segs)))
}

/// popup::Layout → Slint 生成的 Layout 枚举
fn to_slint_layout(l: crate::popup::Layout) -> Layout {    match l {
        crate::popup::Layout::Horizontal => Layout::Horizontal,
        crate::popup::Layout::Vertical => Layout::Vertical,
        crate::popup::Layout::Grid => Layout::Grid,
    }
}

/// 把主题颜色/度量写进 Slint 的 Theme global，把形态/列数写进组件。
fn write_style(ui: &CandidateBar, style: &Style, layout: Layout, cols: i32) {
    let c = &style.theme.colors;
    let m = &style.theme.metrics;
    let theme = ui.global::<Theme>();
    theme.set_bg(to_slint_color(c.bg));
    theme.set_border(to_slint_color(c.border));
    theme.set_text(to_slint_color(c.text));
    // 序号与候选同色（自绘里序号和词是同一个字符串一起画的，没有单独的弱化色）
    theme.set_hint(to_slint_color(c.text));
    theme.set_sel_bg(to_slint_color(c.accent));
    theme.set_sel_text(to_slint_color(c.sel_text));
    // 翻页箭头：文字色的 45% 不透明度（预乘 alpha 整体缩放），弱化但随主题明暗自适应。
    // 比 border 更清晰、比 text 更淡，作「还能翻页」的提示不喧宾夺主。
    theme.set_arrow(to_slint_color(dim_alpha(c.text, 115)));

    theme.set_radius(m.radius as f32);
    theme.set_pad_x(m.pad_x as f32);
    theme.set_pad_y(m.pad_y as f32);
    theme.set_item_pad_x(m.item_pad_x as f32);
    theme.set_item_gap(m.item_gap as f32);
    theme.set_row_gap(m.row_gap as f32);
    theme.set_hl_radius(m.hl_radius as f32);
    theme.set_border_w(m.border_w as f32);

    ui.set_layout(layout);
    ui.set_cols(cols);
}

pub struct SlintUi {
    window: Rc<MinimalSoftwareWindow>,
    ui: CandidateBar,
    /// 当前缩放因子（逻辑像素 → 物理像素）
    scale: f32,
    /// 物理画布尺寸 = ceil(逻辑画布 × scale)
    canvas_w: i32,
    canvas_h: i32,
    /// 大画布的渲染缓冲（预乘 RGBA8），按物理像素分配
    buf: Vec<PremultipliedRgbaColor>,
    /// 裁剪后的内容区，ARGB8888，直接交给 wl_shm（物理像素）
    out: Vec<u32>,
    out_w: i32,
    out_h: i32,
    /// 当前形态与列数（render 时按它决定走一维还是二维模型）
    layout: Layout,
    cols: i32,
}

impl SlintUi {
    /// 创建 UI。`set_platform` 是进程级单次操作，已有平台时返回 None。
    pub fn new(scale: f32, style: &Style) -> Option<SlintUi> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let platform = SlintPlatform { window: window.clone(), start: Instant::now() };
        slint::platform::set_platform(Box::new(platform)).ok()?;

        let ui = CandidateBar::new().ok()?;
        ui.show().ok()?;

        let layout = to_slint_layout(style.layout);
        let cols = style.cols.max(1) as i32;
        write_style(&ui, style, layout, cols);

        let mut sl = SlintUi {
            window,
            ui,
            // 设成 0.0 强制首次 set_scale 走完整分配（包括 scale=1.0 的情况）
            scale: 0.0,
            canvas_w: LOGICAL_W,
            canvas_h: LOGICAL_H,
            buf: Vec::new(),
            out: Vec::new(),
            out_w: 0,
            out_h: 0,
            layout,
            cols,
        };
        sl.set_scale(scale);
        Some(sl)
    }

    /// 配置热重载后同步形态/主题（颜色、度量、布局、列数都可能变）。
    pub fn reload(&mut self, style: &Style) {
        self.layout = to_slint_layout(style.layout);
        self.cols = style.cols.max(1) as i32;
        write_style(&self.ui, style, self.layout, self.cols);
    }

    /// 设置缩放因子。通知 Slint 按逻辑像素布局，并按物理像素重新分配画布。
    /// scale 来自合成器（fractional scale / wl_output）或 XIME_SCALE 环境变量。
    pub fn set_scale(&mut self, scale: f32) {
        // 过滤非法值；1.0~4.0 之外的极端缩放在真实显示器上不出现，夹紧防止 OOM。
        let scale = if scale.is_finite() && scale > 0.0 { scale.clamp(0.5, 4.0) } else { 1.0 };
        if (scale - self.scale).abs() < 0.001 {
            return;
        }
        self.scale = scale;
        // 关键：告诉 Slint 逻辑像素→物理像素的换算。之后 set_size 传物理尺寸时，
        // Slint 会内部换算成逻辑尺寸布局，软件渲染器再按 scale 放大字体/间距。
        self.window.window().dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        self.canvas_w = (LOGICAL_W as f32 * scale).ceil() as i32;
        self.canvas_h = (LOGICAL_H as f32 * scale).ceil() as i32;
        self.buf = vec![
            PremultipliedRgbaColor { red: 0, green: 0, blue: 0, alpha: 0 };
            (self.canvas_w * self.canvas_h) as usize
        ];
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// 更新候选并渲染一帧，返回内容区实际尺寸（物理像素，已裁掉透明留白）。
    pub fn render(&mut self, cands: &[String], has_prev: bool, has_next: bool) -> (i32, i32) {
        self.ui.set_has_prev(has_prev);
        self.ui.set_has_next(has_next);
        match self.layout {
            Layout::Horizontal | Layout::Vertical => {
                let model: Vec<ModelRc<Seg>> = cands.iter().map(|s| segments_of(s)).collect();
                self.ui.set_candidates(ModelRc::from(Rc::new(VecModel::from(model))));
            }
            Layout::Grid => {
                // 网格：按 cols 切成行，交给 .slint 里的嵌套 for 遍历。
                // Slint 的 [[[Seg]]] 对应 Rust 的 ModelRc<ModelRc<ModelRc<Seg>>>：
                // 最外层是「行」，中间是「一条候选」，最内层是「一条候选里的段」。
                let cols = self.cols.max(1) as usize;
                let row_models: Vec<ModelRc<ModelRc<Seg>>> = cands
                    .chunks(cols)
                    .map(|row| {
                        let v: Vec<ModelRc<Seg>> = row.iter().map(|s| segments_of(s)).collect();
                        ModelRc::from(Rc::new(VecModel::from(v)))
                    })
                    .collect();
                let outer: ModelRc<ModelRc<ModelRc<Seg>>> =
                    ModelRc::from(Rc::new(VecModel::from(row_models)));
                self.ui.set_rows(outer);
            }
        }

        // 在物理画布上渲染。set_size 传物理尺寸，Slint 已知道 scale，
        // 会把逻辑布局放大到这块物理画布上（字体、圆角、间距都按比例）。
        let (cw, ch) = (self.canvas_w, self.canvas_h);
        self.window.window().set_size(PhysicalSize::new(cw as u32, ch as u32));
        self.window.request_redraw();
        let win = self.window.clone();
        win.draw_if_needed(|renderer| {
            renderer.render(&mut self.buf[..], cw as usize);
        });

        // 扫描 alpha 求内容包围盒（panel 不透明，其余全透明）。
        let (mut x0, mut y0, mut x1, mut y1) = (cw, ch, -1, -1);
        for y in 0..ch {
            let row = (y * cw) as usize;
            for x in 0..cw {
                if self.buf[row + x as usize].alpha > 0 {
                    if x < x0 { x0 = x; }
                    if x > x1 { x1 = x; }
                    if y < y0 { y0 = y; }
                    if y > y1 { y1 = y; }
                }
            }
        }
        if x1 < 0 {
            // 无内容：清空输出
            self.out.clear();
            self.out_w = 0;
            self.out_h = 0;
            return (0, 0);
        }

        // 裁出内容区并转 ARGB8888（预乘值原样搬运，不二次预乘）。
        let w = x1 - x0 + 1;
        let h = y1 - y0 + 1;
        self.out.clear();
        self.out.reserve((w * h) as usize);
        for y in y0..=y1 {
            let row = (y * cw) as usize;
            for x in x0..=x1 {
                let p = &self.buf[row + x as usize];
                self.out.push(to_argb(*p));
            }
        }
        self.out_w = w;
        self.out_h = h;
        (w, h)
    }

    /// 裁剪后的内容区（ARGB8888），供 wl_shm 上传。
    pub fn out(&self) -> &[u32] {
        &self.out
    }

    /// 把裁剪后的内容写成 24 位 BMP（--ui-probe 肉眼校验用）。
    /// 圆角处为半透明，BMP 无 alpha 通道，这里合成到棋盘格背景。
    pub fn write_bmp(&self, path: &str, check: bool) -> std::io::Result<()> {
        use std::io::Write;
        let (w, h) = (self.out_w, self.out_h);
        if w <= 0 || h <= 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "空内容"));
        }
        let row_size = ((w * 3 + 3) & !3) as usize;
        let img_size = row_size * h as usize;
        let mut hdr = [0u8; 54];
        hdr[0] = b'B';
        hdr[1] = b'M';
        hdr[2..6].copy_from_slice(&((54 + img_size) as u32).to_le_bytes());
        hdr[10..14].copy_from_slice(&54u32.to_le_bytes());
        hdr[14..18].copy_from_slice(&40u32.to_le_bytes());
        hdr[18..22].copy_from_slice(&w.to_le_bytes());
        hdr[22..26].copy_from_slice(&h.to_le_bytes());
        hdr[26..28].copy_from_slice(&1u16.to_le_bytes());
        hdr[28..30].copy_from_slice(&24u16.to_le_bytes());
        hdr[34..38].copy_from_slice(&(img_size as u32).to_le_bytes());
        let mut f = std::fs::File::create(path)?;
        f.write_all(&hdr)?;
        let pad = row_size - (w * 3) as usize;
        for yy in (0..h).rev() {
            for xx in 0..w {
                let p = self.out[(yy * w + xx) as usize];
                let a = (p >> 24) & 0xFF;
                let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
                let (br, bg, bb): (u32, u32, u32) = if check && ((xx / 8 + yy / 8) & 1) == 0 {
                    (238, 238, 238)
                } else if check {
                    (204, 204, 204)
                } else {
                    (255, 255, 255)
                };
                let inv = 255 - a;
                let rr = (r + br * inv / 255).min(255);
                let gg = (g + bg * inv / 255).min(255);
                let bb2 = (b + bb * inv / 255).min(255);
                f.write_all(&[bb2 as u8, gg as u8, rr as u8])?;
            }
            for _ in 0..pad {
                f.write_all(&[0])?;
            }
        }
        Ok(())
    }
}

/// 预乘 RGBA8 → ARGB8888。Slint 输出的 RGB 分量已经预乘过 alpha，
/// 这里只是重新排布字节序（wl_shm 的 ARGB8888 在小端下即 BGRA 存储）。
#[inline]
pub fn to_argb(p: PremultipliedRgbaColor) -> u32 {
    ((p.alpha as u32) << 24)
        | ((p.red as u32) << 16)
        | ((p.green as u32) << 8)
        | (p.blue as u32)
}
