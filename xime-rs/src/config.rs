//! 候选窗外观的配置层。
//!
//! 解析顺序（后者覆盖前者）：
//!   内置默认 → /etc/xime/config.ini → $XDG_CONFIG_HOME/xime/config.ini
//!   → 环境变量 XIME_THEME → 命令行 `--theme/--popup/--popup-cols`
//!
//! 主题有两种来源：
//!   1. xime 自己的 `config.ini`（`[xime]` + `[colors]` + `[metrics]`）
//!   2. **fcitx5 的 theme.conf**（`--theme fcitx:omarchy`）。它是 KConfig/INI 格式，
//!      这里实现需要的子集：颜色、内外边距、边框宽度。纯色主题可以 100% 还原；
//!      带 `Image=` 的九宫格背景不吃（xime 没有图片解码器，见 README）。
//!
//! 不引入 ini/toml crate：格式就几十行，自己写能把二进制体积和构建时间守住，
//! 也和当初自研 keymap 解析是同一条思路。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::popup::{colors, Colors, Layout};

/// 版面度量。默认值等于以前写死在 `popup.rs` 里的那组常量，
/// 所以「不配任何东西」时渲染结果与旧版本逐字节相同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// 面板左右内边距
    pub pad_x: i32,
    /// 面板上下内边距
    pub pad_y: i32,
    /// 单个候选内部左右留白
    pub item_pad_x: i32,
    /// 候选之间水平间距（横排/网格）
    pub item_gap: i32,
    /// 行间距
    pub row_gap: i32,
    /// 面板圆角
    pub radius: i32,
    /// 选中块圆角
    pub hl_radius: i32,
    /// 选中块相对文字行的上下外扩
    pub hl_pad_y: i32,
    /// 面板描边宽度（0 表示不描边）
    pub border_w: i32,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            pad_x: crate::popup::PAD_X,
            pad_y: crate::popup::PAD_Y,
            item_pad_x: crate::popup::ITEM_PAD_X,
            item_gap: crate::popup::ITEM_GAP,
            row_gap: crate::popup::ROW_GAP,
            radius: crate::popup::RADIUS,
            hl_radius: 7,
            hl_pad_y: 1,
            border_w: 1,
        }
    }
}

/// 一套完整外观：颜色 + 度量
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    pub colors: Colors,
    pub metrics: Metrics,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::light()
    }
}

impl Theme {
    pub fn light() -> Theme {
        Theme {
            name: "light".into(),
            dark: false,
            colors: colors(false),
            metrics: Metrics::default(),
        }
    }

    pub fn dark() -> Theme {
        Theme {
            name: "dark".into(),
            dark: true,
            colors: colors(true),
            metrics: Metrics::default(),
        }
    }

    /// 从 fcitx5 的 theme.conf 映射。字段对应关系见 README「兼容 fcitx 主题」。
    pub fn from_fcitx(ini: &Ini, name: &str) -> Theme {
        let bg = ini.color("InputPanel/Background", "Color", colors(false).bg);
        // fcitx 没有「深色主题」标记键，用背景亮度判断（BT.601 luma）
        let luma = ((77 * ((bg >> 16) & 0xFF) + 150 * ((bg >> 8) & 0xFF) + 29 * (bg & 0xFF)) >> 8) as u32;
        let mut th = if luma < 128 { Theme::dark() } else { Theme::light() };

        th.name = name.to_string();
        th.dark = luma < 128;

        // 颜色：InputPanel 段的三个键 + Background 段
        th.colors.bg = bg;
        th.colors.border = ini.color("InputPanel/Background", "BorderColor", th.colors.border);
        th.colors.text = ini.color("InputPanel", "NormalColor", th.colors.text);
        // fcitx 里 HighlightCandidateColor 是选中项的文字色，不是块色；块色是 HighlightBackgroundColor
        th.colors.sel_text = ini.color("InputPanel", "HighlightCandidateColor", th.colors.sel_text);
        th.colors.accent = ini.color(
            "InputPanel",
            "HighlightBackgroundColor",
            ini.color("InputPanel/Highlight", "Color", th.colors.accent),
        );

        // 度量：fcitx 的边距是分层叠加的
        //   实际内边距 = ContentMargin + Background/Margin
        //   文字留白   = TextMargin.Left/Right
        //   高亮块     = 文字行高 + Highlight/Margin.Top/Bottom
        let bgm = ini.i32("InputPanel/Background/Margin", "Left", 0);
        let bgm_t = ini.i32("InputPanel/Background/Margin", "Top", 0);
        let cm = ini.i32("InputPanel/ContentMargin", "Left", 0);
        let cm_t = ini.i32("InputPanel/ContentMargin", "Top", 0);
        let tm = ini.i32("InputPanel/TextMargin", "Left", th.metrics.item_pad_x);

        th.metrics.pad_x = cm + bgm;
        th.metrics.pad_y = cm_t + bgm_t;
        th.metrics.item_pad_x = tm;
        // Spacing 是候选间距；未定义时 fcitx 用 0，靠 TextMargin 撑开呼吸感
        let spacing = ini.i32("InputPanel", "Spacing", 0);
        th.metrics.item_gap = spacing;
        th.metrics.row_gap = spacing;
        th.metrics.hl_pad_y = ini.i32("InputPanel/Highlight/Margin", "Top", 1);
        th.metrics.border_w = ini.i32("InputPanel/Background", "BorderWidth", 1);
        // fcitx 纯色主题是直角 + 直角高亮块，忠实还原就都设 0
        th.metrics.radius = 0;
        th.metrics.hl_radius = 0;
        th
    }

    /// 从 xime 自己的配置格式映射
    pub fn from_xime(ini: &Ini, name: &str) -> Theme {
        let mut th = match ini.get("xime", "dark").unwrap_or("false") {
            "true" | "True" | "1" | "yes" => Theme::dark(),
            _ => Theme::light(),
        };
        th.name = name.to_string();

        th.colors.bg = ini.color("colors", "bg", th.colors.bg);
        th.colors.border = ini.color("colors", "border", th.colors.border);
        th.colors.text = ini.color("colors", "text", th.colors.text);
        th.colors.accent = ini.color("colors", "accent", th.colors.accent);
        th.colors.sel_text = ini.color("colors", "selected_text", th.colors.sel_text);

        let d = Metrics::default();
        th.metrics = Metrics {
            pad_x: ini.i32("metrics", "pad_x", d.pad_x),
            pad_y: ini.i32("metrics", "pad_y", d.pad_y),
            item_pad_x: ini.i32("metrics", "item_pad_x", d.item_pad_x),
            item_gap: ini.i32("metrics", "item_gap", d.item_gap),
            row_gap: ini.i32("metrics", "row_gap", d.row_gap),
            radius: ini.i32("metrics", "radius", d.radius),
            hl_radius: ini.i32("metrics", "hl_radius", d.hl_radius),
            hl_pad_y: ini.i32("metrics", "hl_pad_y", d.hl_pad_y),
            border_w: ini.i32("metrics", "border_width", d.border_w),
        };
        th
    }
}

/// 一份 INI 配置。段名大小写不敏感，键名精确匹配（fcitx 用驼峰键名）。
pub struct Ini {
    /// section（已转小写） → key → value
    map: HashMap<String, HashMap<String, String>>,
}

impl Ini {
    pub fn parse(text: &str) -> Ini {
        let mut map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                if let Some(end) = rest.find(']') {
                    section = rest[..end].trim().to_ascii_lowercase();
                }
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let mut v = v.trim();
                // 去掉行尾注释（fcitx 的配置文件里偶见 `Key=1 # 说明`）
                if let Some(p) = v.find(" #") {
                    v = v[..p].trim();
                }
                // 去掉成对引号
                if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
                    v = &v[1..v.len() - 1];
                }
                map.entry(section.clone()).or_default().insert(k.to_string(), v.to_string());
            }
        }
        Ini { map }
    }

    pub fn load(path: &Path) -> Option<Ini> {
        std::fs::read_to_string(path).ok().map(|s| Ini::parse(&s))
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.map.get(&section.to_ascii_lowercase())?.get(key).map(|s| s.as_str())
    }

    pub fn i32(&self, section: &str, key: &str, default: i32) -> i32 {
        self.get(section, key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    pub fn color(&self, section: &str, key: &str, default: u32) -> u32 {
        self.get(section, key).and_then(parse_color).unwrap_or(default)
    }

    /// 有 `[InputPanel]` 段就是 fcitx 主题
    pub fn is_fcitx(&self) -> bool {
        self.map.contains_key("inputpanel")
    }
}

/// 解析 `#RGB` / `#RRGGBB` / `#AARRGGBB`（fcitx 与 Qt 的约定，alpha 在前）
pub fn parse_color(s: &str) -> Option<u32> {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let n = |i: usize| u32::from_str_radix(&h[i..i + 1], 16).ok();
    let b = |i: usize| u32::from_str_radix(&h[i..i + 2], 16).ok();
    match h.len() {
        3 => Some(crate::render::argb(
            255,
            (n(0)? * 17) as u8,
            (n(1)? * 17) as u8,
            (n(2)? * 17) as u8,
        )),
        6 => Some(crate::render::argb(255, b(0)? as u8, b(2)? as u8, b(4)? as u8)),
        8 => Some(crate::render::argb(b(0)? as u8, b(2)? as u8, b(4)? as u8, b(6)? as u8)),
        _ => None,
    }
}

/// fcitx 主题目录（用户优先）
fn fcitx_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("XDG_DATA_HOME") {
        v.push(PathBuf::from(h).join("fcitx5/themes"));
    } else if let Some(h) = std::env::var_os("HOME") {
        v.push(PathBuf::from(h).join(".local/share/fcitx5/themes"));
    }
    v.push(PathBuf::from("/usr/share/fcitx5/themes"));
    v
}

fn xime_config_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(h).join("xime"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/xime"))
}

/// 按名字找 fcitx 主题的 theme.conf
pub fn find_fcitx(name: &str) -> Option<PathBuf> {
    for dir in fcitx_dirs() {
        let p = dir.join(name).join("theme.conf");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 列出所有能找到的 fcitx5 主题（用户目录优先，同名去重）
pub fn list_fcitx() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in fcitx_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut items: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        items.sort_by_key(|e| e.file_name());
        for e in items {
            let conf = e.path().join("theme.conf");
            if !conf.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !out.iter().any(|(n, _)| *n == name) {
                out.push((name, conf));
            }
        }
    }
    out
}

/// 把内部预乘颜色还原成 `#RRGGBB` / `#AARRGGBB`（打印用）
pub fn to_hex(c: u32) -> String {
    let a = (c >> 24) & 0xFF;
    let un = |v: u32| if a == 0 { 0 } else { (v * 255 / a).min(255) };
    let (r, g, b) = (un((c >> 16) & 0xFF), un((c >> 8) & 0xFF), un(c & 0xFF));
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{a:02x}{r:02x}{g:02x}{b:02x}")
    }
}

/// 按名字/路径解析主题。返回 None 表示这个名字不认识。
pub fn resolve_theme(spec: &str) -> Option<Theme> {
    match spec {
        "light" => return Some(Theme::light()),
        "dark" => return Some(Theme::dark()),
        _ => {}
    }
    if let Some(name) = spec.strip_prefix("fcitx:") {
        let p = find_fcitx(name)?;
        let ini = Ini::load(&p)?;
        return Some(Theme::from_fcitx(&ini, name));
    }
    // 当成文件路径
    if spec.contains('/') || spec.ends_with(".conf") || spec.ends_with(".ini") {
        let p = Path::new(spec);
        let ini = Ini::load(p)?;
        // fcitx 主题的文件名一律叫 theme.conf，名字得取所在目录
        let name = if ini.is_fcitx() {
            p.parent()
                .and_then(|d| d.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "theme".into())
        } else {
            p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        };
        return Some(if ini.is_fcitx() {
            Theme::from_fcitx(&ini, &name)
        } else {
            Theme::from_xime(&ini, &name)
        });
    }
    // 当成名字：先找 xime 主题目录，再找 fcitx
    if let Some(dir) = xime_config_dir() {
        for ext in ["ini", "conf"] {
            let p = dir.join("themes").join(format!("{spec}.{ext}"));
            if let Ok(text) = std::fs::read_to_string(&p) {
                let ini = Ini::parse(&text);
                return Some(if ini.is_fcitx() {
                    Theme::from_fcitx(&ini, spec)
                } else {
                    Theme::from_xime(&ini, spec)
                });
            }
        }
    }
    if let Some(p) = find_fcitx(spec) {
        let ini = Ini::load(&p)?;
        return Some(Theme::from_fcitx(&ini, spec));
    }
    None
}

/// 最终生效的设置
#[derive(Clone, Debug)]
pub struct Settings {
    pub layout: Layout,
    pub cols: usize,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { layout: Layout::Horizontal, cols: 3, theme: Theme::light() }
    }
}

impl Settings {
    /// 叠一份配置文件
    fn apply(&mut self, ini: &Ini, base: &Path) {
        if let Some(v) = ini.get("xime", "layout") {
            if let Some(l) = Layout::parse(v) {
                self.layout = l;
            }
        }
        if let Some(v) = ini.get("xime", "cols") {
            if let Ok(n) = v.parse::<usize>() {
                if (1..=9).contains(&n) {
                    self.cols = n;
                }
            }
        }
        if let Some(v) = ini.get("xime", "theme") {
            // 相对名按配置文件所在目录找，便于把主题和配置放在一起
            let cand = if v.contains('/') { PathBuf::from(v) } else { base.join(v) };
            if let Some(t) = resolve_theme(cand.to_str().unwrap_or(v)) {
                self.theme = t;
            } else if let Some(t) = resolve_theme(v) {
                self.theme = t;
            }
        }
        // 配置文件里直接写颜色/度量也是合法的（覆盖当前主题）
        if ini.map.contains_key("colors") {
            let mut t = Theme::from_xime(ini, &self.theme.name);
            // 只覆盖写了的键，其余沿用当前主题
            t.colors.bg = ini.color("colors", "bg", self.theme.colors.bg);
            t.colors.border = ini.color("colors", "border", self.theme.colors.border);
            t.colors.text = ini.color("colors", "text", self.theme.colors.text);
            t.colors.accent = ini.color("colors", "accent", self.theme.colors.accent);
            t.colors.sel_text = ini.color("colors", "selected_text", self.theme.colors.sel_text);
            self.theme.colors = t.colors;
        }
        if ini.map.contains_key("metrics") {
            let cur = self.theme.metrics;
            self.theme.metrics = Metrics {
                pad_x: ini.i32("metrics", "pad_x", cur.pad_x),
                pad_y: ini.i32("metrics", "pad_y", cur.pad_y),
                item_pad_x: ini.i32("metrics", "item_pad_x", cur.item_pad_x),
                item_gap: ini.i32("metrics", "item_gap", cur.item_gap),
                row_gap: ini.i32("metrics", "row_gap", cur.row_gap),
                radius: ini.i32("metrics", "radius", cur.radius),
                hl_radius: ini.i32("metrics", "hl_radius", cur.hl_radius),
                hl_pad_y: ini.i32("metrics", "hl_pad_y", cur.hl_pad_y),
                border_w: ini.i32("metrics", "border_width", cur.border_w),
            };
        }
    }

    /// 读全局配置：系统 → 用户 → 环境变量
    pub fn load() -> Settings {
        let mut s = Settings::default();

        if let Some(ini) = Ini::load(Path::new("/etc/xime/config.ini")) {
            s.apply(&ini, Path::new("/etc/xime"));
        }
        if let Some(dir) = xime_config_dir() {
            if let Some(ini) = Ini::load(&dir.join("config.ini")) {
                s.apply(&ini, &dir);
            }
        }
        if let Ok(v) = std::env::var("XIME_THEME") {
            match resolve_theme(&v) {
                Some(t) => s.theme = t,
                None => eprintln!("xime: XIME_THEME={v} 找不到对应主题，沿用默认"),
            }
        }
        s
    }
}

/// 会被热重载监视的配置文件
pub fn watched_paths() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/etc/xime/config.ini")];
    if let Some(dir) = xime_config_dir() {
        v.push(dir.join("config.ini"));
    }
    v
}

/// 配置文件指纹（mtime 秒 + 文件大小）。不变就说明没改过。
pub fn stamp() -> u64 {
    let mut h: u64 = 0;
    for p in watched_paths() {
        if let Ok(md) = std::fs::metadata(&p) {
            let t = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            h = h.wrapping_mul(1_000_003).wrapping_add(t).wrapping_add(md.len());
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_three_formats() {
        assert_eq!(parse_color("#fff"), Some(crate::render::argb(255, 255, 255, 255)));
        assert_eq!(parse_color("#1a1b26"), Some(crate::render::argb(255, 26, 27, 38)));
        // #AARRGGBB：alpha 在前
        assert_eq!(parse_color("#801a1b26"), Some(crate::render::argb(128, 26, 27, 38)));
        assert_eq!(parse_color("zzz"), None);
        assert_eq!(parse_color("#12345"), None);
    }

    #[test]
    fn ini_parse_sections_and_keys() {
        let ini = Ini::parse(
            "# 注释\n[InputPanel]\nNormalColor=#a9b1d6\n\n[InputPanel/TextMargin]\nLeft=12\nRight=12\n",
        );
        assert_eq!(ini.get("InputPanel", "NormalColor"), Some("#a9b1d6"));
        // 段名大小写不敏感
        assert_eq!(ini.get("inputpanel", "NormalColor"), Some("#a9b1d6"));
        assert_eq!(ini.i32("InputPanel/TextMargin", "Left", 0), 12);
        assert!(ini.is_fcitx());
    }

    #[test]
    fn fcitx_theme_mapping() {
        let ini = Ini::parse(
            "[InputPanel]\n\
             NormalColor=#a9b1d6\n\
             HighlightCandidateColor=#1a1b26\n\
             HighlightBackgroundColor=#7aa2f7\n\
             [InputPanel/TextMargin]\nLeft=12\n\
             [InputPanel/ContentMargin]\nLeft=6\nTop=6\n\
             [InputPanel/Background]\nColor=#1a1b26\nBorderColor=#292e42\nBorderWidth=2\n\
             [InputPanel/Background/Margin]\nLeft=2\nTop=2\n\
             [InputPanel/Highlight/Margin]\nTop=5\n",
        );
        let t = Theme::from_fcitx(&ini, "omarchy");
        assert_eq!(t.colors.text, crate::render::argb(255, 0xa9, 0xb1, 0xd6));
        assert_eq!(t.colors.accent, crate::render::argb(255, 0x7a, 0xa2, 0xf7));
        assert_eq!(t.colors.sel_text, crate::render::argb(255, 0x1a, 0x1b, 0x26));
        // 内边距是 ContentMargin + Background/Margin 叠加
        assert_eq!(t.metrics.pad_x, 8);
        assert_eq!(t.metrics.pad_y, 8);
        assert_eq!(t.metrics.item_pad_x, 12);
        assert_eq!(t.metrics.hl_pad_y, 5);
        assert_eq!(t.metrics.border_w, 2);
        // 深色背景判定为 dark 主题
        assert!(t.dark);
    }

    #[test]
    fn xime_format_only_overrides_written_keys() {
        let ini = Ini::parse("[colors]\nbg=#112233\n[metrics]\npad_x=20\n");
        let t = Theme::from_xime(&ini, "t");
        assert_eq!(t.colors.bg, crate::render::argb(255, 0x11, 0x22, 0x33));
        // 没写的 key 保持内置默认
        assert_eq!(t.metrics.pad_x, 20);
        assert_eq!(t.metrics.pad_y, Metrics::default().pad_y);
    }

    #[test]
    fn default_theme_matches_hardcoded() {
        let d = Metrics::default();
        assert_eq!(
            (d.pad_x, d.pad_y, d.item_pad_x, d.item_gap, d.row_gap, d.radius),
            (crate::popup::PAD_X, crate::popup::PAD_Y, crate::popup::ITEM_PAD_X,
             crate::popup::ITEM_GAP, crate::popup::ROW_GAP, crate::popup::RADIUS)
        );
        assert_eq!(Theme::default().colors, colors(false));
    }
}
