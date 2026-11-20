//! keymap 解析：把合成器发来的 XKB keymap 文本变成「keycode → keysym」表。
//!
//! C 版用 libxkbcommon 做这件事，但那是一个 400KB 的 C 库，静态链接会破坏
//! "单文件零依赖"。而输入法真正需要的只是：拿到某个 keycode 在当前 shift/caps
//! 状态下产生的 keysym，用来判断「这是字母 q 还是数字 1」。这不需要完整的 XKB
//! 类型系统，只需要 keycodes 段的命名映射和 symbols 段的前两列。
//!
//! 解析失败时回退到内置 US 布局表——绝大多数中文用户使用 US 布局。

// 常用 keysym 值（X11 keysymdef.h）
pub const KS_BACKSPACE: u32 = 0xff08;
pub const KS_TAB: u32 = 0xff09;
pub const KS_RETURN: u32 = 0xff0d;
pub const KS_ESCAPE: u32 = 0xff1b;
pub const KS_DELETE: u32 = 0xffff;
/// 翻页键：`-` / `=`（fcitx5 拼音默认绑定，与 PageUp/PageDown 等价）
pub const KS_MINUS: u32 = 0x2d;
pub const KS_EQUAL: u32 = 0x3d;
pub const KS_HOME: u32 = 0xff50;
pub const KS_LEFT: u32 = 0xff51;
pub const KS_UP: u32 = 0xff52;
pub const KS_RIGHT: u32 = 0xff53;
pub const KS_DOWN: u32 = 0xff54;
pub const KS_PAGE_UP: u32 = 0xff55;
pub const KS_PAGE_DOWN: u32 = 0xff56;
pub const KS_END: u32 = 0xff57;
pub const KS_SHIFT_L: u32 = 0xffe1;
pub const KS_SHIFT_R: u32 = 0xffe2;
pub const KS_CONTROL_L: u32 = 0xffe3;
pub const KS_CONTROL_R: u32 = 0xffe4;
pub const KS_CAPS_LOCK: u32 = 0xffe5;
pub const KS_ALT_L: u32 = 0xffe9;
pub const KS_ALT_R: u32 = 0xffea;
pub const KS_SUPER_L: u32 = 0xffeb;
pub const KS_SUPER_R: u32 = 0xffec;

/// 修饰键位（XKB 标准 real modifier 索引）
pub const MASK_SHIFT: u32 = 1 << 0;
pub const MASK_LOCK: u32 = 1 << 1;
pub const MASK_CONTROL: u32 = 1 << 2;
pub const MASK_MOD1: u32 = 1 << 3; // Alt
pub const MASK_MOD4: u32 = 1 << 6; // Super

const MAX_KEYS: usize = 1024;

pub struct Keymap {
    /// 索引为 xkb keycode（= evdev + 8），值为该键的两列 keysym
    keys: Vec<[u32; 2]>,
}

impl Keymap {
    /// 解析 XKB keymap 文本。只依赖 keycodes 与 symbols 两段。
    pub fn parse(text: &str) -> Option<Keymap> {
        let mut names: Vec<(String, u32)> = Vec::new(); // <AD01> -> keycode
        let mut syms: Vec<(String, [u32; 2])> = Vec::new();

        // 逐行扫描。不能按 ';' 切语句：xkbcomp 的输出里 `xkb_keymap {` 和
        // `xkb_keycodes "..." {` 之间没有分号，会落进同一段，导致 section 判断失效。
        let mut section = String::new();
        // 一个 key 定义可能跨行（多列 keysym 时），用 pending 累积
        let mut pending = String::new();

        for raw in text.lines() {
            let line = strip_comment(raw);
            let t = line.trim();
            if t.is_empty() {
                continue;
            }

            if t.contains("xkb_keycodes") {
                section = "keycodes".into();
                continue;
            }
            if t.contains("xkb_symbols") {
                section = "symbols".into();
                continue;
            }
            if t.contains("xkb_types")
                || t.contains("xkb_compatibility")
                || t.contains("xkb_geometry")
            {
                section = "other".into();
                continue;
            }

            if section == "keycodes" {
                // xkbcommon 输出的是对齐格式 `<AE01>               = 10;`：
                // `>` 与 `=` 之间是一串空格，按 "> =" 找永远匹配不到。
                if let Some((name, code)) = parse_keycode_line(t) {
                    names.push((name, code));
                }
            } else if section == "symbols" {
                if !pending.is_empty() {
                    pending.push(' ');
                    pending.push_str(t);
                    if pending.contains(']') {
                        if let Some(pair) = parse_key_line(&pending) {
                            syms.push(pair);
                        }
                        pending.clear();
                    }
                    continue;
                }
                if t.contains("key <") {
                    if t.contains(']') {
                        if let Some(pair) = parse_key_line(t) {
                            syms.push(pair);
                        }
                    } else {
                        pending = t.to_string();
                    }
                }
            }
        }

        if std::env::var("XIME_KM_DEBUG").is_ok() {
            eprintln!("[km] names={} syms={}", names.len(), syms.len());
            for (n, v) in names.iter().take(3) {
                eprintln!("[km]   name {n} = {v}");
            }
            for (n, v) in syms.iter().take(3) {
                eprintln!("[km]   sym {n} = {:x?}", v);
            }
        }
        if syms.is_empty() {
            return None;
        }

        // 名字 → keycode：symbols 里用名字引用，keycodes 段给出编号
        let mut keys = vec![[0u32; 2]; MAX_KEYS];
        let mut filled = 0;
        for (name, cols) in &syms {
            let code = match names.iter().find(|(n, _)| n == name) {
                Some((_, v)) => *v,
                // keycodes 段缺失该名字时，尝试按 evdev 常见名推断（AD01=24 这种规律无法推断，跳过）
                None => continue,
            };
            if (code as usize) < MAX_KEYS {
                keys[code as usize] = *cols;
                filled += 1;
            }
        }

        if std::env::var("XIME_KM_DEBUG").is_ok() {
            eprintln!("[km] filled={filled}");
        }
        if filled < 20 {
            return None;
        }
        Some(Keymap { keys })
    }

    /// 内置 US 布局（evdev → keysym 直接给出 ASCII/功能键）
    pub fn us_layout() -> Keymap {
        let mut keys = vec![[0u32; 2]; MAX_KEYS];

        // 数字行
        const NUM: [(u32, char, char); 10] = [
            (2, '1', '!'),
            (3, '2', '@'),
            (4, '3', '#'),
            (5, '4', '$'),
            (6, '5', '%'),
            (7, '6', '^'),
            (8, '7', '&'),
            (9, '8', '*'),
            (10, '9', '('),
            (11, '0', ')'),
        ];
        for (code, a, b) in NUM {
            keys[(code + 8) as usize] = [a as u32, b as u32];
        }

        // 字母三行（evdev 码）
        const ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
        const STARTS: [u32; 3] = [16, 30, 44];
        for (r, row) in ROWS.iter().enumerate() {
            for (i, ch) in row.chars().enumerate() {
                let code = STARTS[r] + i as u32;
                let up = ch.to_ascii_uppercase();
                keys[(code + 8) as usize] = [ch as u32, up as u32];
            }
        }

        // 标点
        const PUNCT: [(u32, u32, u32); 12] = [
            (12, b'-' as u32, b'_' as u32),
            (13, b'=' as u32, b'+' as u32),
            (26, b'[' as u32, b'{' as u32),
            (27, b']' as u32, b'}' as u32),
            (39, b';' as u32, b':' as u32),
            (40, b'\'' as u32, b'"' as u32),
            (41, b'`' as u32, b'~' as u32),
            (43, b'\\' as u32, b'|' as u32),
            (51, b',' as u32, b'<' as u32),
            (52, b'.' as u32, b'>' as u32),
            (53, b'/' as u32, b'?' as u32),
            (57, b' ' as u32, b' ' as u32),
        ];
        for (code, a, b) in PUNCT {
            keys[(code + 8) as usize] = [a, b];
        }

        // 功能键
        const FUNC: [(u32, u32); 20] = [
            (1, KS_ESCAPE),
            (14, KS_BACKSPACE),
            (15, KS_TAB),
            (28, KS_RETURN),
            (29, KS_CONTROL_L),
            (42, KS_SHIFT_L),
            (54, KS_SHIFT_R),
            (56, KS_ALT_L),
            (97, KS_CONTROL_R),
            (100, KS_ALT_R),
            (105, KS_LEFT),
            (106, KS_RIGHT),
            (103, KS_UP),
            (108, KS_DOWN),
            (104, KS_PAGE_UP),
            (109, KS_PAGE_DOWN),
            (102, KS_HOME),
            (107, KS_END),
            (111, KS_DELETE),
            (58, KS_CAPS_LOCK),
        ];
        for (code, sym) in FUNC {
            keys[(code + 8) as usize] = [sym, sym];
        }
        keys[(125 + 8) as usize] = [KS_SUPER_L, KS_SUPER_L];
        keys[(126 + 8) as usize] = [KS_SUPER_R, KS_SUPER_R];

        Keymap { keys }
    }

    pub fn count(&self) -> usize {
        self.keys.iter().filter(|k| k[0] != 0).count()
    }

    /// 取某个 evdev keycode 在给定修饰状态下的 keysym
    pub fn sym(&self, evdev: u32, mods: u32) -> u32 {
        let idx = (evdev as usize) + 8;
        if idx >= self.keys.len() {
            return 0;
        }
        let base = self.keys[idx][0];
        if base == 0 {
            return 0;
        }
        let shift = mods & MASK_SHIFT != 0;
        let caps = mods & MASK_LOCK != 0;
        let upper = self.keys[idx][1];

        // 字母键受 CapsLock 与 Shift 共同影响，其它键只看 Shift
        let is_alpha = (base >= 'a' as u32 && base <= 'z' as u32)
            || (base >= 'A' as u32 && base <= 'Z' as u32);
        let want_alt = if is_alpha {
            shift ^ caps
        } else {
            shift
        };
        if want_alt && upper != 0 {
            upper
        } else {
            base
        }
    }
}

/// 去掉 `//` 与 `#` 注释
fn strip_comment(l: &str) -> &str {
    let l = match l.find("//") {
        Some(i) => &l[..i],
        None => l,
    };
    match l.find('#') {
        Some(i) => &l[..i],
        None => l,
    }
}

/// 解析 keycodes 段的 `<AD01>               = 24;`
fn parse_keycode_line(s: &str) -> Option<(String, u32)> {
    let rest = s.strip_prefix('<')?;
    let end = rest.find('>')?;
    let name = rest[..end].to_string();
    // '>' 之后是空白对齐，再是 '='
    let tail = rest[end + 1..].trim_start();
    let tail = tail.strip_prefix('=')?.trim_start();
    let num: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    let code = num.parse::<u32>().ok()?;
    Some((name, code))
}

/// 解析 `key <AD01> { [ 0x71, 0x51 ] };`
fn parse_key_line(s: &str) -> Option<(String, [u32; 2])> {
    let i = s.find("key <")?;
    let rest = &s[i + 5..];
    let end = rest.find('>')?;
    let name = rest[..end].to_string();
    let body = &rest[end + 1..];
    let cols = parse_first_bracket(body)?;
    Some((name, cols))
}

/// 从 `{ [ q, Q ] }` 里取出前两列 keysym
fn parse_first_bracket(body: &str) -> Option<[u32; 2]> {
    // symbols[Group1]=[ ... ] 或直接 [ ... ]
    let open = body.find('[')?;
    let close = body[open..].find(']')? + open;
    let inner = &body[open + 1..close];

    let mut out = [0u32; 2];
    for (i, tok) in inner.split(',').take(2).enumerate() {
        out[i] = sym_value(tok.trim());
    }
    // 只有一个 keysym 时，让它对两个状态都生效
    if out[1] == 0 {
        out[1] = out[0];
    }
    if out[0] == 0 && out[1] == 0 {
        return None;
    }
    Some(out)
}

/// keysym 名字或字面字符 → keysym 值
fn sym_value(tok: &str) -> u32 {
    let t = tok.trim().trim_matches('"');
    if t.is_empty() {
        return 0;
    }
    // xkbcomp 反编译后 keysym 是十六进制数值（0x71），而不是名字（q）
    if let Some(hex) = t.strip_prefix("0x") {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return v;
        }
    }
    // 单字符字面量：'q' 或 q
    let chars: Vec<char> = t.chars().collect();
    if chars.len() == 1 {
        return chars[0] as u32;
    }
    if chars.len() == 3 && chars[0] == '\'' && chars[2] == '\'' {
        return chars[1] as u32;
    }

    // 常见名字表。输入法只需要字母数字、标点与功能键，不需要全部 keysym。
    match t {
        "space" => 0x20,
        "exclam" => 0x21,
        "quotedbl" => 0x22,
        "numbersign" => 0x23,
        "dollar" => 0x24,
        "percent" => 0x25,
        "ampersand" => 0x26,
        "apostrophe" => 0x27,
        "parenleft" => 0x28,
        "parenright" => 0x29,
        "asterisk" => 0x2a,
        "plus" => 0x2b,
        "comma" => 0x2c,
        "minus" => 0x2d,
        "period" => 0x2e,
        "slash" => 0x2f,
        "colon" => 0x3a,
        "semicolon" => 0x3b,
        "less" => 0x3c,
        "equal" => 0x3d,
        "greater" => 0x3e,
        "question" => 0x3f,
        "at" => 0x40,
        "bracketleft" => 0x5b,
        "backslash" => 0x5c,
        "bracketright" => 0x5d,
        "asciicircum" => 0x5e,
        "underscore" => 0x5f,
        "grave" => 0x60,
        "braceleft" => 0x7b,
        "bar" => 0x7c,
        "braceright" => 0x7d,
        "asciitilde" => 0x7e,
        "BackSpace" => KS_BACKSPACE,
        "Tab" => KS_TAB,
        "Return" => KS_RETURN,
        "Escape" => KS_ESCAPE,
        "Delete" => KS_DELETE,
        "Home" => KS_HOME,
        "Left" => KS_LEFT,
        "Up" => KS_UP,
        "Right" => KS_RIGHT,
        "Down" => KS_DOWN,
        "Page_Up" => KS_PAGE_UP,
        "Page_Down" => KS_PAGE_DOWN,
        "End" => KS_END,
        "Shift_L" => KS_SHIFT_L,
        "Shift_R" => KS_SHIFT_R,
        "Control_L" => KS_CONTROL_L,
        "Control_R" => KS_CONTROL_R,
        "Caps_Lock" => KS_CAPS_LOCK,
        "Alt_L" => KS_ALT_L,
        "Alt_R" => KS_ALT_R,
        "Super_L" => KS_SUPER_L,
        "Super_R" => KS_SUPER_R,
        "ISO_Level3_Shift" => 0xfe03,
        "Multi_key" => 0xff20,
        // Unicode keysym 前缀形式：U4E2D
        _ => {
            if let Some(hex) = t.strip_prefix('U') {
                if let Ok(cp) = u32::from_str_radix(hex, 16) {
                    return 0x0100_0000 + cp;
                }
            }
            0
        }
    }
}
