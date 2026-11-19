mod config;
mod dict;
mod en;
mod engine;
mod fcitx_dbus;
mod font;
mod keymap;
mod popup;
mod proto;
mod render;
mod shm;
mod single_instance;
mod wire;
mod zh;

use engine::Engine;
use keymap::Keymap;
use render::Canvas;
use shm::Shm;
use wire::{Conn, Incoming, DISPLAY_ID};

const VERSION: &str = "0.3.0";
const SHM_FORMAT_ARGB8888: u32 = 0;
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
const KEY_PRESSED: u32 = 1;
const CONSUMED_MAX: usize = 768;

struct Buffer {
    shm: Shm,
    pool: u32,
    buffer: u32,
    w: i32,
    h: i32,
}

struct Xime {
    conn: Conn,
    reg: u32,
    comp: u32,
    comp_ver: u32,
    shm_global: u32,
    seat: u32,
    im_manager: u32,
    vk_manager: u32,
    im: u32,
    grab: u32,
    popup_surf: u32,
    popup_role: u32,
    vk: u32,
    buf: Option<Buffer>,
    visible: bool,
    done_serial: u32,
    engine: Engine,
    km: Option<Keymap>,
    keymap_raw: Vec<u8>,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
    consumed: Vec<bool>,
    chinese: bool,
    /// 英文模式的组字状态（前缀补全）
    en: en::EnEngine,
    /// 英文模式下完全透传按键，不做补全（回归「能用就行」的做法）
    en_passthrough: bool,
    /// 当前焦点是密码框（content_purpose=password/pin）：任何模式都全键透传
    field_passthrough: bool,
    /// 当前焦点是终端（content_purpose=terminal）：英文模式退化为纯透传，
    /// 中文模式照常组字——终端里英文补全只有坏处
    terminal_purpose: bool,
    /// Shift 按下但还没松开：松开时才决定是切换中英还是输入大写字母
    shift_pending: bool,
    /// 调试钩子（XIME_SELFTEST）：激活后自动打一段拼音并上屏，只跑一次
    selftest_done: bool,
    inline_cands: bool,
    verbose: bool,
    style: popup::Style,
    /// 输出形态（简体 / 繁体），运行时可切
    variant: zh::Variant,
    running: bool,
    /// 启动时的命令行参数：配置热重载后要用它重新叠加 CLI 覆盖
    args: Vec<String>,
    /// 配置文件指纹与上次检查时间（热重载用）
    cfg_stamp: u64,
    cfg_checked: Option<std::time::Instant>,
}

impl Xime {
    fn new(
        inline_cands: bool,
        verbose: bool,
        style: popup::Style,
        variant: zh::Variant,
        start_english: bool,
        en_passthrough: bool,
        args: Vec<String>,
    ) -> Option<Xime> {
        let conn = match Conn::connect() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("xime: 无法连接 Wayland 合成器（检查 WAYLAND_SOCKET / WAYLAND_DISPLAY / XDG_RUNTIME_DIR）: {e}");
                return None;
            }
        };
        dict::init();
        let mut engine = Engine::new();
        engine.set_variant(variant);
        Some(Xime {
            conn,
            reg: 0,
            comp: 0,
            comp_ver: 0,
            shm_global: 0,
            seat: 0,
            im_manager: 0,
            vk_manager: 0,
            im: 0,
            grab: 0,
            popup_surf: 0,
            popup_role: 0,
            vk: 0,
            buf: None,
            visible: false,
            done_serial: 0,
            engine,
            km: None,
            keymap_raw: Vec::new(),
            mods_depressed: 0,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
            consumed: vec![false; CONSUMED_MAX],
            chinese: !start_english,
            en: en::EnEngine::new(),
            en_passthrough,
            field_passthrough: false,
            terminal_purpose: false,
            shift_pending: false,
            selftest_done: false,
            inline_cands,
            verbose,
            style,
            variant,
            running: true,
            args,
            cfg_stamp: config::stamp(),
            cfg_checked: None,
        })
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            println!("xime: {msg}");
        }
    }

    fn run(&mut self) -> std::io::Result<()> {
        self.reg = self.conn.alloc_id();
        proto::get_registry(&mut self.conn, self.reg)?;

        while self.running {
            // 先把已读进缓冲的消息全部处理完，再去阻塞等待
            while let Some(m) = self.conn.next_msg() {
                if let Err(e) = self.dispatch(m) {
                    eprintln!("xime: 协议错误: {e}");
                    return Err(e);
                }
            }
            if !self.running {
                break;
            }
            if self.conn.wait(-1)? {
                self.conn.fill()?;
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, mut m: Incoming) -> std::io::Result<()> {
        if m.obj == DISPLAY_ID && m.opcode == 0 {
            let _obj = m.args.u32();
            let code = m.args.u32();
            let msg = m.args.string().unwrap_or_default();
            eprintln!("xime: 合成器报告协议错误 code={code}: {msg}");
            self.running = false;
            return Ok(());
        }
        if m.obj == self.reg {
            if m.opcode == 0 {
                let name = m.args.u32();
                let iface = m.args.string().unwrap_or_default();
                let version = m.args.u32();
                self.on_global(name, &iface, version)?;
            }
            return Ok(());
        }
        if self.im != 0 && m.obj == self.im {
            return self.on_im(m.opcode, &mut m);
        }
        if self.grab != 0 && m.obj == self.grab {
            return self.on_grab(m.opcode, &mut m);
        }
        if self.popup_role != 0 && m.obj == self.popup_role {
            // text_input_rectangle：光标矩形，合成器用它定位候选窗，这里只需记录
            if m.opcode == 0 && self.verbose {
                let (x, y, w, h) = (m.args.i32(), m.args.i32(), m.args.i32(), m.args.i32());
                println!("xime: 光标矩形 {x},{y} {w}x{h}");
            }
            return Ok(());
        }
        Ok(())
    }

    fn on_global(&mut self, name: u32, iface: &str, version: u32) -> std::io::Result<()> {
        match iface {
            proto::IFACE_COMPOSITOR => {
                self.comp_ver = version.min(4);
                self.comp = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, self.comp_ver, self.comp)?;
            }
            proto::IFACE_SHM => {
                self.shm_global = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, 1, self.shm_global)?;
            }
            proto::IFACE_SEAT => {
                self.seat = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, version.min(5), self.seat)?;
            }
            proto::IFACE_INPUT_METHOD => {
                self.im_manager = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, 1, self.im_manager)?;
            }
            proto::IFACE_VIRTUAL_KEYBOARD => {
                self.vk_manager = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, 1, self.vk_manager)?;
            }
            _ => {
                self.log(&format!("忽略全局对象 {iface} v{version}"));
                return Ok(());
            }
        }
        self.log(&format!("绑定 {iface} v{version}"));
        self.try_init()
    }

    /// seat 与 input-method manager 都到位后才能创建输入法对象
    fn try_init(&mut self) -> std::io::Result<()> {
        if self.im != 0 || self.im_manager == 0 || self.seat == 0 {
            return Ok(());
        }
        let id = self.conn.alloc_id();
        proto::im_get_input_method(&mut self.conn, self.im_manager, self.seat, id)?;
        self.im = id;
        self.log("已获取输入法实例");
        Ok(())
    }

    fn on_im(&mut self, opcode: u16, m: &mut Incoming) -> std::io::Result<()> {
        match opcode {
            0 => {
                // activate
                self.log("activate");
                self.engine.reset();
                self.consumed.iter_mut().for_each(|c| *c = false);
                self.popup_create()?;
                if self.grab == 0 {
                    let id = self.conn.alloc_id();
                    proto::im_grab_keyboard(&mut self.conn, self.im, id)?;
                    self.grab = id;
                }
                proto::im_set_preedit(&mut self.conn, self.im, "", 0, 0)?;
                proto::im_commit(&mut self.conn, self.im, self.done_serial)?;
            }
            1 => {
                self.log("deactivate");
                self.engine.reset();
                self.popup_hide()?;
                proto::im_set_preedit(&mut self.conn, self.im, "", 0, 0)?;
                proto::im_commit(&mut self.conn, self.im, self.done_serial)?;
            }
            4 => {
                // content_type(hint, purpose)：应用声明的输入框类型。
                // password=8 / pin=9 → 全键透传；terminal=14 → 英文态透传。
                let hint = m.args.u32();
                let purpose = m.args.u32();
                let pw = purpose == 8 || purpose == 9;
                let term = purpose == 14;
                self.log(&format!("content_type: hint={hint} purpose={purpose}"));
                if pw && !self.field_passthrough {
                    // 进入密码框：清掉组字残留，避免半截拼音/补全突然上屏
                    self.engine.reset();
                    self.en.reset();
                    self.popup_hide()?;
                }
                self.field_passthrough = pw;
                self.terminal_purpose = term;
                self.selftest()?;
            }
            5 => {
                // done：每收到一次，可用的 commit serial 上限加一
                self.done_serial += 1;
            }
            6 => {
                eprintln!(
                    "xime: 合成器报告 input-method 不可用（很可能已有另一个输入法占用了这个 seat）"
                );
                self.running = false;
            }
            _ => {}
        }
        Ok(())
    }

    fn on_grab(&mut self, opcode: u16, m: &mut Incoming) -> std::io::Result<()> {
        match opcode {
            0 => {
                // keymap(format, fd, size)
                let _format = m.args.u32();
                let fd = self.conn.take_fd();
                let size = m.args.u32() as usize;
                if fd >= 0 && size > 0 {
                    let raw = read_fd(fd, size);
                    unsafe { libc::close(fd) };
                    if let Ok(bytes) = raw {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        self.log(&format!("keymap 事件：声明 {size} 字节，实读 {} 字节", bytes.len()));
                        match Keymap::parse(&text) {
                            Some(k) => {
                                self.log(&format!("keymap 已解析（{} 键）", k.count()));
                                self.km = Some(k);
                                self.keymap_raw = bytes;
                            }
                            None => {
                                // 有些合成器（如 Hyprland 在某些 grab 场景）会送来只有
                                // keycodes、没有 symbols 的残缺 keymap。此时保留上一份能用的
                                // keymap —— 丢掉它会让所有按键解释崩掉（非 US 布局、
                                // 修饰键全错），比不做任何事更糟。
                                let head: String = text.chars().take(40).collect();
                                if self.km.is_some() {
                                    self.log(&format!(
                                        "keymap 解析失败（开头: {head:?}），继续沿用上一份 keymap"
                                    ));
                                } else {
                                    self.log(&format!(
                                        "keymap 解析失败（开头: {head:?}），回退内置 US 布局"
                                    ));
                                    self.km = Some(Keymap::us_layout());
                                }
                                if std::env::var_os("XIME_DUMP_KEYMAP").is_some() {
                                    let _ = std::fs::write("/tmp/xime-keymap-fail.txt", &text);
                                }
                            }
                        }
                    }
                }
                // 把同一份 keymap 同步给虚拟键盘，否则回注的按键没有解释依据
                let raw = self.keymap_raw.clone();
                if !raw.is_empty() {
                    self.vk_send_keymap(&raw)?;
                }
            }
            1 => {
                // key(serial, time, key, state)
                let _serial = m.args.u32();
                let time = m.args.u32();
                let key = m.args.u32();
                let state = m.args.u32();
                self.log(&format!("key code={key} state={state}"));
                self.on_key(time, key, state)?;
            }
            2 => {
                // modifiers(serial, depressed, latched, locked, group)
                let _serial = m.args.u32();
                self.mods_depressed = m.args.u32();
                self.mods_latched = m.args.u32();
                self.mods_locked = m.args.u32();
                self.group = m.args.u32();
            }
            3 => {
                // repeat_info：不需要自己重复，合成器会重复发 key
                let _rate = m.args.i32();
                let _delay = m.args.i32();
            }
            _ => {}
        }
        Ok(())
    }

    fn on_key(&mut self, time: u32, key: u32, state: u32) -> std::io::Result<()> {
        let pressed = state == KEY_PRESSED;
        let idx = key as usize;
        let sym = match &self.km {
            Some(km) => km.sym(key, self.mods_depressed),
            None => 0,
        };

        if !pressed {
            // 密码框：按下与松开都原样转发，修饰键状态也不能被截留
            if self.field_passthrough {
                self.forward_key(time, key, state)?;
                return Ok(());
            }
            if idx < CONSUMED_MAX && self.consumed[idx] {
                self.consumed[idx] = false;
                // Shift 按下又原样松开、中间没敲别的键：这时才确定是「切换中英」。
                // 不能在按下瞬间就切，否则 Shift+A 想打大写字母会先被切成中文。
                if is_shift_key(key) && self.shift_pending {
                    self.shift_pending = false;
                    self.toggle_language()?;
                }
            } else {
                self.forward_key(time, key, state)?;
            }
            return Ok(());
        }

        let has_cmd = self.mods_depressed
            & (keymap::MASK_CONTROL | keymap::MASK_MOD1 | keymap::MASK_MOD4)
            != 0;

        // 密码框（按下路径）：不进任何组字/切换逻辑，直接透传
        if self.field_passthrough {
            self.forward_key(time, key, state)?;
            return Ok(());
        }

        // Shift 按下：先挂起，等松开时才知道是修饰键还是切换键
        if is_shift_key(key) && !has_cmd {
            self.shift_pending = true;
            if idx < CONSUMED_MAX {
                self.consumed[idx] = true;
            }
            return Ok(());
        }
        self.shift_pending = false;

        // Ctrl+Shift+F：简体 / 繁体切换（与 fcitx5 同组按键）。
        // 用按键切换而不是命令行参数，是因为形态属于「随手改」的输出偏好：
        // 同一个词在不同语境下要的写法不一样，重启输入法太笨重。
        if self.chinese && is_variant_toggle(sym, self.mods_depressed) {
            self.variant = self.variant.toggled();
            self.engine.set_variant(self.variant);
            self.refresh()?;
            if idx < CONSUMED_MAX {
                self.consumed[idx] = true;
            }
            self.log(&format!("输出形态切换为{}", self.variant.label()));
            return Ok(());
        }

        if has_cmd {
            self.forward_key(time, key, state)?;
            return Ok(());
        }

        // 英文模式：独立的按键处理（前缀补全），与拼音引擎不共用
        if !self.chinese {
            return self.on_key_en(time, key, state, sym);
        }

        // 数字选词
        if (b'1' as u32..=b'9' as u32).contains(&sym) {
            if self.engine.active() {
                let pick = (sym - b'1' as u32) as usize;
                let text = self
                    .engine
                    .cands
                    .get(pick)
                    .cloned()
                    .unwrap_or_else(|| self.engine.input.clone());
                self.commit_text(&text)?;
                if idx < CONSUMED_MAX {
                    self.consumed[idx] = true;
                }
                return Ok(());
            }
            self.forward_key(time, key, state)?;
            return Ok(());
        }

        match sym {
            keymap::KS_BACKSPACE => {
                if self.engine.active() {
                    self.engine.backspace();
                    self.refresh()?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
            x if x == b' ' as u32 => {
                if !self.engine.cands.is_empty() || self.engine.active() {
                    self.commit_selection()?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
            keymap::KS_RETURN => {
                if self.engine.active() {
                    let t = self.engine.input.clone();
                    self.commit_text(&t)?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
            keymap::KS_ESCAPE => {
                if self.engine.active() {
                    self.engine.reset();
                    self.refresh()?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
            _ => {
                // 字母：进组字缓冲
                if let Some(ch) = ascii_letter(sym) {
                    self.engine.push_char(ch);
                    self.refresh()?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                // 数字（无输入串时）交给应用
                if (b'0' as u32..=b'9' as u32).contains(&sym) {
                    self.forward_key(time, key, state)?;
                    return Ok(());
                }
                // 标点：先把候选上屏，再上屏中文标点
                if let Some(p) = punct_of(sym) {
                    self.commit_selection()?;
                    self.commit_text(p)?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
        }
        Ok(())
    }

    /// 英文模式的按键处理：字母进缓冲、候选实时刷新
    ///
    /// 与中文共用「缓冲 + 候选窗 + 空格上屏」这套流程，所以数字选词、退格、Esc
    /// 的行为和中文完全一致，用户不用学两套。差别只在候选来源：那边是词网格解码，
    /// 这边是前缀补全。
    fn on_key_en(
        &mut self,
        time: u32,
        key: u32,
        state: u32,
        sym: u32,
    ) -> std::io::Result<()> {
        let idx = key as usize;
        let cap = self.style.max_items();

        // 完全透传模式（显式参数或终端焦点）：不组字、不弹候选，字母直接进应用
        if self.en_passthrough || self.terminal_purpose {
            self.forward_key(time, key, state)?;
            return Ok(());
        }

        match sym {
            keymap::KS_BACKSPACE if self.en.active() => {
                self.en.backspace(cap);
                self.refresh()?;
                self.consume_key(idx);
            }
            x if x == b' ' as u32 && self.en.active() => {
                // 空格确认补全/上屏缓冲词，随后空格本身照常发给应用——
                // 「hel<空格>」得到的是「hello 」而不是把空格吞掉
                self.commit_selection()?;
                self.forward_key(time, key, state)?;
            }
            keymap::KS_RETURN if self.en.active() => {
                // 回车上屏「我刚敲的字母」，并把回车本身也转发——
                // 否则 sudo 密码输完按回车，应用永远收不到提交
                let t = self.en.raw();
                self.commit_text(&t)?;
                self.forward_key(time, key, state)?;
            }
            keymap::KS_ESCAPE if self.en.active() => {
                self.en.reset();
                self.refresh()?;
                self.consume_key(idx);
            }
            _ => {
                if let Some(ch) = letter_keep_case(sym) {
                    self.en.set_caps(self.caps_on());
                    self.en.push_char(ch, cap);
                    self.refresh()?;
                    self.consume_key(idx);
                } else {
                    // 标点、符号、方向键：英文态不该被输入法改写，直接透传
                    self.forward_key(time, key, state)?;
                }
            }
        }
        Ok(())
    }

    /// 标记这个键已被输入法吃掉，松开时不再转发
    fn consume_key(&mut self, idx: usize) {
        if idx < CONSUMED_MAX {
            self.consumed[idx] = true;
        }
    }

    /// Caps Lock 是否生效（锁定态优先，某些合成器只报 depressed）
    fn caps_on(&self) -> bool {
        (self.mods_locked | self.mods_depressed) & keymap::MASK_LOCK != 0
    }

    /// 中英切换：先把当前未上屏的内容定下来，再清空两边的组字状态
    fn toggle_language(&mut self) -> std::io::Result<()> {
        self.commit_selection()?;
        self.chinese = !self.chinese;
        self.engine.reset();
        self.en.reset();
        self.popup_hide()?;
        self.log(if self.chinese {
            "切换到中文模式"
        } else if self.en_passthrough {
            "切换到英文模式（直通）"
        } else {
            "切换到英文模式（前缀补全）"
        });
        Ok(())
    }

    /// 提交首选候选；没有候选但有输入串时提交原始拼音
    fn commit_selection(&mut self) -> std::io::Result<()> {
        if !self.chinese {
            if let Some(s) = self.en.cands.first().cloned() {
                return self.commit_text(&s);
            }
            if self.en.active() {
                return self.commit_text(&self.en.raw());
            }
            return Ok(());
        }
        if let Some(s) = self.engine.cands.first().cloned() {
            return self.commit_text(&s);
        }
        if self.engine.active() {
            let t = self.engine.input.clone();
            return self.commit_text(&t);
        }
        Ok(())
    }

    /// 调试钩子：`XIME_SELFTEST=nihao` 时，第一个文本框激活后自动把这段拼音
    /// 打进引擎并上屏一次。用来在没有真人敲键的情况下验证
    /// 「组字 → 候选 → commit_string → 客户端」这条链路到底断在哪一环。
    fn selftest(&mut self) -> std::io::Result<()> {
        let Ok(s) = std::env::var("XIME_SELFTEST") else {
            return Ok(());
        };
        if self.selftest_done || s.is_empty() {
            return Ok(());
        }
        self.selftest_done = true;
        if !self.chinese {
            self.log("selftest: 英文模式，钩子不生效");
            return Ok(());
        }
        self.log(&format!("selftest: 输入 \"{s}\""));
        for ch in s.chars() {
            self.engine.push_char(ch);
        }
        self.refresh()?;
        self.log(&format!(
            "selftest: 候选 {:?}",
            self.engine.cands.iter().take(3).collect::<Vec<_>>()
        ));
        self.commit_selection()?;
        Ok(())
    }

    fn commit_text(&mut self, text: &str) -> std::io::Result<()> {
        if self.im == 0 {
            return Ok(());
        }
        self.log(&format!("commit \"{text}\" (serial={})", self.done_serial));
        if !text.is_empty() {
            proto::im_commit_string(&mut self.conn, self.im, text)?;
        }
        proto::im_set_preedit(&mut self.conn, self.im, "", 0, 0)?;
        proto::im_commit(&mut self.conn, self.im, self.done_serial)?;
        self.engine.reset();
        self.en.reset();
        self.popup_hide()?;
        Ok(())
    }

    /// 配置文件改动后热重载外观。每 400ms 最多 stat 一次，代价可以忽略。
    ///
    /// 只重算样式，不碰引擎状态——换主题不该打断正在进行的组字。
    /// 下一次 refresh 会用新度量重新 measure/ensure_buffer，尺寸变了自动重建缓冲。
    fn maybe_reload_config(&mut self) {
        let now = std::time::Instant::now();
        if let Some(t) = self.cfg_checked {
            if now.duration_since(t).as_millis() < 400 {
                return;
            }
        }
        self.cfg_checked = Some(now);
        let stamp = config::stamp();
        if stamp == self.cfg_stamp {
            return;
        }
        self.cfg_stamp = stamp;
        self.style = parse_style(&self.args);
        self.log(&format!(
            "配置已重载：主题={} 形态={}",
            self.style.theme.name,
            self.style.layout.name()
        ));
    }

    fn refresh(&mut self) -> std::io::Result<()> {
        if self.im == 0 {
            return Ok(());
        }
        self.maybe_reload_config();
        if !self.chinese {
            return self.refresh_en();
        }
        if self.inline_cands {
            let s = self.engine.inline_preedit();
            let len = s.len() as i32;
            proto::im_set_preedit(&mut self.conn, self.im, &s, len, len)?;
            proto::im_commit(&mut self.conn, self.im, self.done_serial)?;
            self.popup_hide()?;
            return Ok(());
        }
        let p = self.engine.preedit.clone();
        let len = p.len() as i32;
        proto::im_set_preedit(&mut self.conn, self.im, &p, len, len)?;
        proto::im_commit(&mut self.conn, self.im, self.done_serial)?;

        let cands = self.engine.cands.clone();
        if cands.is_empty() {
            self.popup_hide()?;
        } else {
            self.popup_update(&cands, popup::Mode::Zh)?;
        }
        Ok(())
    }

    /// 英文态的 preedit 与候选刷新
    ///
    /// preedit 显示用户敲的字母本身（不是补全结果）——补全只是建议，
    /// 不该在组字区里冒充「你已经输入了这个词」。
    fn refresh_en(&mut self) -> std::io::Result<()> {
        let p = self.en.preedit().to_string();
        if p.is_empty() {
            if self.im != 0 {
                proto::im_set_preedit(&mut self.conn, self.im, "", 0, 0)?;
                proto::im_commit(&mut self.conn, self.im, self.done_serial)?;
            }
            return self.popup_hide();
        }
        let len = p.len() as i32;
        proto::im_set_preedit(&mut self.conn, self.im, &p, len, len)?;
        proto::im_commit(&mut self.conn, self.im, self.done_serial)?;

        let cands = self.en.cands.clone();
        if cands.is_empty() {
            self.popup_hide()?;
        } else if self.inline_cands {
            // 内联模式复用中文那套：候选塞进 preedit
            self.popup_hide()?;
        } else {
            self.popup_update(&cands, popup::Mode::En)?;
        }
        Ok(())
    }

    fn popup_create(&mut self) -> std::io::Result<()> {
        if self.popup_surf != 0 || self.comp == 0 || self.im == 0 {
            return Ok(());
        }
        let surf = self.conn.alloc_id();
        proto::create_surface(&mut self.conn, self.comp, surf)?;

        // 候选窗不吃鼠标事件：输入区设为空
        let region = self.conn.alloc_id();
        proto::create_region(&mut self.conn, self.comp, region)?;
        proto::surface_set_input_region(&mut self.conn, surf, region)?;
        proto::region_destroy(&mut self.conn, region)?;

        let role = self.conn.alloc_id();
        proto::im_get_popup(&mut self.conn, self.im, role, surf)?;
        self.popup_surf = surf;
        self.popup_role = role;
        proto::surface_commit(&mut self.conn, surf)?;
        self.log("候选窗 surface 已创建");
        Ok(())
    }

    fn ensure_buffer(&mut self, w: i32, h: i32) -> std::io::Result<()> {
        if let Some(b) = &self.buf {
            if b.w == w && b.h == h {
                return Ok(());
            }
        }
        if let Some(b) = self.buf.take() {
            proto::buffer_destroy(&mut self.conn, b.buffer)?;
            proto::pool_destroy(&mut self.conn, b.pool)?;
        }
        let size = (w as usize) * (h as usize) * 4;
        let shm = Shm::new("xime-popup", size)?;
        let fd = shm.fd;
        let pool = self.conn.alloc_id();
        proto::create_pool(&mut self.conn, self.shm_global, pool, fd, size as i32)?;
        let buffer = self.conn.alloc_id();
        proto::pool_create_buffer(
            &mut self.conn,
            pool,
            buffer,
            0,
            w,
            h,
            w * 4,
            SHM_FORMAT_ARGB8888,
        )?;
        self.buf = Some(Buffer { shm, pool, buffer, w, h });
        Ok(())
    }

    fn popup_update(&mut self, cands: &[String], mode: popup::Mode) -> std::io::Result<()> {
        if self.popup_surf == 0 {
            return Ok(());
        }
        let shown: Vec<String> =
            cands.iter().take(self.style.max_items()).cloned().collect();
        if shown.is_empty() {
            return self.popup_hide();
        }

        let probe = Canvas::new(1, 1);
        let (w, h) = popup::measure(&probe, &shown, &self.style);
        if w <= 0 || h <= 0 {
            return Ok(());
        }
        self.ensure_buffer(w, h)?;

        let mut c = Canvas::new(w, h);
        popup::paint(&mut c, &shown, &self.style, mode);
        if let Some(b) = &mut self.buf {
            b.shm.upload(&c.px);
        }
        if let Some(b) = &self.buf {
            proto::surface_attach(&mut self.conn, self.popup_surf, Some(b.buffer), 0, 0)?;
        }
        if self.comp_ver >= 4 {
            proto::surface_damage_buffer(&mut self.conn, self.popup_surf, 0, 0, w, h)?;
        } else {
            proto::surface_damage(&mut self.conn, self.popup_surf, 0, 0, w, h)?;
        }
        proto::surface_commit(&mut self.conn, self.popup_surf)?;
        self.visible = true;
        Ok(())
    }

    fn popup_hide(&mut self) -> std::io::Result<()> {
        if self.popup_surf == 0 || !self.visible {
            return Ok(());
        }
        proto::surface_attach(&mut self.conn, self.popup_surf, None, 0, 0)?;
        proto::surface_commit(&mut self.conn, self.popup_surf)?;
        self.visible = false;
        Ok(())
    }

    fn vk_ensure(&mut self) -> std::io::Result<()> {
        if self.vk != 0 || self.vk_manager == 0 || self.seat == 0 {
            return Ok(());
        }
        let id = self.conn.alloc_id();
        proto::vk_create(&mut self.conn, self.vk_manager, self.seat, id)?;
        self.vk = id;
        Ok(())
    }

    fn vk_send_keymap(&mut self, raw: &[u8]) -> std::io::Result<()> {
        self.vk_ensure()?;
        if self.vk == 0 {
            return Ok(());
        }
        let shm = Shm::new("xime-keymap", raw.len())?;
        let mut shm = shm;
        shm.upload_bytes(raw);
        // sendmsg 已经把 fd 复制进内核，函数返回后关闭本地 fd 是安全的
        proto::vk_keymap(&mut self.conn, self.vk, KEYMAP_FORMAT_XKB_V1, shm.fd, raw.len() as u32)?;
        Ok(())
    }

    fn forward_key(&mut self, time: u32, key: u32, state: u32) -> std::io::Result<()> {
        if self.vk == 0 {
            self.log(&format!("vk 未就绪，丢弃按键 {key}"));
            return Ok(());
        }
        self.log(&format!("vk 回注 key={key} state={state}"));
        proto::vk_modifiers(
            &mut self.conn,
            self.vk,
            self.mods_depressed,
            self.mods_latched,
            self.mods_locked,
            self.group,
        )?;
        proto::vk_key(&mut self.conn, self.vk, time, key, state)
    }
}

/// 从 fd 读满 size 字节（keymap 通常是 40KB 左右，一次读不完）
fn read_fd(fd: i32, size: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        let n = unsafe {
            libc::pread(fd, buf[off..].as_mut_ptr() as *mut libc::c_void, size - off, off as i64)
        };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
    buf.truncate(off);
    Ok(buf)
}

fn ascii_letter(sym: u32) -> Option<char> {
    if (b'a' as u32..=b'z' as u32).contains(&sym) {
        return Some(sym as u8 as char);
    }
    if (b'A' as u32..=b'Z' as u32).contains(&sym) {
        return Some((sym as u8).to_ascii_lowercase() as char);
    }
    None
}

/// 字母 keysym → 字符，**保留大小写**（英文模式用）
///
/// 拼音不需要大小写，但英文的首字母大写、Caps Lock 全大写都得靠它，
/// 所以这里不能复用 `ascii_letter` 的降级逻辑。
fn letter_keep_case(sym: u32) -> Option<char> {
    if (b'a' as u32..=b'z' as u32).contains(&sym) || (b'A' as u32..=b'Z' as u32).contains(&sym) {
        char::from_u32(sym)
    } else {
        None
    }
}

/// Shift 的 evdev keycode（左 42 / 右 54）
///
/// 按松开事件判断中英切换时必须用 keycode 而不是 keysym：松开 Shift 的那一刻
/// 修饰键状态已经改了，keysym 会跟着变，而 keycode 是稳定的。
fn is_shift_key(key: u32) -> bool {
    key == 42 || key == 54
}

/// keysym → 中文标点
fn punct_of(sym: u32) -> Option<&'static str> {
    Some(match sym {
        0x2c => "，",
        0x2e => "。",
        0x3f => "？",
        0x21 => "！",
        0x3a => "：",
        0x3b => "；",
        0x28 => "（",
        0x29 => "）",
        0x5b => "【",
        0x5d => "】",
        0x3c => "《",
        0x3e => "》",
        0x2f => "、",
        0x22 => "“”",
        0x27 => "’",
        _ => return None,
    })
}

/// 简繁切换按键：Ctrl+Shift+F（与 fcitx5 一致）
///
/// 单独抽出来是为了能直接测：按键路由里的修饰键判断最容易写错位，
/// 而这组按键一旦失灵，用户会以为「繁体功能不存在」。
fn is_variant_toggle(sym: u32, mods: u32) -> bool {
    let need = keymap::MASK_CONTROL | keymap::MASK_SHIFT;
    mods & need == need && (sym == b'f' as u32 || sym == b'F' as u32)
}

fn usage() {
    println!("xime {VERSION} — Wayland 原生的中文输入法（含英文补全）");
    println!();
    println!("用法: xime [选项]");
    println!("  --verbose           打印协议与按键处理细节");
    println!("  --inline-candidates 把候选塞进 preedit 由应用字体渲染（不建候选窗）");
    println!("  --popup FORM        候选窗形态: horizontal(默认) | vertical | grid");
    println!("  --popup-cols N      网格形态的列数（默认 3）");
    println!("  --theme NAME        候选窗主题: light(默认) | dark | fcitx:NAME | 主题文件路径");
    println!("                      例: --theme fcitx:omarchy  直接用 fcitx5 的主题");
    println!("  --list-themes       列出能找到的 fcitx5 主题");
    println!("  --dump-theme        打印当前生效主题的颜色与度量（排查配置用）");
    println!("  --zh VARIANT        输出形态: hans(默认,简体) | hant(繁体)；运行中 Ctrl+Shift+F 切换");
    println!("  --en                启动即英文模式（默认不出候选窗，按键直接进应用，与 fcitx5 一致）");
    println!("  --en-completion     英文模式启用前缀补全 + 候选窗（旧行为，按需开启）");
    println!("  --check             诊断：检查协议可用性与接线状态");
    println!("  --dbus              以 fcitx5 DBus 前端运行（接管 org.fcitx.Fcitx5）");
    println!("                      用于微信/WPS/Electron/GNOME/X11 等自带 fcitx5-qt 的程序；");
    println!("                      与 fcitx5 及本程序的 Wayland 前端互斥，二者只能留一个");
    println!("  --query 拼音        打印该拼音串的候选与解码代价（调参用）");
    println!("  --render-test PATH [拼音]  把候选窗渲染成 BMP（可用 --popup/--theme 预览各形态）");
    println!("  --probe             枚举合成器提供的全局对象");
    println!("  --bench             解码性能测试");
    println!("  --keymap-test PATH  解析一份 XKB keymap 并抽查按键");
    println!("  --version           显示版本");
}

/// 取 `--flag value` 的值
fn flag_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// 从命令行组装候选窗样式
///
/// 顺序：全局配置（系统 → 用户 → XIME_THEME）先落地，命令行只覆盖显式指定的项。
fn parse_style(args: &[String]) -> popup::Style {
    let cfg = config::Settings::load();

    let mut style = popup::Style {
        layout: cfg.layout,
        cols: cfg.cols,
        dark: cfg.theme.dark,
        theme: cfg.theme,
    };
    if let Some(v) = flag_value(args, "--popup") {
        match popup::Layout::parse(&v) {
            Some(l) => style.layout = l,
            None => {
                eprintln!("xime: 未知候选窗形态 {v}（可选 horizontal/vertical/grid）");
                std::process::exit(2);
            }
        }
    }
    if let Some(v) = flag_value(args, "--popup-cols") {
        match v.parse::<usize>() {
            Ok(n) if (1..=9).contains(&n) => style.cols = n,
            _ => {
                eprintln!("xime: --popup-cols 需要 1..9 的整数");
                std::process::exit(2);
            }
        }
    }
    if let Some(v) = flag_value(args, "--theme") {
        match config::resolve_theme(&v) {
            Some(t) => {
                style.dark = t.dark;
                style.theme = t;
            }
            // 兼容旧写法：--theme light / dark 已在 resolve_theme 里处理，
            // 走到这里说明名字既不是内置也不是文件，也不是已知 fcitx 主题
            None => {
                eprintln!(
                    "xime: 未知主题 {v}\n  可用：light | dark | fcitx:NAME | /path/to/theme.conf"
                );
                std::process::exit(2);
            }
        }
    }
    style
}

/** 从命令行解析输出形态（简体 / 繁体） */
fn parse_variant(args: &[String]) -> zh::Variant {
    match flag_value(args, "--zh") {
        Some(v) => match zh::Variant::parse(&v) {
            Some(x) => x,
            None => {
                eprintln!("xime: 未知输出形态 {v}（可选 hans/hant）");
                std::process::exit(2);
            }
        },
        None => zh::Variant::Hans,
    }
}

/** 列出能直接用的 fcitx5 主题 */
fn list_themes() {
    println!("内置主题: light, dark");
    let themes = config::list_fcitx();
    if themes.is_empty() {
        println!("没有找到 fcitx5 主题（已查找 ~/.local/share/fcitx5/themes 与 /usr/share/fcitx5/themes）");
        return;
    }
    println!("fcitx5 主题（--theme fcitx:名字 即可启用）:");
    for (name, path) in themes {
        println!("  {:<18} {}", name, path.display());
    }
}

/** 打印当前生效的主题，排查配置问题时用 */
fn dump_theme(s: &popup::Style) {
    let t = &s.theme;
    let m = &t.metrics;
    println!("主题   {} ({})", t.name, if t.dark { "dark" } else { "light" });
    println!("形态   {}  cols={}", s.layout.name(), s.cols);
    println!("颜色   bg={} border={} text={} accent={} selected_text={}",
        config::to_hex(t.colors.bg),
        config::to_hex(t.colors.border),
        config::to_hex(t.colors.text),
        config::to_hex(t.colors.accent),
        config::to_hex(t.colors.sel_text));
    println!("度量   pad=({},{}) item_pad_x={} item_gap={} row_gap={}",
        m.pad_x, m.pad_y, m.item_pad_x, m.item_gap, m.row_gap);
    println!("       radius={} hl_radius={} hl_pad_y={} border_width={}",
        m.radius, m.hl_radius, m.hl_pad_y, m.border_w);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let style = parse_style(&args);
    let variant = parse_variant(&args);
    let english = args.iter().any(|a| a == "--en");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        usage();
        return;
    }
    if args.iter().any(|a| a == "--version") {
        println!("xime {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--bench") {
        bench(variant);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--keymap-test") {
        let path = args.get(i + 1).map(|s| s.as_str()).unwrap_or("/tmp/keymap.txt");
        keymap_test(path);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--query") {
        if let Some(q) = args.get(i + 1) {
            query(q, variant, english);
            return;
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--render-test") {
        let path = args.get(i + 1).map(|s| s.as_str()).unwrap_or("/tmp/popup.bmp");
        let input = args.get(i + 2).map(|s| s.as_str()).unwrap_or("nihao");
        render_test(path, input, &style, variant, english);
        return;
    }
    if args.iter().any(|a| a == "--probe") {
        probe();
        return;
    }
    if args.iter().any(|a| a == "--dbus") {
        let verbose = args.iter().any(|a| a == "--verbose");
        if let Err(e) = fcitx_dbus::run(variant, verbose) {
            eprintln!("xime: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--check") {
        check(variant);
        return;
    }
    if args.iter().any(|a| a == "--list-themes") {
        list_themes();
        return;
    }
    if args.iter().any(|a| a == "--dump-theme") {
        dump_theme(&style);
        return;
    }

    let inline_cands = args.iter().any(|a| a == "--inline-candidates");
    let verbose = args.iter().any(|a| a == "--verbose");

    // 英文模式默认「不出候选窗」：字母直接进应用，与 fcitx5 的英文态一致。
    // 前缀补全改成显式开启（--en-completion），因为它会在每个词上打扰输入。
    let en_completion = args.iter().any(|a| a == "--en-completion");
    let en_passthrough = args.iter().any(|a| a == "--en-passthrough") || !en_completion;

    // 单实例：systemd、KWin、XDG autostart、手工启动都可能在拉起 xime，
    // 谁先拿到锁谁当输入法，后来者安静退出（退出码 0，免得 systemd 反复重启）。
    if !single_instance::try_lock() {
        eprintln!("xime: 已有实例在运行，退出（运行中切换输入法用 xime-switch）");
        std::process::exit(0);
    }

    let mut x = match Xime::new(
        inline_cands,
        verbose,
        style,
        variant,
        english,
        en_passthrough,
        args.clone(),
    ) {
        Some(x) => x,
        None => std::process::exit(1),
    };
    if let Err(e) = x.run() {
        eprintln!("xime: {e}");
        std::process::exit(1);
    }
}

/// 诊断：只看协议是否齐全，不进入事件循环
fn check(variant: zh::Variant) {
    let mut c = match Conn::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("无法连接合成器: {e}");
            std::process::exit(1);
        }
    };
    // 是「合成器把连接塞过来」还是「自己连」——直接决定 KDE 下能不能拿到席位
    match wire::inherited_socket_str() {
        Some(fd) => println!(
            "连接方式：WAYLAND_SOCKET={}（由合成器/会话拉起，可绑定输入法席位）",
            fd.trim()
        ),
        None => println!(
            "连接方式：自行连接 WAYLAND_DISPLAY（普通客户端；KDE 下须由 KWin 拉起才拿得到席位）"
        ),
    }
    let reg = c.alloc_id();
    let cb = c.alloc_id();
    proto::get_registry(&mut c, reg).expect("发 get_registry 失败");
    proto::sync(&mut c, cb).expect("发 sync 失败");

    let mut have: Vec<(u32, String, u32)> = Vec::new();
    let mut done = false;
    for _ in 0..50 {
        if c.wait(500).is_err() || c.fill().is_err() {
            break;
        }
        while let Some(mut m) = c.next_msg() {
            if m.obj == reg && m.opcode == 0 {
                let name = m.args.u32();
                let iface = m.args.string().unwrap_or_default();
                let v = m.args.u32();
                have.push((name, iface, v));
            } else if m.obj == cb && m.opcode == 0 {
                done = true;
            }
        }
        if done {
            break;
        }
    }

    let want = [
        proto::IFACE_INPUT_METHOD,
        proto::IFACE_VIRTUAL_KEYBOARD,
        proto::IFACE_COMPOSITOR,
        proto::IFACE_SHM,
        proto::IFACE_SEAT,
    ];
    let mut ok = true;
    println!("协议可用性：");
    for w in want {
        let has = have.iter().any(|(_, h, _)| h == w);
        println!("  {:<38} {}", w, if has { "可用" } else { "缺失" });
        if !has {
            ok = false;
        }
    }

    // 全局接口存在 ≠ 拿得到：这个席位同一时刻只属于一个客户端
    let seat_g = have.iter().find(|(_, h, _)| h == proto::IFACE_SEAT).cloned();
    let mgr_g = have.iter().find(|(_, h, _)| h == proto::IFACE_INPUT_METHOD).cloned();
    if let (Some((seat_name, _, seat_ver)), Some((mgr_name, _, _))) = (seat_g, mgr_g) {
        match probe_seat(&mut c, reg, seat_name, seat_ver, mgr_name) {
            Ok(true) => println!("  {:<38} 空闲，xime 可以接管", "input-method 席位"),
            Ok(false) => {
                println!("  {:<38} 已被占用（另一个输入法持有）", "input-method 席位");
            }
            Err(e) => println!("  {:<38} 探测失败：{e}", "input-method 席位"),
        }
    }

    // 终端/普通应用能不能用输入法，取决于合成器有没有 text-input 管理器；
    // 没有它，终端即使拿到按键也不会把焦点交给输入法。
    let ti = have
        .iter()
        .find(|(_, h, _)| h == "zwp_text_input_manager_v3" || h == "zwp_text_input_manager_v1");
    match ti {
        Some((_, h, _)) => println!("  {:<38} 可用（{h}）", "text-input（应用侧）"),
        None => println!(
            "  {:<38} 缺失，终端/应用无法把焦点交给输入法",
            "text-input（应用侧）"
        ),
    }

    // 席位一山不容二虎：fcitx5/ibus 在跑时 xime 一定拿不到席位
    let others = running_imes();
    if others.is_empty() {
        println!("  {:<38} 无", "其他输入法进程");
    } else {
        println!("  {:<38} {}", "其他输入法进程", others.join(", "));
        println!(
            "      → 切换输入法用 xime-switch（别直接 pkill：Omarchy 的 fcitx5 是\n        systemd 用户单元，Restart=always 两秒后就会把它拉回来）"
        );
    }

    println!();
    println!("内置词条：{} 条", dict::word_count());
    println!("英文词表：{} 词（前缀补全，Shift 切换中英）", en::word_count());
    println!("字形数量：{} 个", font::Atlas::get().count());
    let (nc, np, nw) = zh::stats();
    println!("简繁数据：{nc} 个单字映射 / {np} 条词级例外 / {nw} 条词表（{} 条繁体形词条）",
        zh::trad_word_count());
    println!(
        "输出形态：{}（--zh hant 或运行时 Ctrl+Shift+F 切换）",
        variant.label()
    );
    if !have.iter().any(|(_, h, _)| h == proto::IFACE_INPUT_METHOD) {
        eprintln!("合成器未提供 zwp_input_method_manager_v2：GNOME/Mutter 不支持该协议。");
    }
    if !ok {
        std::process::exit(1);
    }
}

/// 扫 /proc 找出正在跑的其它输入法（它们会占掉 input-method 席位）
fn running_imes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmd) = std::fs::read_to_string(e.path().join("cmdline")) else { continue };
        let argv0 = cmd.split('\0').next().unwrap_or("");
        let base = argv0.rsplit('/').next().unwrap_or("");
        if matches!(base, "fcitx5" | "fcitx" | "ibus-daemon" | "ibus") {
            out.push(base.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 真正申请一次 input-method，判断席位是否空闲。
///
/// 合成器不会对「已占用」回协议错误，而是给新申请者发一条 unavailable 事件，
/// 所以只能等一下看有没有它。Ok(true) = 空闲，Ok(false) = 已被占用。
fn probe_seat(
    c: &mut Conn,
    reg: u32,
    seat_name: u32,
    seat_ver: u32,
    mgr_name: u32,
) -> std::io::Result<bool> {
    let seat = c.alloc_id();
    proto::bind(c, reg, seat_name, proto::IFACE_SEAT, seat_ver.min(5), seat)?;
    let mgr = c.alloc_id();
    proto::bind(c, reg, mgr_name, proto::IFACE_INPUT_METHOD, 1, mgr)?;
    let im = c.alloc_id();
    proto::im_get_input_method(c, mgr, seat, im)?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    while std::time::Instant::now() < deadline {
        if c.wait(80)? {
            c.fill()?;
        }
        while let Some(m) = c.next_msg() {
            // zwp_input_method_v2.unavailable = opcode 6
            if m.obj == im && m.opcode == 6 {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// 把候选窗渲染成 BMP，用于肉眼校验字形、布局与配色
fn render_test(path: &str, input: &str, style: &popup::Style, variant: zh::Variant, english: bool) {
    let (cands, mode, label) = if english {
        let mut e = en::EnEngine::new();
        for ch in input.chars() {
            e.push_char(ch, style.max_items());
        }
        (e.cands.clone(), popup::Mode::En, format!("英文/{}", e.case_mode_label()))
    } else {
        dict::init();
        let mut e = Engine::new();
        e.set_variant(variant);
        for c in input.chars() {
            e.push_char(c);
        }
        (e.cands.clone(), popup::Mode::Zh, variant.label().to_string())
    };

    let shown: Vec<String> = cands.iter().take(style.max_items()).cloned().collect();
    if shown.is_empty() {
        eprintln!("无候选");
        std::process::exit(1);
    }
    let probe = Canvas::new(1, 1);
    let (w, h) = popup::measure(&probe, &shown, style);
    let mut c = Canvas::new(w, h);
    popup::paint(&mut c, &shown, style, mode);
    match c.write_bmp(path) {
        Ok(()) => println!(
            "已写出 {path}  {w}x{h}  形态 {}（{} 列）  配色 {}  字体 {}  候选 {} 条",
            style.layout.name(),
            style.cols,
            style.theme.name.as_str(),
            label,
            shown.len()
        ),
        Err(e) => eprintln!("写 BMP 失败: {e}"),
    }
}

/// 解码性能：与 C 版同口径（每组 2000 次，含逐键重算）
fn bench(variant: zh::Variant) {
    use std::time::Instant;

    dict::init();
    let cases = [
        "n",
        "ni",
        "nih",
        "nihao",
        "woshiyigexuesheng",
        "jintiantianqihenhaowoxiangchuquwanwan",
        "zhonghuarenmingongheguo",
    ];

    let t0 = Instant::now();
    let mut e = Engine::new();
    e.set_variant(variant);
    println!("首次载入词库（含一次引擎构造）: {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);
    println!("输出形态: {}（繁体会在候选输出环节多做一次查表转换）", variant.label());

    for case in cases {
        // 长串单次成本高，按长度收敛迭代次数，避免基准自己跑不完
        let iters = if case.len() <= 5 {
            2000
        } else if case.len() <= 20 {
            300
        } else {
            100
        };
        let t0 = Instant::now();
        for _ in 0..iters {
            e.reset();
            for c in case.chars() {
                e.push_char(c);
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        println!(
            "{:<42} len={:<2}  {:.3} ms/次  候选={}  首选={}",
            case,
            case.len(),
            ms,
            e.cands.len(),
            e.cands.first().map(|s| s.as_str()).unwrap_or("-")
        );
    }
    println!("词条 {} 条", dict::word_count());

    // 词库查询微基准：把每类查询的成本单独量出来，避免只能看总量猜瓶颈
    let n = 20000;
    let mut hits = Vec::with_capacity(8);
    for (label, key) in [
        ("exact(\"n\") 单字母", "n"),
        ("exact(\"ni\") 双字母", "ni"),
        ("exact(\"nihao\") 整词", "nihao"),
        ("prefix(\"n\",3)", "n"),
        ("initials(\"nh\")", "nh"),
    ] {
        let t0 = Instant::now();
        for _ in 0..n {
            match label.split('(').next().unwrap() {
                "prefix" => dict::prefix_into(key, 3, 4, variant, &mut hits),
                "initials" => dict::initials_into(key, 4, variant, &mut hits),
                _ => dict::exact_into(key, 4, variant, &mut hits),
            }
        }
        let us = t0.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("  {label:<22} {us:>7.3} μs/次  命中 {}", hits.len());
    }
}

/// 解析一份 XKB keymap 文本并抽查几个键，用于调 keymap 解析器
fn keymap_test(path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读不到 {path}: {e}");
            std::process::exit(1);
        }
    };
    match Keymap::parse(&text) {
        Some(k) => {
            println!("解析成功：{} 键", k.count());
            // evdev 码：1=Esc 14=BackSpace 16=q 30=a 28=Enter 57=Space 51=逗号
            for (code, name) in [
                (1u32, "Esc"),
                (14, "BackSpace"),
                (16, "q"),
                (30, "a"),
                (28, "Enter"),
                (57, "Space"),
                (51, "comma"),
            ] {
                println!(
                    "  {name:<10} evdev={code:<3} 无修饰={:#06x} shift={:#06x}",
                    k.sym(code, 0),
                    k.sym(code, keymap::MASK_SHIFT)
                );
            }
        }
        None => println!("解析失败"),
    }
}

/// 打印某拼音串的候选与解码代价
fn query(input: &str, variant: zh::Variant, english: bool) {
    if english {
        let mut e = en::EnEngine::new();
        for ch in input.chars() {
            e.push_char(ch, 9);
        }
        println!("前缀: {}  形态: 英文/{}", e.preedit(), e.case_mode_label());
        for (i, c) in e.cands.iter().enumerate() {
            println!("{}. {c}", i + 1);
        }
        return;
    }
    dict::init();
    let mut e = Engine::new();
    e.set_variant(variant);
    for c in input.chars() {
        e.push_char(c);
    }
    println!("拼音: {}  形态: {}", e.preedit, variant.label());
    println!("候选: {}", e.cands.len());
    for (i, c) in e.cands.iter().enumerate() {
        println!("  {}. {:<24} cost={:.2}", i + 1, c, e.cost[i]);
    }
}

/// 枚举合成器提供的全局对象，用来验证 wire 层
fn probe() {
    let mut c = match Conn::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    };
    let reg = c.alloc_id();
    let cb = c.alloc_id();
    proto::get_registry(&mut c, reg).expect("发 get_registry 失败");
    proto::sync(&mut c, cb).expect("发 sync 失败");

    let mut done = false;
    for _ in 0..50 {
        if c.wait(500).is_err() || c.fill().is_err() {
            break;
        }
        while let Some(mut m) = c.next_msg() {
            if m.obj == reg && m.opcode == 0 {
                let name = m.args.u32();
                let iface = m.args.string().unwrap_or_default();
                let ver = m.args.u32();
                println!("global  name={name:<3} {iface} v{ver}");
            } else if m.obj == cb && m.opcode == 0 {
                println!("-- sync 完成 --");
                done = true;
            } else if m.obj == DISPLAY_ID && m.opcode == 0 {
                let obj = m.args.u32();
                let code = m.args.u32();
                let msg = m.args.string().unwrap_or_default();
                eprintln!("协议错误: obj={obj} code={code} {msg}");
                std::process::exit(1);
            }
        }
        if done {
            break;
        }
    }
    if !done {
        eprintln!("未收到 sync 完成事件");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 简繁切换按键判定() {
        let both = keymap::MASK_CONTROL | keymap::MASK_SHIFT;
        assert!(is_variant_toggle(b'f' as u32, both));
        assert!(is_variant_toggle(b'F' as u32, both), "Shift 生效后 sym 是大写 F");
        // 缺一个修饰键就不该触发，否则单按 f 会误切形态
        assert!(!is_variant_toggle(b'f' as u32, keymap::MASK_CONTROL));
        assert!(!is_variant_toggle(b'f' as u32, keymap::MASK_SHIFT));
        // Alt / Super 不影响判定（多按着也能切）
        assert!(is_variant_toggle(b'f' as u32, both | keymap::MASK_MOD1));
        // 别把别的键卷进来
        assert!(!is_variant_toggle(b'g' as u32, both));
        assert!(!is_variant_toggle(keymap::KS_SHIFT_L, both));
    }

    #[test]
    fn shift键码判定() {
        // evdev：左 Shift 42、右 Shift 54
        assert!(is_shift_key(42));
        assert!(is_shift_key(54));
        // 字母 a 是 38，别把普通键当切换键
        assert!(!is_shift_key(38));
        assert!(!is_shift_key(0));
    }

    #[test]
    fn 英文字母保留大小写_拼音统一小写() {
        assert_eq!(letter_keep_case(b'a' as u32), Some('a'));
        assert_eq!(letter_keep_case(b'A' as u32), Some('A'), "英文要大写就得上大写");
        assert_eq!(letter_keep_case(b'1' as u32), None);
        assert_eq!(letter_keep_case(b' ' as u32), None);
        // 拼音不需要大小写，走另一条路径
        assert_eq!(ascii_letter(b'A' as u32), Some('a'));
        assert_eq!(ascii_letter(b'a' as u32), Some('a'));
    }
}
