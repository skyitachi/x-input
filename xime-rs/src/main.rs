mod bigram;
mod config;
mod dict;
mod en;
mod engine;
mod fcitx_dbus;
mod font;
mod fuzzy;
mod im_state;
mod keymap;
mod popup;
mod proto;
mod render;
mod shm;
mod single_instance;
mod ui_slint;
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

/// 走哪一套 Wayland 输入法协议。
///
/// v2 是 wlroots 系（Hyprland/sway/labwc）的现状；KWin 6.6 仍停在 v1。
/// 两套协议对「候选窗放哪」「不消费的键怎么还回去」的做法完全不同，
/// 但按键路由、引擎、渲染是同一套——所以只在协议出入口分叉。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frontend {
    /// 自动：注册表里出现哪个用哪个（v2 优先）
    Auto,
    V2,
    V1,
}

impl Frontend {
    fn label(self) -> &'static str {
        match self {
            Frontend::Auto => "auto",
            Frontend::V2 => "v2",
            Frontend::V1 => "v1",
        }
    }

    fn parse(s: &str) -> Option<Frontend> {
        match s {
            "auto" => Some(Frontend::Auto),
            "v2" | "input-method-v2" => Some(Frontend::V2),
            "v1" | "kwin" | "input-method-v1" => Some(Frontend::V1),
            _ => None,
        }
    }
}

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
    /// 主显示器（第一个 wl_output），用于读取整数 scale 作为回退
    output: u32,
    /// 分数缩放管理器（wp_fractional_scale_manager_v1），0 = 合成器不支持
    frac_scale_mgr: u32,
    /// 绑定到 popup surface 的分数缩放对象，收到 preferred_scale 事件
    frac_scale: u32,
    /// viewporter 全局（支持任意分数缩放的逻辑尺寸声明）
    viewporter: u32,
    /// 绑定到 popup surface 的 wp_viewport 对象
    viewport: u32,
    /// 当前缩放来源与值（逻辑→物理），Slint 与 shm 缓冲共用
    scale: f32,
    /// 是否用环境变量 XIME_SCALE 锁定了缩放（锁定后忽略合成器的 scale 事件）
    scale_locked: bool,
    im: u32,
    grab: u32,
    popup_surf: u32,
    popup_role: u32,
    vk: u32,
    /// 实际在用的协议（Auto 在注册表扫描时解析成 V1/V2）
    fe: Frontend,
    /// v1：zwp_input_method_v1 对象（KWin 直接给，没有 manager）
    im1: u32,
    /// v1：当前输入上下文 id，由合成器在 activate 事件里分配
    ctx: u32,
    /// v1：zwp_input_panel_v1 的注册表 name（延后到首次弹候选窗时才 bind，
    /// 该接口同一时刻只允许一个客户端持有，早绑可能在切换输入法时被拒）
    panel_global: u32,
    /// v1：候选窗角色 zwp_input_panel_v1（候选窗 surface 本身复用 popup_surf）
    panel: u32,
    /// v1：commit_state 给的最新 serial，commit_string / preedit_string 都要带上
    ctx_serial: u32,
    /// v1：最近一次 wl_keyboard.key 的 serial，转发按键时原样带回
    key_serial: u32,
    /// 注册表扫描的结束锚点（wl_callback）：到这一刻才能确定「到底拿到了哪套席位」
    sync_cb: u32,
    /// 扫描完发现没有任何输入法席位：退出码 1，避免静默占着单实例锁
    no_seat: bool,
    buf: Option<Buffer>,
    visible: bool,
    done_serial: u32,
    engine: Engine,
    /// 候选页码（翻页）。候选集变化时归零；翻页键 -/= 与 PageUp/PageDown 调整。
    page: usize,
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
    /// Slint 候选窗渲染器。Some = 用 Slint，None = 手写 Canvas（--legacy-ui）。
    /// 进程级只初始化一次，Wayland 前端独占。
    slint_ui: Option<ui_slint::SlintUi>,
}

/// 候选按页切片的 [start, end)。page 越界时回退到第 0 页（候选集可能已变）。
fn page_range(total: usize, page: usize, page_size: usize) -> (usize, usize) {
    let start = (page * page_size).min(total);
    let end = (start + page_size).min(total);
    (start, end)
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
        use_slint: bool,
        fe: Frontend,
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

        // 初始缩放：XIME_SCALE 环境变量优先（便于覆盖/测试）。
        // 合成器的 fractional scale / wl_output scale 会在连接建立后到达并覆盖它。
        let env_scale = std::env::var("XIME_SCALE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok());
        let scale_locked = env_scale.is_some();
        let init_scale = env_scale.unwrap_or(1.0);

        // Slint UI：set_platform 是进程级一次性操作，这里只在 Wayland 前端初始化。
        // 失败时（例如平台已设置）自动回退到手写渲染，不让输入法起不来。
        let slint_ui = if use_slint {
            match ui_slint::SlintUi::new(init_scale, &style) {
                Some(ui) => {
                    eprintln!(
                        "xime: 候选窗使用 Slint 软件渲染，初始 scale={init_scale}，形态={}",
                        style.layout.name()
                    );
                    Some(ui)
                }
                None => {
                    eprintln!("xime: Slint 初始化失败，回退到手写渲染");
                    None
                }
            }
        } else {
            None
        };
        Some(Xime {
            conn,
            reg: 0,
            comp: 0,
            comp_ver: 0,
            shm_global: 0,
            seat: 0,
            im_manager: 0,
            vk_manager: 0,
            output: 0,
            frac_scale_mgr: 0,
            frac_scale: 0,
            viewporter: 0,
            viewport: 0,
            scale: init_scale,
            scale_locked,
            im: 0,
            grab: 0,
            popup_surf: 0,
            popup_role: 0,
            vk: 0,
            fe,
            im1: 0,
            ctx: 0,
            panel_global: 0,
            panel: 0,
            ctx_serial: 0,
            key_serial: 0,
            sync_cb: 0,
            no_seat: false,
            buf: None,
            visible: false,
            done_serial: 0,
            engine,
            page: 0,
            km: None,
            keymap_raw: Vec::new(),
            mods_depressed: 0,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
            consumed: vec![false; CONSUMED_MAX],
            chinese: if start_english {
                false
            } else {
                // 双前端共享中英状态：文件里有就以文件为准，见 im_state 模块注释。
                // 显式 --en 是启动时的强制指定，优先于共享状态。
                im_state::read().unwrap_or(true)
            },
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
            slint_ui,
        })
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            println!("xime: {msg}");
        }
    }

    /// 应用合成器报告的缩放。XIME_SCALE 锁定时忽略。变化时同步给 Slint，
    /// 若候选窗正显示则重渲染一帧，避免切换显示器后候选条尺寸不变。
    fn apply_scale(&mut self, scale: f32, source: &str) {
        if self.scale_locked {
            return;
        }
        if (scale - self.scale).abs() < 0.001 {
            return;
        }
        self.scale = scale;
        if let Some(ui) = self.slint_ui.as_mut() {
            ui.set_scale(scale);
        }
        self.log(&format!("缩放 {scale:.3} ← {source}"));
        // 用当前候选重画一帧，让尺寸立即生效（来源与正常刷新路径一致）
        if self.visible {
            let cands = self.engine.cands.clone();
            if !cands.is_empty() {
                let mode = if self.chinese {
                    popup::Mode::Zh
                } else {
                    popup::Mode::En
                };
                let _ = self.popup_update(&cands, mode);
            }
        }
    }

    fn run(&mut self) -> std::io::Result<()> {
        self.reg = self.conn.alloc_id();
        proto::get_registry(&mut self.conn, self.reg)?;
        // registry 的 global 事件流没有「结束」信号，用一次 sync 当锚点：
        // 回调到时就能确定自己拿到了哪套协议的席位（v2 manager / v1 对象）。
        self.sync_cb = self.conn.alloc_id();
        proto::sync(&mut self.conn, self.sync_cb)?;

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
        // XIME_TRACE=1：打印收到的每条消息（对象 + opcode），排查「按键到底有没有送到」
        // 这类问题时用——候选窗不出来和按键没到是完全不同的两个原因。
        if std::env::var_os("XIME_TRACE").is_some() {
            eprintln!("trace: obj={} opcode={}", m.obj, m.opcode);
        }
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
        if self.sync_cb != 0 && m.obj == self.sync_cb && m.opcode == 0 {
            self.after_registry();
            return Ok(());
        }
        if self.im != 0 && m.obj == self.im {
            return self.on_im(m.opcode, &mut m);
        }
        // v1：输入法对象与它按焦点创建的上下文是两个对象，事件语义也不同
        if self.im1 != 0 && m.obj == self.im1 {
            return self.on_im1(m.opcode, &mut m);
        }
        if self.ctx != 0 && m.obj == self.ctx {
            return self.on_ctx1(m.opcode, &mut m);
        }
        if self.grab != 0 && m.obj == self.grab {
            // v1 抓来的抓取对象就是标准 wl_keyboard（keymap/key/modifiers 的 opcode 与 v2 抓取对象不同）
            return if self.fe == Frontend::V1 {
                self.on_kb1(m.opcode, &mut m)
            } else {
                self.on_grab(m.opcode, &mut m)
            };
        }
        if self.popup_role != 0 && m.obj == self.popup_role {
            // text_input_rectangle：光标矩形，合成器用它定位候选窗，这里只需记录
            if m.opcode == 0 && self.verbose {
                let (x, y, w, h) = (m.args.i32(), m.args.i32(), m.args.i32(), m.args.i32());
                println!("xime: 光标矩形 {x},{y} {w}x{h}");
            }
            return Ok(());
        }
        if self.output != 0 && m.obj == self.output {
            // wl_output.scale (event 3, since v2): int factor
            if m.opcode == 3 {
                let factor = m.args.i32();
                if factor > 0 {
                    self.apply_scale(factor as f32, "wl_output");
                }
            }
            return Ok(());
        }
        if self.frac_scale != 0 && m.obj == self.frac_scale {
            // wp_fractional_scale_v1.preferred_scale (event 0): uint，值 = scale × 120
            if m.opcode == 0 {
                let v = m.args.u32();
                if v > 0 {
                    self.apply_scale(v as f32 / 120.0, "fractional-scale");
                }
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
                proto::bind(
                    &mut self.conn,
                    self.reg,
                    name,
                    iface,
                    self.comp_ver,
                    self.comp,
                )?;
            }
            proto::IFACE_SHM => {
                self.shm_global = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, 1, self.shm_global)?;
            }
            proto::IFACE_SEAT => {
                self.seat = self.conn.alloc_id();
                proto::bind(
                    &mut self.conn,
                    self.reg,
                    name,
                    iface,
                    version.min(5),
                    self.seat,
                )?;
            }
            proto::IFACE_INPUT_METHOD => {
                // v2：得先有 manager，再由 manager + seat 建输入法对象
                if self.fe != Frontend::V1 {
                    self.fe = Frontend::V2;
                    self.im_manager = self.conn.alloc_id();
                    proto::bind(&mut self.conn, self.reg, name, iface, 1, self.im_manager)?;
                }
            }
            proto::IFACE_INPUT_METHOD_V1 => {
                // 只有「由合成器拉起」（WAYLAND_SOCKET）的连接上才会有这个全局，
                // 而且出现了就不需要 seat、不需要 manager：对象本身就是输入法。
                if self.fe == Frontend::Auto {
                    self.fe = Frontend::V1;
                }
                if self.fe == Frontend::V1 {
                    self.im1 = self.conn.alloc_id();
                    proto::bind(&mut self.conn, self.reg, name, iface, 1, self.im1)?;
                    self.log(&format!(
                        "使用 input-method {} 前端（KWin）",
                        self.fe.label()
                    ));
                }
            }
            proto::IFACE_INPUT_PANEL => {
                // 候选窗角色用，先只记名字：该接口同一时刻只允许一个客户端持有，
                // 真正需要弹候选窗时（activate）再 bind，避免切换输入法时抢绑定。
                self.panel_global = name;
                self.log("记录 zwp_input_panel_v1（候选窗角色，用到时再 bind）");
                return Ok(());
            }
            proto::IFACE_VIRTUAL_KEYBOARD => {
                self.vk_manager = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, 1, self.vk_manager)?;
            }
            proto::IFACE_OUTPUT => {
                // 只取第一个 output 作为整数 scale 回退来源（多显示器下 fractional scale 更准）
                if self.output == 0 {
                    self.output = self.conn.alloc_id();
                    proto::bind(
                        &mut self.conn,
                        self.reg,
                        name,
                        iface,
                        version.min(4),
                        self.output,
                    )?;
                }
            }
            proto::IFACE_FRACTIONAL_SCALE => {
                self.frac_scale_mgr = self.conn.alloc_id();
                proto::bind(
                    &mut self.conn,
                    self.reg,
                    name,
                    iface,
                    1,
                    self.frac_scale_mgr,
                )?;
            }
            proto::IFACE_VIEWPORTER => {
                self.viewporter = self.conn.alloc_id();
                proto::bind(&mut self.conn, self.reg, name, iface, 1, self.viewporter)?;
            }
            _ => {
                self.log(&format!("忽略全局对象 {iface} v{version}"));
                return Ok(());
            }
        }
        self.log(&format!("绑定 {iface} v{version}"));
        self.try_init()
    }

    /// 注册表扫描结束：确认到底拿到了哪套协议的席位。
    ///
    /// 这一步不能省。单实例锁在连接之前就已经拿下，如果这里发现自己其实没有席位却继续挂着，
    /// 合成器随后拉起的那份实例会因为锁被占而「安静退出」——用户看到的是输入法完全不工作、
    /// 且没有任何报错。宁可自己失败退出，把位置让给真正的拉起方。
    fn after_registry(&mut self) {
        if self.im_manager != 0 || self.im1 != 0 {
            self.log(&format!("协议前端确定：{}", self.fe.label()));
            return;
        }
        self.no_seat = true;
        self.running = false;
        eprintln!(
            "xime: 合成器没有给出输入法席位（没有 zwp_input_method_v2，也没有 zwp_input_method_v1）"
        );
        if wire::inherited_socket_str().is_some() {
            eprintln!(
                "xime: 本次是被合成器拉起的（WAYLAND_SOCKET），却没有拿到席位 → 请确认桌面条目带了 X-KDE-Wayland-VirtualKeyboard=true"
            );
        } else {
            eprintln!(
                "xime: 本次是自行连接的普通客户端。KDE 下 KWin 只把 zwp_input_method_v1 交给它自己拉起的进程，"
            );
            eprintln!(
                "xime: 手工执行 `xime &` 在 KDE 上拿不到席位；改用 xime-switch xime（写 kwinrc 的 [Wayland] InputMethod）"
            );
        }
        if self.fe == Frontend::V2 {
            eprintln!(
                "xime: 当前指定了 --frontend v2，但这个合成器没有 zwp_input_method_manager_v2（KDE 请用 v1/auto）"
            );
        }
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
                self.send_preedit("", 0)?;
            }
            1 => {
                // deactivate
                self.log("deactivate");
                self.engine.reset();
                self.popup_hide()?;
                // 主动释放键盘 grab，而不是只把 id 清零。
                //
                // 合成器发 deactivate 只表示「本轮输入法不再被需要」，并不保证它已经把
                // grab 对象销毁。Hyprland 0.56 实测就不会：焦点从 text-input 应用切到
                // Qt/DBus 应用（Omarchy 菜单、Quickshell）后，按键继续被送进 grab，
                // 而输入法这边已经 reset、不消费这些键 —— 字就凭空消失。
                // 用户侧表现是「启动器里连一个英文字母都打不出来」，而 fcitx5 没这个问题
                // （它的 Wayland 前端在 deactivate 时会 release）。
                if self.grab != 0 {
                    let _ = proto::im_grab_release(&mut self.conn, self.grab);
                }
                // 无论 release 是否成功都必须清零：否则下次 activate 时 self.grab != 0
                // 会跳过重新 grab，输入法就再也收不到按键。
                self.grab = 0;
                self.send_preedit("", 0)?;
            }
            4 => {
                // content_type(hint, purpose)：应用声明的输入框类型
                let hint = m.args.u32();
                let purpose = m.args.u32();
                self.apply_content_type(hint, purpose)?;
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

    /// 解析合成器送来的 XKB keymap 文本。两套协议的 keymap 事件参数格式相同
    /// （format uint / fd / size uint），差别只在拿它做什么。
    fn on_keymap(&mut self, m: &mut Incoming) -> std::io::Result<()> {
        let _format = m.args.u32();
        let fd = self.conn.take_fd();
        let size = m.args.u32() as usize;
        if fd < 0 || size == 0 {
            return Ok(());
        }
        let raw = read_fd(fd, size);
        unsafe { libc::close(fd) };
        if let Ok(bytes) = raw {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            self.log(&format!(
                "keymap 事件：声明 {size} 字节，实读 {} 字节",
                bytes.len()
            ));
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
        // v2 靠虚拟键盘回注按键，必须把同一份 keymap 同步过去；
        // v1 的回注走 context.key()，keycode 直接用合成器自己的布局，不需要这一步。
        if self.fe != Frontend::V1 {
            let raw = self.keymap_raw.clone();
            if !raw.is_empty() {
                self.vk_send_keymap(&raw)?;
            }
        }
        Ok(())
    }

    /// zwp_input_method_v1：只有 activate / deactivate，没有 manager、没有 done。
    fn on_im1(&mut self, opcode: u16, m: &mut Incoming) -> std::io::Result<()> {
        match opcode {
            0 => {
                // activate(id new_id)：id 由合成器分配（0xff000000 起），直接当已有对象用
                let ctx = m.args.u32();
                self.ctx = ctx;
                self.ctx_serial = 0;
                self.key_serial = 0;
                self.engine.reset();
                self.en.reset();
                self.consumed.iter_mut().for_each(|c| *c = false);
                self.log(&format!("activate（v1, ctx={ctx:#x}）"));
                self.popup_create()?;
                // grab_keyboard 是 v1 里唯一不带 serial 的请求（见 proto::ctx_grab_keyboard），
                // 所以可以立刻抓。抓完合成器会把 keymap 与所有按键送到这个 wl_keyboard 上。
                if self.grab == 0 {
                    let kb = self.conn.alloc_id();
                    proto::ctx_grab_keyboard(&mut self.conn, self.ctx, kb)?;
                    self.grab = kb;
                    self.log("已抓取键盘（v1）");
                }
                self.send_preedit("", 0)?;
            }
            1 => {
                // deactivate(context)：按协议，输入法处理完后要销毁这个上下文
                let ctx = m.args.u32();
                self.log(&format!("deactivate（v1, ctx={ctx:#x}）"));
                self.engine.reset();
                self.en.reset();
                self.popup_hide()?;
                if ctx != 0 {
                    proto::ctx_destroy(&mut self.conn, ctx)?;
                }
                self.ctx = 0;
                self.grab = 0;
                self.ctx_serial = 0;
            }
            _ => {}
        }
        Ok(())
    }

    /// zwp_input_method_context_v1 的事件。
    ///
    /// opcode: 0 surrounding_text / 1 reset / 2 content_type / 3 invoke_action
    ///         4 commit_state / 5 preferred_language
    fn on_ctx1(&mut self, opcode: u16, m: &mut Incoming) -> std::io::Result<()> {
        match opcode {
            0 => {
                // surrounding_text(text, cursor, anchor)：客户端当前文本的快照。
                // 平时不用，但它是**唯一能反向确认「提交的文字真的进了客户端」**的东西，
                // 端到端排查（尤其「候选窗有、上屏没反应」）时打开 verbose 就能看到。
                if self.verbose {
                    let text = m.args.string().unwrap_or_default();
                    let cursor = m.args.u32();
                    let anchor = m.args.u32();
                    self.log(&format!(
                        "surrounding_text（cursor={cursor} anchor={anchor}）: {text:?}"
                    ));
                }
            }
            1 => {
                // reset：客户端要求丢掉组字（失焦、清空输入框等）
                self.log("reset");
                self.engine.reset();
                self.en.reset();
                self.popup_hide()?;
                self.send_preedit("", 0)?;
            }
            2 => {
                let hint = m.args.u32();
                let purpose = m.args.u32();
                self.apply_content_type(hint, purpose)?;
            }
            3 => { /* invoke_action：软键盘动作，暂不使用 */ }
            4 => {
                // commit_state(serial)：此后所有改变文本状态的请求都要带这个 serial
                self.ctx_serial = m.args.u32();
                self.log(&format!("commit_state serial={}", self.ctx_serial));
                // 第一个 serial 到手，这才有资格抓键盘
                if self.grab == 0 && self.ctx != 0 {
                    let kb = self.conn.alloc_id();
                    proto::ctx_grab_keyboard(&mut self.conn, self.ctx, kb)?;
                    self.grab = kb;
                    self.log("已抓取键盘（v1）");
                    self.send_preedit("", 0)?;
                }
            }
            5 => { /* preferred_language */ }
            _ => {}
        }
        Ok(())
    }

    /// v1 抓到的键盘是标准 wl_keyboard，opcode 与 v2 的抓取对象不同：
    /// 0 keymap / 1 enter / 2 leave / 3 key / 4 modifiers / 5 repeat_info
    fn on_kb1(&mut self, opcode: u16, m: &mut Incoming) -> std::io::Result<()> {
        match opcode {
            0 => self.on_keymap(m)?,
            1 | 2 => { /* enter / leave：焦点由 activate 事件表达，这里不需要 */ }
            3 => {
                let serial = m.args.u32();
                let time = m.args.u32();
                let key = m.args.u32();
                let state = m.args.u32();
                self.key_serial = serial;
                self.log(&format!("key code={key} state={state}"));
                self.on_key(time, key, state)?;
            }
            4 => {
                let _serial = m.args.u32();
                self.mods_depressed = m.args.u32();
                self.mods_latched = m.args.u32();
                self.mods_locked = m.args.u32();
                self.group = m.args.u32();
                // v1 没有虚拟键盘：修饰键状态变化必须显式还给应用，
                // 否则单独按 Ctrl、Alt 的状态在应用侧永远是旧的。
                if self.ctx != 0 {
                    proto::ctx_modifiers(
                        &mut self.conn,
                        self.ctx,
                        self.key_serial,
                        self.mods_depressed,
                        self.mods_latched,
                        self.mods_locked,
                        self.group,
                    )?;
                }
            }
            _ => { /* repeat_info：重复由合成器负责 */ }
        }
        Ok(())
    }

    fn on_grab(&mut self, opcode: u16, m: &mut Incoming) -> std::io::Result<()> {
        match opcode {
            0 => self.on_keymap(m)?,
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
        // 另一个前端可能刚切过中英，先对齐再判键，否则这一键会用旧状态处理
        self.sync_shared_im();
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
            self.page = 0;
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

        // 数字选词：序号按当前页算（1-9 对应页内第 1-9 条），超出当前页范围的序号透传
        if (b'1' as u32..=b'9' as u32).contains(&sym) {
            if self.engine.active() {
                let page_size = self.style.max_items();
                let in_page = ((sym - b'1' as u32) as usize) < page_size;
                let pick = self.page * page_size + (sym - b'1' as u32) as usize;
                if in_page {
                    if let Some(text) = self.engine.cands.get(pick).cloned() {
                        self.commit_text(&text)?;
                        if idx < CONSUMED_MAX {
                            self.consumed[idx] = true;
                        }
                        return Ok(());
                    }
                }
                self.forward_key(time, key, state)?;
                return Ok(());
            }
            self.forward_key(time, key, state)?;
            return Ok(());
        }

        match sym {
            // 翻页：上一页 -/PageUp、下一页 =/PageDown。边界不循环，越界时吞键不动作
            // （与 fcitx5 一致，不把 -/= 漏给应用）。无组字时透传。
            keymap::KS_MINUS | keymap::KS_PAGE_UP | keymap::KS_UP => {
                if self.engine.active() && self.page > 0 {
                    self.page -= 1;
                    self.refresh()?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
            keymap::KS_EQUAL | keymap::KS_PAGE_DOWN | keymap::KS_DOWN => {
                let page_size = self.style.max_items();
                if self.engine.active() && (self.page + 1) * page_size < self.engine.cands.len() {
                    self.page += 1;
                    self.refresh()?;
                    if idx < CONSUMED_MAX {
                        self.consumed[idx] = true;
                    }
                    return Ok(());
                }
                self.forward_key(time, key, state)?;
            }
            keymap::KS_BACKSPACE => {
                if self.engine.active() {
                    self.engine.backspace();
                    self.page = 0;
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
                    self.page = 0;
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
                    self.page = 0;
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
    fn on_key_en(&mut self, time: u32, key: u32, state: u32, sym: u32) -> std::io::Result<()> {
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
    /// 把另一个前端刚改过的中英状态同步过来。
    ///
    /// 每次拿到按键前调一次：共享文件只在 Shift 切换时变，正常打字是一次
    /// 页缓存内的字符串读，代价可以忽略；换来的是「在哪个窗口切的状态都一致」。
    fn sync_shared_im(&mut self) {
        let Some(zh) = im_state::read() else { return };
        if zh == self.chinese {
            return;
        }
        self.chinese = zh;
        self.engine.reset();
        self.en.reset();
        let _ = self.popup_hide();
        self.log(if zh {
            "同步为中文（另一前端切换）"
        } else if self.en_passthrough {
            "同步为英文（直通，另一前端切换）"
        } else {
            "同步为英文（前缀补全，另一前端切换）"
        });
    }

    fn toggle_language(&mut self) -> std::io::Result<()> {
        self.commit_selection()?;
        self.chinese = !self.chinese;
        self.engine.reset();
        self.en.reset();
        self.popup_hide()?;
        // 通知另一个前端（DBus）：两个进程各管一半应用，状态必须一起变，
        // 否则会出现"终端里切英文了、Qt 应用里还是中文"这种无从察觉的错位。
        im_state::write(self.chinese);
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
        let page_size = self.style.max_items();
        if let Some(s) = self.engine.cands.get(self.page * page_size).cloned() {
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

    /// content_type 的公共处理。v1 的 purpose 与 v3 是同一套枚举（KWin 原样转发），
    /// 但 v1 没有 pin，且 terminal 在 v3 里是 13，两个值都认，避免版本差异。
    fn apply_content_type(&mut self, hint: u32, purpose: u32) -> std::io::Result<()> {
        let pw = purpose == 8 || purpose == 9;
        let term = purpose == 13 || purpose == 14;
        self.log(&format!("content_type: hint={hint} purpose={purpose}"));
        if pw && !self.field_passthrough {
            // 进入密码框：清掉组字残留，避免半截拼音/补全突然上屏
            self.engine.reset();
            self.en.reset();
            self.page = 0;
            self.popup_hide()?;
        }
        self.field_passthrough = pw;
        self.terminal_purpose = term;
        self.selftest()
    }

    /// 输入法是否已经拿到可写文本的目标（v2 是输入法对象，v1 是当前上下文）
    fn ready(&self) -> bool {
        match self.fe {
            Frontend::V1 => self.ctx != 0,
            _ => self.im != 0,
        }
    }

    /// 写回组字串。v2 先改状态再 commit(serial)；v1 直接发，带 commit_state 的 serial。
    fn send_preedit(&mut self, text: &str, cursor: i32) -> std::io::Result<()> {
        match self.fe {
            Frontend::V1 => {
                if self.ctx == 0 {
                    return Ok(());
                }
                // 第三参是 KWin 变体里的 commit 字符串（标准 XML 写的是 cursor int，
                // 见 proto::ctx_preedit 的注释）。组字期间没有「随预编辑一起提交」的内容。
                proto::ctx_preedit(&mut self.conn, self.ctx, self.ctx_serial, text, "")
            }
            _ => {
                if self.im == 0 {
                    return Ok(());
                }
                proto::im_set_preedit(&mut self.conn, self.im, text, cursor, cursor)?;
                // serial 必须等于已收到的 done 事件计数，否则严格实现的合成器会静默丢弃
                proto::im_commit(&mut self.conn, self.im, self.done_serial)
            }
        }
    }

    /// 上屏一段文字。v1 的 commit_string 自带「清空预编辑」语义，不需要再补一次空 preedit。
    fn send_commit_string(&mut self, text: &str) -> std::io::Result<()> {
        match self.fe {
            Frontend::V1 => {
                if self.ctx == 0 {
                    return Ok(());
                }
                proto::ctx_commit_string(&mut self.conn, self.ctx, self.ctx_serial, text)
            }
            _ => {
                if self.im == 0 {
                    return Ok(());
                }
                proto::im_commit_string(&mut self.conn, self.im, text)
            }
        }
    }

    fn commit_text(&mut self, text: &str) -> std::io::Result<()> {
        if !self.ready() {
            return Ok(());
        }
        let serial = if self.fe == Frontend::V1 {
            self.ctx_serial
        } else {
            self.done_serial
        };
        self.log(&format!("commit \"{text}\" (serial={serial})"));
        if !text.is_empty() {
            self.send_commit_string(text)?;
        }
        self.send_preedit("", 0)?;
        self.engine.reset();
        self.en.reset();
        self.page = 0;
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
        // Slint 路径也同步形态/主题：颜色、度量、布局、列数都可能变。
        if let Some(ui) = self.slint_ui.as_mut() {
            ui.reload(&self.style);
        }
        self.log(&format!(
            "配置已重载：主题={} 形态={}",
            self.style.theme.name,
            self.style.layout.name()
        ));
    }

    fn refresh(&mut self) -> std::io::Result<()> {
        if !self.ready() {
            return Ok(());
        }
        self.maybe_reload_config();
        if !self.chinese {
            return self.refresh_en();
        }
        if self.inline_cands {
            let s = self.engine.inline_preedit();
            let len = s.len() as i32;
            self.send_preedit(&s, len)?;
            self.popup_hide()?;
            return Ok(());
        }
        let p = self.engine.preedit.clone();
        let len = p.len() as i32;
        self.send_preedit(&p, len)?;

        let cands = self.engine.cands.clone();
        if cands.is_empty() {
            self.popup_hide()?;
        } else {
            // 候选集可能已变（如退格），页码越界则回退到第 0 页，避免候选窗消失
            if self.page > 0 && self.page * self.style.max_items() >= cands.len() {
                self.page = 0;
            }
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
            if self.ready() {
                self.send_preedit("", 0)?;
            }
            return self.popup_hide();
        }
        let len = p.len() as i32;
        self.send_preedit(&p, len)?;

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

    /// v1 的候选窗角色：zwp_input_panel_v1 同一时刻只允许一个客户端持有，
    /// 所以延后到真的要弹候选窗时才 bind。
    fn panel_ensure(&mut self) -> std::io::Result<()> {
        if self.panel != 0 || self.panel_global == 0 {
            return Ok(());
        }
        let id = self.conn.alloc_id();
        proto::bind(
            &mut self.conn,
            self.reg,
            self.panel_global,
            proto::IFACE_INPUT_PANEL,
            1,
            id,
        )?;
        self.panel = id;
        self.log("已绑定 zwp_input_panel_v1");
        Ok(())
    }

    fn popup_create(&mut self) -> std::io::Result<()> {
        if self.popup_surf != 0 || self.comp == 0 {
            return Ok(());
        }
        // 先确认候选窗角色有归属对象：v2 是输入法对象，v1 是 input_panel
        if self.fe == Frontend::V1 {
            self.panel_ensure()?;
            if self.panel == 0 {
                self.log("没有 zwp_input_panel_v1，候选窗只能走内联形态");
                return Ok(());
            }
        } else if self.im == 0 {
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
        if self.fe == Frontend::V1 {
            // v1 没有 input_popup_surface：overlay panel 就是「贴着光标的浮层」，
            // 位置由合成器按当前输入光标算，xime 自己不摆坐标
            proto::panel_get_surface(&mut self.conn, self.panel, surf, role)?;
            proto::panel_set_overlay(&mut self.conn, role)?;
        } else {
            proto::im_get_popup(&mut self.conn, self.im, role, surf)?;
        }
        self.popup_surf = surf;
        self.popup_role = role;

        // 绑定分数缩放：合成器按该 surface 所在显示器发 preferred_scale，
        // 混合 DPI 多显示器下比全局 wl_output 更准。XIME_SCALE 锁定时无须绑定。
        if self.frac_scale_mgr != 0 && !self.scale_locked {
            self.frac_scale = self.conn.alloc_id();
            proto::get_fractional_scale(
                &mut self.conn,
                self.frac_scale_mgr,
                surf,
                self.frac_scale,
            )?;
        }
        // viewport：物理 buffer 需要声明对应的逻辑显示尺寸，合成器才不会
        // 在缩放输出上把候选条放大成 scale 倍。scale=1 时不用设置。
        if self.viewporter != 0 {
            self.viewport = self.conn.alloc_id();
            proto::get_viewport(&mut self.conn, self.viewporter, surf, self.viewport)?;
        }

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
        self.buf = Some(Buffer {
            shm,
            pool,
            buffer,
            w,
            h,
        });
        Ok(())
    }

    fn popup_update(&mut self, cands: &[String], mode: popup::Mode) -> std::io::Result<()> {
        if self.popup_surf == 0 {
            return Ok(());
        }
        let (start, end) = page_range(cands.len(), self.page, self.style.max_items());
        let shown: Vec<String> = cands[start..end].to_vec();
        if shown.is_empty() {
            return self.popup_hide();
        }
        let has_prev = self.page > 0;
        let has_next = end < cands.len();

        // 渲染来源：Slint 布局 + 软件光栅（默认），或手写 Canvas（--legacy-ui）。
        // 两条路产出都是 ARGB8888 像素 + 尺寸，后面的 shm 上传流程完全相同。
        let (w, h, pixels): (i32, i32, Vec<u32>) = if let Some(ui) = self.slint_ui.as_mut() {
            let (w, h) = ui.render(&shown, has_prev, has_next);
            if w <= 0 || h <= 0 {
                return self.popup_hide();
            }
            (w, h, ui.out().to_vec())
        } else {
            let probe = Canvas::new(1, 1);
            let (w, h) = popup::measure(&probe, &shown, &self.style);
            if w <= 0 || h <= 0 {
                return Ok(());
            }
            let mut c = Canvas::new(w, h);
            popup::paint(&mut c, &shown, &self.style, mode);
            (w, h, c.px)
        };

        self.ensure_buffer(w, h)?;
        if let Some(b) = &mut self.buf {
            b.shm.upload(&pixels);
        }
        if let Some(b) = &self.buf {
            proto::surface_attach(&mut self.conn, self.popup_surf, Some(b.buffer), 0, 0)?;
        }
        // viewport：告诉合成器物理 buffer 对应的逻辑显示尺寸。Slint 按 scale 渲染，
        // 这里把物理尺寸除回 scale，候选条在缩放屏上才维持与 scale=1 一致的视觉大小。
        // legacy 渲染不涉及 scale，destination 就等于 buffer 尺寸（等效无缩放）。
        if self.viewport != 0 {
            let (lw, lh) = if self.slint_ui.is_some() {
                let s = self.scale.max(0.01);
                (
                    ((w as f32) / s).round().max(1.0) as i32,
                    ((h as f32) / s).round().max(1.0) as i32,
                )
            } else {
                (w, h)
            };
            proto::viewport_set_destination(&mut self.conn, self.viewport, lw, lh)?;
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
        proto::vk_keymap(
            &mut self.conn,
            self.vk,
            KEYMAP_FORMAT_XKB_V1,
            shm.fd,
            raw.len() as u32,
        )?;
        Ok(())
    }

    fn forward_key(&mut self, time: u32, key: u32, state: u32) -> std::io::Result<()> {
        // v1：把键原样还给应用。keycode 与合成器自己的布局一致，不需要重新解释，
        // 也就没有「先同步 keymap 再注入」这一层（那是 v2 虚拟键盘的活）。
        if self.fe == Frontend::V1 {
            if self.ctx == 0 {
                self.log(&format!("没有输入上下文，丢弃按键 {key}"));
                return Ok(());
            }
            self.log(&format!("ctx 回注 key={key} state={state}"));
            return proto::ctx_key(&mut self.conn, self.ctx, self.key_serial, time, key, state);
        }
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
            libc::pread(
                fd,
                buf[off..].as_mut_ptr() as *mut libc::c_void,
                size - off,
                off as i64,
            )
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

/// keysym → 中文标点。DBus 前端（`fcitx_dbus`）复用同一张表，
/// 两条路径的中文标点行为才不会一个全角一个半角。
pub(crate) fn punct_of(sym: u32) -> Option<&'static str> {
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
    println!("  --frontend FE       输入法协议前端: auto(默认) | v2 | v1");
    println!("                      v2: Hyprland/sway/labwc；v1: KDE/KWin（由 KWin 拉起时自动选）");
    println!("  --inline-candidates 把候选塞进 preedit 由应用字体渲染（不建候选窗）");
    println!(
        "  --legacy-ui         候选窗用手写 Canvas 渲染（默认用 Slint 软件渲染）；对照/救急用"
    );
    println!("  --popup FORM        候选窗形态: horizontal(默认) | vertical | grid");
    println!("  --popup-cols N      网格形态的列数（默认 3）");
    println!("  --theme NAME        候选窗主题: light(默认) | dark | fcitx:NAME | 主题文件路径");
    println!("                      例: --theme fcitx:omarchy  直接用 fcitx5 的主题");
    println!("  --list-themes       列出能找到的 fcitx5 主题");
    println!("  --dump-theme        打印当前生效主题的颜色与度量（排查配置用）");
    println!(
        "  --zh VARIANT        输出形态: hans(默认,简体) | hant(繁体)；运行中 Ctrl+Shift+F 切换"
    );
    println!(
        "  --en                启动即英文模式（默认不出候选窗，按键直接进应用，与 fcitx5 一致）"
    );
    println!("  --en-completion     英文模式启用前缀补全 + 候选窗（旧行为，按需开启）");
    println!("  --check             诊断：检查协议可用性与接线状态");
    println!("  --dbus              以 fcitx5 DBus 前端运行（接管 org.fcitx.Fcitx5）");
    println!("                      用于微信/WPS/Electron/GNOME/X11 等自带 fcitx5-qt 的程序；");
    println!("                      与 fcitx5 及本程序的 Wayland 前端互斥，二者只能留一个");
    println!("  --query 拼音        打印该拼音串的候选与解码代价（调参用）");
    println!("  --render-test PATH [拼音] [页码]  把候选窗渲染成 BMP（可用 --popup/--theme 预览各形态）");
    println!("  --ui-probe PATH [拼音] [scale] [页码]  用 Slint 软件渲染器画候选条写 BMP（可指定缩放/页码）");
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

/** 从命令行解析协议前端选择 */
fn parse_frontend(args: &[String]) -> Frontend {
    match flag_value(args, "--frontend") {
        Some(v) => match Frontend::parse(&v) {
            Some(f) => f,
            None => {
                eprintln!("xime: 未知前端 {v}（可选 auto/v2/v1）");
                std::process::exit(2);
            }
        },
        None => Frontend::Auto,
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
    println!(
        "主题   {} ({})",
        t.name,
        if t.dark { "dark" } else { "light" }
    );
    println!("形态   {}  cols={}", s.layout.name(), s.cols);
    println!(
        "颜色   bg={} border={} text={} accent={} selected_text={}",
        config::to_hex(t.colors.bg),
        config::to_hex(t.colors.border),
        config::to_hex(t.colors.text),
        config::to_hex(t.colors.accent),
        config::to_hex(t.colors.sel_text)
    );
    println!(
        "度量   pad=({},{}) item_pad_x={} item_gap={} row_gap={}",
        m.pad_x, m.pad_y, m.item_pad_x, m.item_gap, m.row_gap
    );
    println!(
        "       radius={} hl_radius={} hl_pad_y={} border_width={}",
        m.radius, m.hl_radius, m.hl_pad_y, m.border_w
    );
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
        let path = args
            .get(i + 1)
            .map(|s| s.as_str())
            .unwrap_or("/tmp/keymap.txt");
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
        let path = args
            .get(i + 1)
            .map(|s| s.as_str())
            .unwrap_or("/tmp/popup.bmp");
        let input = args.get(i + 2).map(|s| s.as_str()).unwrap_or("nihao");
        let page = args
            .get(i + 3)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        render_test(path, input, page, &style, variant, english);
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--ui-probe") {
        let path = args
            .get(i + 1)
            .map(|s| s.as_str())
            .unwrap_or("/tmp/slint.bmp");
        let input = args.get(i + 2).map(|s| s.as_str()).unwrap_or("你好");
        let scale = args
            .get(i + 3)
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(1.0);
        let page = args
            .get(i + 4)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        ui_probe(path, input, scale, page, &style);
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
    // 候选窗 UI：默认 Slint；--legacy-ui 回退到手写 Canvas（对照/救急用）。
    let use_slint = !args.iter().any(|a| a == "--legacy-ui");

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
        use_slint,
        parse_frontend(&args),
    ) {
        Some(x) => x,
        None => std::process::exit(1),
    };
    if let Err(e) = x.run() {
        eprintln!("xime: {e}");
        std::process::exit(1);
    }
    // 没席位（见 after_registry）：退出码 1，区别于「已有实例」那种 0 的安静退出
    if x.no_seat {
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

    let has_v2 = have.iter().any(|(_, h, _)| h == proto::IFACE_INPUT_METHOD);
    let has_v1 = have
        .iter()
        .any(|(_, h, _)| h == proto::IFACE_INPUT_METHOD_V1);
    let has_panel = have.iter().any(|(_, h, _)| h == proto::IFACE_INPUT_PANEL);

    let mut ok = true;
    println!("协议可用性：");
    for w in [proto::IFACE_COMPOSITOR, proto::IFACE_SHM, proto::IFACE_SEAT] {
        let has = have.iter().any(|(_, h, _)| h == w);
        println!("  {:<38} {}", w, if has { "可用" } else { "缺失" });
        if !has {
            ok = false;
        }
    }

    // 输入法席位：v2（wlroots 系）与 v1（KWin）二选一，有哪个用哪个
    match (has_v2, has_v1) {
        (true, _) => println!("  {:<38} 可用（zwp_input_method_manager_v2）", "输入法席位"),
        (false, true) => println!(
            "  {:<38} 可用（zwp_input_method_v1，KWin 拉起时）",
            "输入法席位"
        ),
        (false, false) => {
            println!("  {:<38} 缺失", "输入法席位");
            ok = false;
        }
    }
    if has_v1 {
        println!(
            "  {:<38} {}",
            "候选窗角色（v1）",
            if has_panel {
                "可用（zwp_input_panel_v1）"
            } else {
                "缺失，候选窗只能用 --inline-candidates"
            }
        );
    }

    // 全局接口存在 ≠ 拿得到：v2 的席位同一时刻只属于一个客户端
    let seat_g = have
        .iter()
        .find(|(_, h, _)| h == proto::IFACE_SEAT)
        .cloned();
    let mgr_g = have
        .iter()
        .find(|(_, h, _)| h == proto::IFACE_INPUT_METHOD)
        .cloned();
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
    //
    // v3/v2/v1 可能同时存在（KWin 三个都提供），现代应用走 v3。早先只取「第一个命中的」，
    // KWin 下会报成 v1，看起来像「应用只能用老协议」，排查时很误导。
    let ti: Vec<&str> = [
        "zwp_text_input_manager_v3",
        "zwp_text_input_manager_v2",
        "zwp_text_input_manager_v1",
    ]
    .into_iter()
    .filter(|w| have.iter().any(|(_, h, _)| h == w))
    .collect();
    match ti.split_first() {
        Some((best, rest)) => {
            let extra = if rest.is_empty() {
                String::new()
            } else {
                format!(
                    "；另有 {}",
                    rest.iter()
                        .map(|h| h.rsplit('_').next().unwrap_or(h))
                        .collect::<Vec<_>>()
                        .join("、")
                )
            };
            println!("  {:<38} 可用（{best}{extra}）", "text-input（应用侧）")
        }
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
    println!(
        "英文词表：{} 词（前缀补全，Shift 切换中英）",
        en::word_count()
    );
    println!("字形数量：{} 个", font::Atlas::get().count());
    let (nc, np, nw) = zh::stats();
    println!(
        "简繁数据：{nc} 个单字映射 / {np} 条词级例外 / {nw} 条词表（{} 条繁体形词条）",
        zh::trad_word_count()
    );
    println!(
        "输出形态：{}（--zh hant 或运行时 Ctrl+Shift+F 切换）",
        variant.label()
    );
    if !has_v2 && !has_v1 {
        eprintln!(
            "合成器没提供任何输入法协议：GNOME/Mutter 两者都没有；\n\
             KDE 下则说明这条连接不是 KWin 交给输入法的那条。"
        );
    } else if !has_v2 {
        eprintln!(
            "当前只有 zwp_input_method_v1（KDE/KWin）：必须由 KWin 拉起才能拿到席位，\n\
             系统设置 → 键盘 → 虚拟键盘里选 xime，或 xime-setup --auto。"
        );
    }
    if !ok {
        std::process::exit(1);
    }
}

/// 扫 /proc 找出正在跑的其它输入法（它们会占掉 input-method 席位）
fn running_imes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmd) = std::fs::read_to_string(e.path().join("cmdline")) else {
            continue;
        };
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
fn render_test(path: &str, input: &str, page: usize, style: &popup::Style, variant: zh::Variant, english: bool) {
    let (cands, mode, label) = if english {
        let mut e = en::EnEngine::new();
        for ch in input.chars() {
            e.push_char(ch, style.max_items());
        }
        (
            e.cands.clone(),
            popup::Mode::En,
            format!("英文/{}", e.case_mode_label()),
        )
    } else {
        dict::init();
        let mut e = Engine::new();
        e.set_variant(variant);
        for c in input.chars() {
            e.push_char(c);
        }
        (
            e.cands.clone(),
            popup::Mode::Zh,
            variant.label().to_string(),
        )
    };

    let (start, end) = page_range(cands.len(), page, style.max_items());
    let shown: Vec<String> = cands[start..end].to_vec();
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

/// Slint 候选条渲染探针：用 Slint 软件渲染器画一条候选并写 BMP，供肉眼校验。
/// scale 参数用于验证 HiDPI 缩放（逻辑布局不变，输出物理像素按比例放大）。
/// 形态/主题跟随命令行 --popup / --popup-cols / --theme。
fn ui_probe(path: &str, input: &str, scale: f32, page: usize, style: &popup::Style) {
    dict::init();
    let mut e = Engine::new();
    for c in input.chars() {
        e.push_char(c);
    }
    let (start, end) = page_range(e.cands.len(), page, style.max_items());
    let cands: Vec<String> = e.cands[start..end].to_vec();
    if cands.is_empty() {
        eprintln!("无候选");
        std::process::exit(1);
    }
    let has_prev = page > 0;
    let has_next = end < e.cands.len();
    let mut ui = match ui_slint::SlintUi::new(scale, style) {
        Some(u) => u,
        None => {
            eprintln!("Slint 平台初始化失败（可能测试进程里已有平台）");
            std::process::exit(1);
        }
    };
    let (w, h) = ui.render(&cands, has_prev, has_next);
    match ui.write_bmp(path, true) {
        Ok(()) => println!(
            "已写出 {path}  {w}x{h} 物理像素 (scale={:.2}, Slint 软件渲染)  形态 {}（{} 列）  主题 {}  候选 {} 条",
            ui.scale(),
            style.layout.name(),
            style.cols,
            style.theme.name.as_str(),
            cands.len()
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
    println!(
        "首次载入词库（含一次引擎构造）: {:.2} ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "输出形态: {}（繁体会在候选输出环节多做一次查表转换）",
        variant.label()
    );

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
    fn simplified_traditional_toggle_key() {
        let both = keymap::MASK_CONTROL | keymap::MASK_SHIFT;
        assert!(is_variant_toggle(b'f' as u32, both));
        assert!(
            is_variant_toggle(b'F' as u32, both),
            "Shift 生效后 sym 是大写 F"
        );
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
    fn shift_keycode() {
        // evdev：左 Shift 42、右 Shift 54
        assert!(is_shift_key(42));
        assert!(is_shift_key(54));
        // 字母 a 是 38，别把普通键当切换键
        assert!(!is_shift_key(38));
        assert!(!is_shift_key(0));
    }

    #[test]
    fn ascii_keeps_case_pinyin_lowercased() {
        assert_eq!(letter_keep_case(b'a' as u32), Some('a'));
        assert_eq!(
            letter_keep_case(b'A' as u32),
            Some('A'),
            "英文要大写就得上大写"
        );
        assert_eq!(letter_keep_case(b'1' as u32), None);
        assert_eq!(letter_keep_case(b' ' as u32), None);
        // 拼音不需要大小写，走另一条路径
        assert_eq!(ascii_letter(b'A' as u32), Some('a'));
        assert_eq!(ascii_letter(b'a' as u32), Some('a'));
    }

    #[test]
    fn page_range_bounds() {
        // 第 0 页：从 0 起，取满一页
        assert_eq!(page_range(20, 0, 6), (0, 6));
        // 中间页
        assert_eq!(page_range(20, 1, 6), (6, 12));
        // 末页不满一页：end 收在 total
        assert_eq!(page_range(20, 3, 6), (18, 20));
        // 页码越界：start 收在 total，得到空切片（回退到第 0 页由调用方负责）
        assert_eq!(page_range(20, 4, 6), (20, 20));
        // 空候选
        assert_eq!(page_range(0, 0, 6), (0, 0));
        // 恰好一页时下一页翻不动
        assert_eq!(page_range(9, 0, 9), (0, 9));
        assert_eq!(page_range(9, 1, 9), (9, 9));
    }
}
