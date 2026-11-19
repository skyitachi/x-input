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

use std::rc::Rc;
use std::time::Instant;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter};
use slint::{ModelRc, PhysicalSize, SharedString, VecModel};

slint::include_modules!();

/// 渲染画布上限。候选条最宽也就十几条，1024×128 足够；超出会被裁，但我们
/// 的候选数在 popup::Style::max_items() 里已限制在 6~9 条。
const CANVAS_W: i32 = 1024;
const CANVAS_H: i32 = 128;

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

pub struct SlintUi {
    window: Rc<MinimalSoftwareWindow>,
    ui: CandidateBar,
    /// 大画布的渲染缓冲（预乘 RGBA8）
    buf: Vec<PremultipliedRgbaColor>,
    /// 裁剪后的内容区，ARGB8888，直接交给 wl_shm
    out: Vec<u32>,
    out_w: i32,
    out_h: i32,
}

impl SlintUi {
    /// 创建 UI。`set_platform` 是进程级单次操作，已有平台时返回 None。
    pub fn new() -> Option<SlintUi> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let platform = SlintPlatform { window: window.clone(), start: Instant::now() };
        slint::platform::set_platform(Box::new(platform)).ok()?;

        let ui = CandidateBar::new().ok()?;
        ui.show().ok()?;
        let buf = vec![
            PremultipliedRgbaColor { red: 0, green: 0, blue: 0, alpha: 0 };
            (CANVAS_W * CANVAS_H) as usize
        ];
        Some(SlintUi { window, ui, buf, out: Vec::new(), out_w: 0, out_h: 0 })
    }

    /// 更新候选并渲染一帧，返回内容区实际尺寸（已裁掉透明留白）。
    pub fn render(&mut self, cands: &[String]) -> (i32, i32) {
        let model: Vec<SharedString> =
            cands.iter().map(|s| SharedString::from(s.as_str())).collect();
        self.ui.set_candidates(ModelRc::from(Rc::new(VecModel::from(model))));

        // 在大画布上渲染。MinimalSoftwareWindow 的 size 由 set_size 决定。
        self.window.window().set_size(PhysicalSize::new(CANVAS_W as u32, CANVAS_H as u32));
        self.window.request_redraw();
        let win = self.window.clone();
        win.draw_if_needed(|renderer| {
            renderer.render(&mut self.buf[..], CANVAS_W as usize);
        });

        // 扫描 alpha 求内容包围盒（panel 不透明，其余全透明）。
        let (mut x0, mut y0, mut x1, mut y1) = (CANVAS_W, CANVAS_H, -1, -1);
        for y in 0..CANVAS_H {
            let row = (y * CANVAS_W) as usize;
            for x in 0..CANVAS_W {
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
            let row = (y * CANVAS_W) as usize;
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
