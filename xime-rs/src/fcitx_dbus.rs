//! fcitx5 DBus 前端。
//!
//! 让「内置了 fcitx5-qt / fcitx5-gtk 客户端」的程序用上 xime 的引擎：
//! 微信、WPS、Electron 应用、以及 X11 / GNOME 会话下的任何 GTK/Qt 程序。
//!
//! 与 Wayland 前端的关系：
//! - 两者互斥（DBus 前端要独占 `org.fcitx.Fcitx5` 服务名，fcitx5 也一样）
//! - 共用 `engine` / `dict` / `zh`，不共享任何 Wayland 代码
//! - 候选窗由客户端自绘（Qt 客户端恒声明 ClientSideInputPanel），
//!   所以这里不需要 atlas、render、shm、popup 一行代码——这正是 Wayland
//!   路径上最费劲的部分
//!
//! 协议实现依据 `docs/fcitx5-dbus-frontend.md`，并经 Phase 0/0.5 实测验证。

use std::sync::{Arc, Mutex};

use zbus::interface;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::Connection;

use crate::engine::Engine;
use crate::zh::Variant;

const IM_PATH: &str = "/org/freedesktop/portal/inputmethod";
const IC_PREFIX: &str = "/org/freedesktop/portal/inputcontext";
const SERVICE_MAIN: &str = "org.fcitx.Fcitx5";
/// fcitx5 会同时占这个老名字，客户端两种都可能查
const SERVICE_LEGACY: &str = "org.fcitx.Fcitx-0";

/// 能力位（fcitx5-qt 的 flags.h）：注意 Password 是 bit3、Sensitive 是 bit36。
/// 曾经误以为在 bit0/bit1，导致微信（caps=0x1e801800032，bit35 Multiline 置位）
/// 被当成密码框，所有按键都被透传。
const CAP_PASSWORD: u64 = 1 << 3;
const CAP_SENSITIVE: u64 = 1 << 36;

/// 修饰键位（与 fcitx5-qt 的 flags.h 一致）
const ST_SHIFT: u32 = 1 << 0;
const ST_CAPSLOCK: u32 = 1 << 1;
const ST_CTRL: u32 = 1 << 2;
#[allow(dead_code)]
const ST_ALT: u32 = 1 << 3;

/// X11 keysym
const KS_SPACE: u32 = 0x20;
const KS_BACKSPACE: u32 = 0xFF08;
const KS_RETURN: u32 = 0xFF0D;
const KS_ESCAPE: u32 = 0xFF1B;
const KS_SHIFT_L: u32 = 0xFFE1;
const KS_SHIFT_R: u32 = 0xFFE2;

/// 候选窗排版：2 = 水平排列（Phase 0.5 实测微信能正确渲染）
const LAYOUT_HORIZONTAL: i32 = 2;

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

struct Global {
    inner: Mutex<GlobalInner>,
}

struct GlobalInner {
    chinese: bool,
    variant: Variant,
    verbose: bool,
    next_id: u32,
}

impl Global {
    fn new(variant: Variant, verbose: bool) -> Arc<Global> {
        Arc::new(Global {
            inner: Mutex::new(GlobalInner {
                chinese: true,
                variant,
                verbose,
                next_id: 0,
            }),
        })
    }

    fn log(&self, msg: &str) {
        let on = self.inner.lock().map(|g| g.verbose).unwrap_or(false);
        if on {
            eprintln!("xime-dbus: {msg}");
        }
    }

    fn chinese(&self) -> bool {
        self.inner.lock().map(|g| g.chinese).unwrap_or(true)
    }

    fn toggle(&self) -> bool {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return true,
        };
        g.chinese = !g.chinese;
        let now = g.chinese;
        if g.verbose {
            eprintln!("xime-dbus: 切换为{}", if now { "中文" } else { "英文" });
        }
        now
    }

    fn alloc_id(&self) -> u32 {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return 1,
        };
        g.next_id += 1;
        g.next_id
    }
}

// ---------------------------------------------------------------------------
// 输入上下文
// ---------------------------------------------------------------------------

/// 一次按键处理后需要对外做的事，全部在锁外执行（信号发送是 async）
struct Outcome {
    consumed: bool,
    commit: Option<String>,
    /// Some 表示要推送一次 UI（组字串 / 候选）
    ui: Option<Snapshot>,
}

struct Snapshot {
    preedit: String,
    cands: Vec<String>,
    sel: usize,
}

impl Outcome {
    fn ignore() -> Outcome {
        Outcome {
            consumed: false,
            commit: None,
            ui: None,
        }
    }
    fn eat() -> Outcome {
        Outcome {
            consumed: true,
            commit: None,
            ui: None,
        }
    }
}

struct CtxInner {
    engine: Engine,
    sel: usize,
    focused: bool,
    /// 当前焦点是密码框：全键透传
    sensitive: bool,
    /// Shift 按下但未松开：松开时决定是切换中英还是输入大写
    shift_pending: bool,
    /// 上次按键是否被消费，松开事件跟着走，避免「按下吞了、松开没吞」的抖动
    last_consumed: bool,
}

struct InputContext {
    id: u32,
    global: Arc<Global>,
    inner: Mutex<CtxInner>,
}

impl InputContext {
    fn new(id: u32, global: Arc<Global>, variant: Variant) -> InputContext {
        let mut engine = Engine::new();
        engine.set_variant(variant);
        InputContext {
            id,
            global,
            inner: Mutex::new(CtxInner {
                engine,
                sel: 0,
                focused: false,
                sensitive: false,
                shift_pending: false,
                last_consumed: false,
            }),
        }
    }

    fn sync_variant(&self) {
        let v = match self.global.inner.lock() {
            Ok(g) => g.variant,
            Err(_) => return,
        };
        if let Ok(mut st) = self.inner.lock() {
            st.engine.set_variant(v);
        }
    }

    /// 锁内完成按键处理，返回要在锁外执行的动作
    fn handle_key(&self, keyval: u32, state: u32, press: bool) -> Outcome {
        let mut st = match self.inner.lock() {
            Ok(s) => s,
            Err(_) => return Outcome::ignore(),
        };

        // 松开事件：跟随上一次按下的消费结果
        if !press {
            let c = st.last_consumed;
            return Outcome {
                consumed: c,
                commit: None,
                ui: None,
            };
        }

        // 密码框 / PIN：任何字符都不拦，交回应用自己处理
        if st.sensitive {
            st.last_consumed = false;
            return Outcome::ignore();
        }

        // Ctrl+Space 切换中英（先于其它判断，避免空格被当成上屏）
        if keyval == KS_SPACE && (state & ST_CTRL) != 0 {
            st.shift_pending = false;
            st.last_consumed = true;
            return Outcome::eat();
        }

        // Shift 单击切换中英：这里只记账，松开时再决定
        if keyval == KS_SHIFT_L || keyval == KS_SHIFT_R {
            st.shift_pending = true;
            st.last_consumed = false;
            return Outcome::ignore();
        }
        st.shift_pending = false;

        let chinese = self.global.chinese();
        let composing = st.engine.active();

        // 英文模式下不组字，一律透传（与 Wayland 前端默认行为一致）
        if !chinese {
            st.last_consumed = false;
            return Outcome::ignore();
        }

        // ---- 组字中 ----
        if composing {
            // 空格 / 回车：提交首候选
            if keyval == KS_SPACE || keyval == KS_RETURN {
                let text = st
                    .engine
                    .cands
                    .first()
                    .cloned()
                    .unwrap_or_else(|| st.engine.input.clone());
                st.engine.reset();
                st.sel = 0;
                st.last_consumed = true;
                return Outcome {
                    consumed: true,
                    commit: Some(text),
                    ui: Some(Snapshot {
                        preedit: String::new(),
                        cands: Vec::new(),
                        sel: 0,
                    }),
                };
            }
            // Esc：放弃这次组字
            if keyval == KS_ESCAPE {
                st.engine.reset();
                st.sel = 0;
                st.last_consumed = true;
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(Snapshot {
                        preedit: String::new(),
                        cands: Vec::new(),
                        sel: 0,
                    }),
                };
            }
            // 退格
            if keyval == KS_BACKSPACE {
                st.engine.backspace();
                st.sel = 0;
                let snap = snapshot(&st);
                st.last_consumed = true;
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(snap),
                };
            }
            // 数字 1-9 选词
            if (0x31..=0x39).contains(&keyval) {
                let idx = (keyval - 0x31) as usize;
                if let Some(w) = st.engine.cands.get(idx).cloned() {
                    st.engine.reset();
                    st.sel = 0;
                    st.last_consumed = true;
                    return Outcome {
                        consumed: true,
                        commit: Some(w),
                        ui: Some(Snapshot {
                            preedit: String::new(),
                            cands: Vec::new(),
                            sel: 0,
                        }),
                    };
                }
                // 超出候选范围：不吞键
                st.last_consumed = false;
                return Outcome::ignore();
            }
        }

        // 字母：进入或延续组字
        if let Some(c) = ascii_letter(keyval) {
            let forced_lower = (state & (ST_SHIFT | ST_CAPSLOCK)) == 0;
            let ch = if forced_lower { c.to_ascii_lowercase() } else { c };
            st.engine.push_char(ch.to_ascii_lowercase());
            st.sel = 0;
            let snap = snapshot(&st);
            st.last_consumed = true;
            return Outcome {
                consumed: true,
                commit: None,
                ui: Some(snap),
            };
        }

        // 标点等：不组字时透传；组字中若输入标点，先把候选上屏再透传
        if composing {
            let text = st
                .engine
                .cands
                .first()
                .cloned()
                .unwrap_or_else(|| st.engine.input.clone());
            st.engine.reset();
            st.sel = 0;
            st.last_consumed = false;
            return Outcome {
                consumed: false,
                commit: Some(text),
                ui: Some(Snapshot {
                    preedit: String::new(),
                    cands: Vec::new(),
                    sel: 0,
                }),
            };
        }

        st.last_consumed = false;
        Outcome::ignore()
    }
}

fn snapshot(st: &CtxInner) -> Snapshot {
    let mut cands = st.engine.cands.clone();
    if st.sel < cands.len() {
        // 让首候选与当前高亮一致，客户端据此画选中态
        if st.sel > 0 {
            let w = cands.remove(st.sel);
            cands.insert(0, w);
        }
    }
    Snapshot {
        preedit: st.engine.input.clone(),
        cands,
        sel: 0,
    }
}

fn ascii_letter(keyval: u32) -> Option<char> {
    let c = char::from_u32(keyval)?;
    if c.is_ascii_alphabetic() {
        Some(c)
    } else {
        None
    }
}

#[interface(name = "org.fcitx.Fcitx.InputContext1")]
impl InputContext {
    #[zbus(signal)]
    async fn commit_string(emitter: &SignalEmitter<'_>, text: &str) -> zbus::Result<()>;

    #[zbus(name = "UpdateClientSideUI")]
    #[zbus(signal)]
    #[allow(clippy::too_many_arguments)]
    async fn update_client_side_ui(
        emitter: &SignalEmitter<'_>,
        preedit: Vec<(String, i32)>,
        cursorpos: i32,
        aux_up: Vec<(String, i32)>,
        aux_down: Vec<(String, i32)>,
        candidates: Vec<(String, String)>,
        candidate_index: i32,
        layout_hint: i32,
        has_prev: bool,
        has_next: bool,
    ) -> zbus::Result<()>;

    async fn focus_in(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.focused = true;
        }
        self.global.log(&format!("ic{} FocusIn", self.id));
    }

    async fn focus_out(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.focused = false;
            st.engine.reset();
            st.shift_pending = false;
        }
        self.global.log(&format!("ic{} FocusOut", self.id));
    }

    async fn reset(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.engine.reset();
            st.sel = 0;
            st.shift_pending = false;
            st.last_consumed = false;
        }
    }

    async fn set_cursor_rect(&self, x: i32, y: i32, w: i32, h: i32) {
        self.global
            .log(&format!("ic{} 光标矩形 {}x{} @{x},{y}", self.id, w, h));
    }

    async fn set_cursor_rect_v2(&self, x: i32, y: i32, w: i32, h: i32, scale: f64) {
        self.global
            .log(&format!("ic{} 光标矩形 {}x{} @{x},{y} 缩放{scale}", self.id, w, h));
    }

    async fn set_supported_capability(&self, caps: u64) {
        self.global
            .log(&format!("ic{} 支持能力 {caps:#x}", self.id));
    }

    async fn set_capability(&self, caps: u64) {
        // 密码框 / 敏感输入：不保留任何组字状态
        let sensitive = (caps & (CAP_PASSWORD | CAP_SENSITIVE)) != 0;
        if let Ok(mut st) = self.inner.lock() {
            st.sensitive = sensitive;
            if sensitive {
                st.engine.reset();
            }
        }
        self.global
            .log(&format!("ic{} 当前能力 {caps:#x} 敏感={sensitive}", self.id));
    }

    async fn set_surrounding_text(&self, text: String, cursor: u32, anchor: u32) {
        self.global.log(&format!(
            "ic{} 周边文本 {:?} cursor={cursor} anchor={anchor}",
            self.id,
            truncate(&text, 40)
        ));
    }

    async fn set_surrounding_text_position(&self, _cursor: u32, _anchor: u32) {}

    async fn destroy_ic(&self) {
        self.global.log(&format!("ic{} DestroyIC", self.id));
    }

    async fn process_key_event(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        keyval: u32,
        keycode: u32,
        state: u32,
        press: bool,
        _time: u32,
    ) -> bool {
        self.sync_variant();
        let out = self.handle_key(keyval, state, press);

        if self.global_chinese_changed_to_shift_release(keyval, press) {
            // Shift 单击松开：切换中英，并清掉组字
            self.global.toggle();
            if let Ok(mut st) = self.inner.lock() {
                st.engine.reset();
                st.sel = 0;
            }
        }

        if let Some(text) = out.commit {
            if !text.is_empty() {
                self.global.log(&format!("ic{} commit {:?}", self.id, text));
                let _ = Self::commit_string(&emitter, &text).await;
            }
        }

        if let Some(snap) = out.ui {
            self.push_ui(&emitter, snap).await;
        }

        if self.global_verbose() {
            self.global.log(&format!(
                "ic{} 按键 keyval={keyval:#x} code={keycode} state={state:#x} 按下={press} -> 消费={}",
                self.id, out.consumed
            ));
        }
        out.consumed
    }

    async fn prev_page(&self) {}

    async fn next_page(&self) {}

    async fn select_candidate(&self, index: i32) {
        let text = {
            let mut st = match self.inner.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            match st.engine.cands.get(index.max(0) as usize).cloned() {
                Some(w) => {
                    st.engine.reset();
                    st.sel = 0;
                    Some(w)
                }
                None => None,
            }
        };
        if let Some(t) = text {
            self.global
                .log(&format!("ic{} 选词 {index} -> {:?}", self.id, t));
            // 选词信号只能通过 emitter 发，这里借助一个临时 emitter 不可行，
            // 交给调用方在 process_key_event / 外部路径处理。
            // SelectCandidate 由客户端主动调用时，用 pending 队列。
            self.queue_commit(t);
        }
    }

    async fn invoke_action(&self, _action: u32, _cursor: i32) {}

    async fn show_virtual_keyboard(&self) {}

    async fn hide_virtual_keyboard(&self) {}

    async fn is_virtual_keyboard_visible(&self) -> bool {
        false
    }
}

impl InputContext {
    fn global_verbose(&self) -> bool {
        self.global.inner.lock().map(|g| g.verbose).unwrap_or(false)
    }

    /// Shift 单击切换中英：按下时已记 shift_pending，这里判断松开
    fn global_chinese_changed_to_shift_release(&self, keyval: u32, press: bool) -> bool {
        if press || (keyval != KS_SHIFT_L && keyval != KS_SHIFT_R) {
            return false;
        }
        match self.inner.lock() {
            Ok(mut st) => {
                let p = st.shift_pending;
                st.shift_pending = false;
                p
            }
            Err(_) => false,
        }
    }

    fn queue_commit(&self, _text: String) {
        // 客户端主动 SelectCandidate 的路径暂未接线（fcitx5-qt 在
        // ClientSideInputPanel 模式下很少调用它，选词走 ProcessKeyEvent）。
        // 保留占位，避免客户端报 UnknownMethod。
    }

    async fn push_ui(&self, emitter: &SignalEmitter<'_>, snap: Snapshot) {
        let preedit = if snap.preedit.is_empty() {
            Vec::new()
        } else {
            vec![(snap.preedit.clone(), 1)] // 1 = Underline
        };
        let cursor = snap.preedit.chars().count() as i32;
        let cands: Vec<(String, String)> = snap
            .cands
            .iter()
            .take(9)
            .map(|c| (c.clone(), String::new()))
            .collect();
        let _ = Self::update_client_side_ui(
            emitter,
            preedit,
            cursor,
            Vec::new(),
            Vec::new(),
            cands,
            snap.sel as i32,
            LAYOUT_HORIZONTAL,
            false,
            false,
        )
        .await;
    }
}

fn truncate(s: &str, n: usize) -> String {
    let v: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{v}…")
    } else {
        v
    }
}

// ---------------------------------------------------------------------------
// InputMethod1：创建输入上下文
// ---------------------------------------------------------------------------

struct InputMethod {
    global: Arc<Global>,
    variant: Variant,
}

#[interface(name = "org.fcitx.Fcitx.InputMethod1")]
impl InputMethod {
    async fn create_input_context(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] _conn: &Connection,
        kvs: Vec<(String, String)>,
    ) -> zbus::fdo::Result<(zbus::OwnedObjectPath, Vec<u8>)> {
        let id = self.global.alloc_id();
        let path = format!("{IC_PREFIX}/{id}");
        let ctx = InputContext::new(id, self.global.clone(), self.variant);
        server
            .at(path.as_str(), ctx)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("注册 {path} 失败: {e}")))?;

        let program = kvs
            .iter()
            .find(|(k, _)| k == "program")
            .map(|(_, v)| v.as_str())
            .unwrap_or("?");
        self.global
            .log(&format!("CreateInputContext -> {path}（program={program}）"));

        let op = zbus::OwnedObjectPath::try_from(path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("路径非法: {e}")))?;
        // 客户端用它做去重；生成一个伪 UUID
        let uuid = {
            let n = id as u128 ^ 0x9e37_79b9_7f4a_7c15;
            n.to_be_bytes().to_vec()
        };
        Ok((op, uuid))
    }

    fn version(&self) -> u32 {
        1
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

pub fn run(variant: Variant, verbose: bool) -> Result<(), String> {
    let global = Global::new(variant, verbose);
    let im = InputMethod {
        global: global.clone(),
        variant,
    };

    zbus::block_on(async move {
        let conn = zbus::connection::Builder::session()
            .name(SERVICE_MAIN)
            .serve_at(IM_PATH, im)
            .build()
            .await
            .map_err(|e| format!("连上总线并申请 {SERVICE_MAIN} 失败（可能有 fcitx5 在跑）: {e}"))?;

        // 老名字尽力而为，失败不影响主名
        if let Err(e) = conn.request_name(SERVICE_LEGACY).await {
            eprintln!("xime-dbus: 附加服务名 {SERVICE_LEGACY} 未申请到（{e}），通常无碍");
        }

        eprintln!("xime-dbus: 已接管 {SERVICE_MAIN}，等待客户端连接");
        std::future::pending::<()>().await;
        Ok::<(), String>(())
    })
}
