//! fcitx5 DBus 前端。
//!
//! 让「内置了 fcitx5-qt / fcitx5-gtk 客户端」的程序用上 xime 的引擎：
//! 微信、WPS、Electron 应用、以及 X11 / GNOME 会话下的任何 GTK/Qt 程序。
//!
//! 与 Wayland 前端的关系：
//! - 与 **fcitx5 守护进程**互斥（都要独占 `org.fcitx.Fcitx5` 服务名）
//! - 与 **xime 自己的 Wayland 前端**可以同时跑（两个进程，见 `packaging/xime-both`）：
//!   应用在启动时各自选一条，X11/XWayland 走这里，Wayland 原生走那边
//! - 共用 `engine` / `dict` / `zh`，不共享任何 Wayland 代码
//! - 候选窗由客户端自绘（Qt / GTK 客户端都声明 ClientSideInputPanel），
//!   所以这里不需要 atlas、render、shm、popup 一行代码——这正是 Wayland
//!   路径上最费劲的部分
//!
//! 环境变量写 `fcitx` 或 `fcitx5` 都能连上：插件的 Key 只是「加载谁」，
//! 走哪套协议由服务名 `org.fcitx.Fcitx5` 是否被占用决定（见文档第十五节）。
//!
//! 协议实现依据 `docs/fcitx5-dbus-frontend.md`，并经 Phase 0/0.5 实测验证。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use zbus::interface;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::Connection;

use crate::en;
use crate::engine::Engine;
use crate::zh::Variant;

const IM_PATH: &str = "/org/freedesktop/portal/inputmethod";
const CONTROLLER_PATH: &str = "/controller";
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
/// 翻页键：`-` / `=` 与 PageUp / PageDown（fcitx5 拼音的默认绑定）
const KS_MINUS: u32 = 0x2D;
const KS_EQUAL: u32 = 0x3D;
const KS_PAGE_UP: u32 = 0xFF55;
const KS_PAGE_DOWN: u32 = 0xFF56;

/// 候选窗排版：2 = 水平排列（Phase 0.5 实测微信能正确渲染）
const LAYOUT_HORIZONTAL: i32 = 2;

/// 每页候选数：必须正好 9，数字键 1-9 才与之一一对应
const PAGE_SIZE: usize = 9;

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

struct Global {
    inner: Mutex<GlobalInner>,
}

struct GlobalInner {
    chinese: bool,
    /// 英文态是否启用「前缀补全 + 候选窗」。
    /// 默认关闭：fcitx5 的 keyboard-us 不出候选窗，xime 也照此处理（纯透传）。
    en_completion: bool,
    variant: Variant,
    verbose: bool,
    next_id: u32,
    /// DBus 客户端唯一名 → 它创建的 input context 路径列表。
    /// 客户端断开（唯一名被总线回收）时据此销毁它的全部 IC，避免 busctl tree 残留。
    contexts: HashMap<String, Vec<String>>,
}

/// 状态栏（omarchy 的 indicator）认这两个名字：keyboard-us = 英文直通，
/// pinyin = 中文组字。它通过 Controller1.SetCurrentIM 来切换。
const IM_EN: &str = "keyboard-us";
const IM_ZH: &str = "pinyin";

/// 英文态的前缀补全是否开启（`XIME_EN_COMPLETION=1` 时恢复旧行为）。
///
/// 默认关闭，和命令行前端的 `--en-completion` 语义一致：fcitx5 在英文态
/// （keyboard-us）不弹候选窗、不组字，xime 也不该弹——多出来的候选窗
/// 会让人以为输入法在「抢字」，和 fcitx5 的行为对不上。
fn en_completion_enabled() -> bool {
    matches!(
        std::env::var("XIME_EN_COMPLETION").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

impl Global {
    fn new(variant: Variant, verbose: bool) -> Arc<Global> {
        Arc::new(Global {
            inner: Mutex::new(GlobalInner {
                // 双前端共享中英状态（见 im_state 模块）：文件里有就以文件为准
                chinese: crate::im_state::read().unwrap_or(true),
                en_completion: en_completion_enabled(),
                variant,
                verbose,
                next_id: 0,
                contexts: HashMap::new(),
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

    fn en_completion(&self) -> bool {
        self.inner.lock().map(|g| g.en_completion).unwrap_or(false)
    }

    fn variant(&self) -> Variant {
        self.inner.lock().map(|g| g.variant).unwrap_or(Variant::Hans)
    }

    /// 简繁切换（Ctrl+Shift+F），返回切换后的形态，调用方据此就地重算
    fn toggle_variant(&self) -> Variant {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Variant::Hans,
        };
        g.variant = g.variant.toggled();
        let v = g.variant;
        if g.verbose {
            eprintln!("xime-dbus: 输出形态切换为{}", v.label());
        }
        v
    }

    /// 按输入法名设置中英状态，供状态栏 / 外部工具调用
    fn set_im(&self, name: &str) -> bool {
        let zh = match name {
            IM_ZH => true,
            IM_EN => false,
            _ => return false,
        };
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        g.chinese = zh;
        crate::im_state::write(zh);
        if g.verbose {
            eprintln!("xime-dbus: 切到 {name}（{}）", if zh { "中文" } else { "英文" });
        }
        true
    }

    fn im_name(&self) -> &'static str {
        if self.chinese() {
            IM_ZH
        } else {
            IM_EN
        }
    }

    fn toggle(&self) -> bool {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return true,
        };
        g.chinese = !g.chinese;
        let now = g.chinese;
        // 同步给 Wayland 前端，避免两边状态各自漂移
        crate::im_state::write(now);
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

    /// 把 Wayland 前端刚改过的中英状态同步过来。
    ///
    /// 两个进程分别服务不同应用：在终端（Wayland）里按 Shift 切英文，Qt 应用
    /// 这边必须跟着变，否则用户会遇到「同一个键在两边表现不同」且毫无提示。
    fn sync_shared(&self) {
        let Some(zh) = crate::im_state::read() else { return };
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if g.chinese == zh {
            return;
        }
        g.chinese = zh;
        if g.verbose {
            eprintln!(
                "xime-dbus: 同步为{}（另一前端切换）",
                if zh { "中文" } else { "英文" }
            );
        }
    }

    /// 记录某个客户端（owner 唯一名）名下新注册的 input context 路径。
    fn register_context(&self, owner: String, path: String) {
        if let Ok(mut g) = self.inner.lock() {
            g.contexts.entry(owner).or_default().push(path);
        }
    }

    /// 取走某个客户端名下的全部 IC 路径（并清空该条目），用于断开时回收。
    fn take_contexts(&self, owner: &str) -> Vec<String> {
        self.inner
            .lock()
            .map(|mut g| g.contexts.remove(owner).unwrap_or_default())
            .unwrap_or_default()
    }

    /// 从所有客户端的清单里摘掉一个已销毁的 IC 路径。
    ///
    /// 客户端自己发 DestroyIC 时用：路径要立刻从清单里移除，否则它断开连接时
    /// 还会对这个已不存在的对象再 remove 一次，日志里出现一串无害但误导的报错。
    fn forget_context(&self, path: &str) {
        if let Ok(mut g) = self.inner.lock() {
            for v in g.contexts.values_mut() {
                v.retain(|p| p != path);
            }
            g.contexts.retain(|_, v| !v.is_empty());
        }
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
    /// 当前页的候选（最多 PAGE_SIZE 条）
    cands: Vec<String>,
    sel: usize,
    has_prev: bool,
    has_next: bool,
}

impl Snapshot {
    fn empty() -> Snapshot {
        Snapshot {
            preedit: String::new(),
            cands: Vec::new(),
            sel: 0,
            has_prev: false,
            has_next: false,
        }
    }
}

impl Outcome {
    fn ignore() -> Outcome {
        Outcome {
            consumed: false,
            commit: None,
            ui: None,
        }
    }
}

struct CtxInner {
    /// 中文：拼音解码
    engine: Engine,
    /// 英文：前缀补全（中文模式下始终为空，切换时两边一起清）
    en: en::EnEngine,
    /// 候选页码。中文候选最多 128 条，一屏只放 9 条，需翻页
    page: usize,
    focused: bool,
    /// 当前焦点是密码框：全键透传
    sensitive: bool,
    /// Shift 按下但未松开：松开时决定是切换中英还是输入大写
    shift_pending: bool,
    /// 上一次「按下」是否被消费。松开事件跟着它走，避免「按下吞了、松开没吞」
    /// 导致客户端把字符又插进缓冲区。
    last_consumed: bool,
}

impl CtxInner {
    /// 两个引擎互斥，切换中英 / 放弃组字时一起清，避免残留串串味
    fn reset_all(&mut self) {
        self.engine.reset();
        self.en.reset();
        self.page = 0;
    }
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
                en: en::EnEngine::new(),
                page: 0,
                focused: false,
                sensitive: false,
                shift_pending: false,
                last_consumed: false,
            }),
        }
    }

    fn sync_variant(&self) {
        let v = self.global.variant();
        if let Ok(mut st) = self.inner.lock() {
            st.engine.set_variant(v);
        }
    }

    /// 锁内完成按键处理，返回要在锁外执行的动作。
    ///
    /// `release` 对应 DBus 上的第四个参数（协议名就叫 `isRelease`）：**true 是松开**。
    /// 早先当成 isPress 用，靠「松开事件跟随上次消费结果」蒙对了组字，
    /// 但新组字的第一个键必然漏给应用（此时 last_consumed 还是 false），
    /// 于是出现「n你好」这种首字母残留。
    fn handle_key(&self, keyval: u32, state: u32, release: bool) -> Outcome {
        let mut st = match self.inner.lock() {
            Ok(s) => s,
            Err(_) => return Outcome::ignore(),
        };

        // 松开事件：跟随上一次按下的消费结果
        if release {
            return Outcome {
                consumed: st.last_consumed,
                commit: None,
                ui: None,
            };
        }

        // 密码框 / PIN：任何字符都不拦，交回应用自己处理
        if st.sensitive {
            st.last_consumed = false;
            return Outcome::ignore();
        }

        let ctrl = (state & ST_CTRL) != 0;
        let shift = (state & ST_SHIFT) != 0;

        // Ctrl+Space 切换中英（先于其它判断，避免空格被当成上屏）
        if ctrl && keyval == KS_SPACE {
            st.shift_pending = false;
            st.reset_all();
            self.global.toggle();
            st.last_consumed = true;
            return Outcome {
                consumed: true,
                commit: None,
                ui: Some(Snapshot::empty()),
            };
        }

        // Ctrl+Shift+F 简繁切换（与 fcitx5 一致）
        if ctrl && shift && (keyval == 'f' as u32 || keyval == 'F' as u32) {
            let v = self.global.toggle_variant();
            st.engine.set_variant(v);
            st.page = 0;
            st.shift_pending = false;
            st.last_consumed = true;
            let snap = snapshot(&st);
            return Outcome {
                consumed: true,
                commit: None,
                ui: Some(snap),
            };
        }

        // 其它 Ctrl 组合一律透传：否则 Ctrl+C / Ctrl+V 会被当成字母吃掉
        if ctrl {
            st.shift_pending = false;
            st.last_consumed = false;
            return Outcome::ignore();
        }

        // Shift 单击切换中英：这里只记账，松开时再决定
        if keyval == KS_SHIFT_L || keyval == KS_SHIFT_R {
            st.shift_pending = true;
            st.last_consumed = false;
            return Outcome::ignore();
        }
        st.shift_pending = false;

        // 英文直通：与 fcitx5 的 keyboard-us 完全一致——不组字、不推 preedit、
        // 不出候选窗，按键原样交回客户端。前缀补全只在 XIME_EN_COMPLETION=1 时启用。
        if !self.global.chinese() && !self.global.en_completion() {
            if st.en.active() {
                st.reset_all();
            }
            st.last_consumed = false;
            return Outcome::ignore();
        }

        let out = if self.global.chinese() {
            key_zh(&mut st, keyval)
        } else {
            key_en(&mut st, keyval, state)
        };
        st.last_consumed = out.consumed;
        out
    }
}

/// 中文模式的按键处理：组字、候选、翻页
fn key_zh(st: &mut CtxInner, keyval: u32) -> Outcome {
    let composing = st.engine.active();

    if composing {
        match keyval {
            // 空格 / 回车：提交当前页首候选
            KS_SPACE | KS_RETURN => {
                let text =
                    pick(st.page * PAGE_SIZE, &st.engine.cands).unwrap_or_else(|| st.engine.input.clone());
                st.engine.reset();
                st.page = 0;
                return Outcome {
                    consumed: true,
                    commit: Some(text),
                    ui: Some(Snapshot::empty()),
                };
            }
            // Esc：放弃这次组字
            KS_ESCAPE => {
                st.engine.reset();
                st.page = 0;
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(Snapshot::empty()),
                };
            }
            KS_BACKSPACE => {
                st.engine.backspace();
                st.page = 0;
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(snapshot(st)),
                };
            }
            // 翻页：到边界就吞键不动作（与 fcitx5 一致，不会把 - 漏给应用）
            KS_MINUS | KS_PAGE_UP => {
                st.page = st.page.saturating_sub(1);
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(snapshot(st)),
                };
            }
            KS_EQUAL | KS_PAGE_DOWN => {
                if (st.page + 1) * PAGE_SIZE < st.engine.cands.len() {
                    st.page += 1;
                }
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(snapshot(st)),
                };
            }
            // 数字 1-9 选词：序号按当前页算，不是全局序号
            0x31..=0x39 => {
                let idx = st.page * PAGE_SIZE + (keyval - 0x31) as usize;
                if let Some(w) = pick(idx, &st.engine.cands) {
                    st.engine.reset();
                    st.page = 0;
                    return Outcome {
                        consumed: true,
                        commit: Some(w),
                        ui: Some(Snapshot::empty()),
                    };
                }
                // 超出候选范围：不吞键
                return Outcome::ignore();
            }
            _ => {}
        }
    }

    // 字母：进入或延续组字
    if let Some(c) = ascii_letter(keyval) {
        st.engine.push_char(c.to_ascii_lowercase());
        st.page = 0;
        return Outcome {
            consumed: true,
            commit: None,
            ui: Some(snapshot(st)),
        };
    }

    // 中文标点：与 Wayland 前端共用同一张映射表，组字中先把候选上屏。
    // 注意 `-` / `=` / PageUp / PageDown 在上面已作为翻页键处理掉了，
    // 这里不会被误判成标点。
    if let Some(p) = crate::punct_of(keyval) {
        let cand = if composing {
            pick(st.page * PAGE_SIZE, &st.engine.cands).unwrap_or_else(|| st.engine.input.clone())
        } else {
            String::new()
        };
        st.engine.reset();
        st.page = 0;
        return Outcome {
            consumed: true,
            commit: Some(format!("{cand}{p}")),
            ui: Some(Snapshot::empty()),
        };
    }

    // 其它键：不组字时透传，交给应用自己处理
    Outcome::ignore()
}

/// 英文模式的按键处理：前缀补全 + 候选窗，与中文共用同一套 UI 流程
fn key_en(st: &mut CtxInner, keyval: u32, state: u32) -> Outcome {
    if st.en.active() {
        match keyval {
            // 空格确认补全，但空格本身不吞——「hel␣」应得到「hello␣」
            KS_SPACE => {
                let text = st.en.cands.first().cloned().unwrap_or_else(|| st.en.raw());
                st.en.reset();
                st.page = 0;
                return Outcome {
                    consumed: false,
                    commit: Some(text),
                    ui: Some(Snapshot::empty()),
                };
            }
            // 回车上屏「我刚敲的字母」，回车本身也交给应用
            KS_RETURN => {
                let text = st.en.raw();
                st.en.reset();
                st.page = 0;
                return Outcome {
                    consumed: false,
                    commit: Some(text),
                    ui: Some(Snapshot::empty()),
                };
            }
            KS_ESCAPE => {
                st.en.reset();
                st.page = 0;
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(Snapshot::empty()),
                };
            }
            KS_BACKSPACE => {
                st.en.backspace(PAGE_SIZE);
                return Outcome {
                    consumed: true,
                    commit: None,
                    ui: Some(snapshot(st)),
                };
            }
            0x31..=0x39 => {
                let idx = (keyval - 0x31) as usize;
                if let Some(w) = pick(idx, &st.en.cands) {
                    st.en.reset();
                    st.page = 0;
                    return Outcome {
                        consumed: true,
                        commit: Some(w),
                        ui: Some(Snapshot::empty()),
                    };
                }
                return Outcome::ignore();
            }
            _ => {}
        }
    }

    if let Some(c) = char::from_u32(keyval).filter(|c| c.is_ascii_alphabetic()) {
        st.en.set_caps((state & ST_CAPSLOCK) != 0);
        st.en.push_char(c, PAGE_SIZE);
        st.page = 0;
        return Outcome {
            consumed: true,
            commit: None,
            ui: Some(snapshot(st)),
        };
    }

    Outcome::ignore()
}

fn pick(idx: usize, cands: &[String]) -> Option<String> {
    cands.get(idx).cloned()
}

/// 取一次 UI 快照。中文按页切片，英文本来就只够一页。
fn snapshot(st: &CtxInner) -> Snapshot {
    if st.engine.active() {
        let total = st.engine.cands.len();
        let start = (st.page * PAGE_SIZE).min(total);
        let end = (start + PAGE_SIZE).min(total);
        return Snapshot {
            preedit: st.engine.input.clone(),
            cands: st.engine.cands[start..end].to_vec(),
            sel: 0,
            has_prev: st.page > 0,
            has_next: end < total,
        };
    }
    if st.en.active() {
        return Snapshot {
            preedit: st.en.preedit().to_string(),
            cands: st.en.cands.clone(),
            sel: 0,
            has_prev: false,
            has_next: false,
        };
    }
    Snapshot::empty()
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

    #[zbus(name = "UpdateFormattedPreedit")]
    #[zbus(signal)]
    async fn update_formatted_preedit(
        emitter: &SignalEmitter<'_>,
        text: Vec<(String, i32)>,
        cursorpos: i32,
    ) -> zbus::Result<()>;

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
        // 只记状态，绝不在这里清组字/推空 UI：微信每敲一个键都会发一次
        // FocusIn，清一次就等于把用户正在打的拼音抹掉。
        if let Ok(mut st) = self.inner.lock() {
            st.focused = true;
        }
        self.global.log(&format!("ic{} FocusIn", self.id));
    }

    async fn focus_out(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.focused = false;
            st.reset_all();
            st.shift_pending = false;
        }
        self.global.log(&format!("ic{} FocusOut", self.id));
    }

    async fn reset(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.reset_all();
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
                st.reset_all();
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

    // 必须显式写名字：zbus 会把 destroy_ic 转成 DestroyIc，而 fcitx5 官方接口
    // 叫 DestroyIC（大写 IC）。拼错的那段时间里，客户端每次调用都收到
    // UnknownMethod，于是 /inputcontext/N 永不回收、无限累积。
    #[zbus(name = "DestroyIC")]
    async fn destroy_ic(&self, #[zbus(object_server)] server: &ObjectServer) {
        let path = format!("{IC_PREFIX}/{}", self.id);
        self.global.forget_context(&path);
        let server = server.clone();
        let global = self.global.clone();
        let id = self.id;
        // 不在这里直接 remove：此刻这个对象正在被派发（对象表上还挂着本次调用），
        // 同线程里取写锁会和派发路径打架。挪到独立线程回收——派发一结束它就拿到锁，
        // 效果等同于「调用返回后立刻销毁」，而 /inputcontext/N 不再无限累积。
        std::thread::spawn(move || {
            zbus::block_on(async move {
                match server.remove::<InputContext, _>(path.as_str()).await {
                    Ok(true) => global.log(&format!("ic{id} DestroyIC，已回收 {path}")),
                    Ok(false) => global.log(&format!("ic{id} DestroyIC，{path} 已不在对象表")),
                    Err(e) => global.log(&format!("ic{id} DestroyIC 回收 {path} 失败: {e}")),
                }
            });
        });
    }

    async fn process_key_event(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        keyval: u32,
        keycode: u32,
        state: u32,
        release: bool,
        _time: u32,
    ) -> bool {
        self.global.sync_shared();
        self.sync_variant();
        let out = self.handle_key(keyval, state, release);

        // 切换中英时先把残留的组字串取出来，随后原样提交 —— 与 fcitx5 一致，
        // 用户按 Shift 不该把已经敲的拼音丢掉。
        let mut leftover: Option<String> = None;
        let toggled = self.shift_released(keyval, release);
        if toggled {
            self.global.toggle();
            if let Ok(mut st) = self.inner.lock() {
                let s = if st.engine.active() {
                    st.engine.input.clone()
                } else if st.en.active() {
                    st.en.raw()
                } else {
                    String::new()
                };
                st.reset_all();
                if !s.is_empty() {
                    leftover = Some(s);
                }
            }
        }

        let has_commit =
            out.commit.as_deref().is_some_and(|s| !s.is_empty()) || leftover.is_some();

        // 顺序严格照 fcitx5：
        //   有提交时  UI(收候选窗) → CommitString → FormattedPreedit(空)
        //   只是组字  UI → FormattedPreedit
        // 把「清 preedit」提前到 Commit 之前，GTK 会按旧 preedit 定位光标，
        // 表现为插入文本后光标跳回行首。
        if let Some(snap) = out.ui {
            if has_commit {
                self.push_ui_client(&emitter, &snap).await;
            } else {
                self.push_ui(&emitter, snap).await;
            }
        } else if toggled {
            let empty = Snapshot::empty();
            if has_commit {
                self.push_ui_client(&emitter, &empty).await;
            } else {
                self.push_ui(&emitter, empty).await;
            }
        }

        for text in [out.commit, leftover].into_iter().flatten() {
            if !text.is_empty() {
                self.global.log(&format!("ic{} commit {:?}", self.id, text));
                let _ = Self::commit_string(&emitter, &text).await;
            }
        }

        if has_commit {
            self.push_formatted(&emitter, &Snapshot::empty()).await;
        }

        if self.global_verbose() {
            self.global.log(&format!(
                "ic{} 按键 keyval={keyval:#x} code={keycode} state={state:#x} 松开={release} -> 消费={}",
                self.id, out.consumed
            ));
        }
        out.consumed
    }

    /// 客户端点候选窗的翻页按钮时调用。信号只能从方法里发，所以这里注入
    /// emitter —— 空实现会让微信/GTK 的翻页箭头点了没反应。
    async fn prev_page(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.shift_page(&emitter, false).await;
    }

    async fn next_page(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.shift_page(&emitter, true).await;
    }

    async fn select_candidate(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        index: i32,
    ) {
        let text = {
            let mut st = match self.inner.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            // 客户端给的是「当前页内序号」，不是全局序号
            let zh_idx = st.page * PAGE_SIZE + index.max(0) as usize;
            let t = pick(zh_idx, &st.engine.cands).or_else(|| pick(index.max(0) as usize, &st.en.cands));
            if t.is_some() {
                st.reset_all();
            }
            t
        };
        if let Some(t) = text {
            self.global
                .log(&format!("ic{} 选词 {index} -> {:?}", self.id, t));
            self.push_ui(&emitter, Snapshot::empty()).await;
            let _ = Self::commit_string(&emitter, &t).await;
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
    fn shift_released(&self, keyval: u32, release: bool) -> bool {
        if !release || (keyval != KS_SHIFT_L && keyval != KS_SHIFT_R) {
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

    /// 翻页：中文候选最多 128 条而一屏只放 9 条，边界处不动（不循环）
    async fn shift_page(&self, emitter: &SignalEmitter<'_>, forward: bool) {
        let snap = {
            let mut st = match self.inner.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            let total = st.engine.cands.len();
            if forward {
                if (st.page + 1) * PAGE_SIZE < total {
                    st.page += 1;
                }
            } else {
                st.page = st.page.saturating_sub(1);
            }
            snapshot(&st)
        };
        self.push_ui(emitter, snap).await;
    }

    /// 组字中的常规刷新：候选窗 + 输入框内预编辑，两条一起发
    async fn push_ui(&self, emitter: &SignalEmitter<'_>, snap: Snapshot) {
        self.push_ui_client(emitter, &snap).await;
        self.push_formatted(emitter, &snap).await;
    }

    /// 只发 UpdateClientSideUI（候选窗、辅助行、像素级定位）
    async fn push_ui_client(&self, emitter: &SignalEmitter<'_>, snap: &Snapshot) {
        let preedit = if snap.preedit.is_empty() {
            Vec::new()
        } else {
            vec![(snap.preedit.clone(), 1)] // 1 = Underline
        };
        // 与 fcitx5 对齐：没有组字串时 cursorpos 给 -1，hint 给 0
        let cursor = if snap.preedit.is_empty() {
            -1
        } else {
            snap.preedit.chars().count() as i32
        };
        let cands: Vec<(String, String)> = snap
            .cands
            .iter()
            .take(PAGE_SIZE)
            .map(|c| (c.clone(), String::new()))
            .collect();
        let hint = if cands.is_empty() { 0 } else { LAYOUT_HORIZONTAL };
        let _ = Self::update_client_side_ui(
            emitter,
            preedit,
            cursor,
            Vec::new(),
            Vec::new(),
            cands,
            snap.sel as i32,
            hint,
            snap.has_prev,
            snap.has_next,
        )
        .await;
    }

    /// 只发 UpdateFormattedPreedit（输入框内的下划线预编辑）
    async fn push_formatted(&self, emitter: &SignalEmitter<'_>, snap: &Snapshot) {
        let seg = if snap.preedit.is_empty() {
            Vec::new()
        } else {
            vec![(snap.preedit.clone(), 1)]
        };
        // cursorpos 是**预编辑内部**的光标偏移，必须给实际长度：
        // 写 0 会让客户端把光标摆在拼音最前面，看起来就是「光标跳到行首」，
        // 之后敲的字也插到拼音前面去。空串时给 0（此时该字段无意义）。
        let cursor = snap.preedit.chars().count() as i32;
        let _ = Self::update_formatted_preedit(emitter, seg, cursor).await;
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
        #[zbus(header)] header: zbus::message::Header<'_>,
        kvs: Vec<(String, String)>,
    ) -> zbus::fdo::Result<(zbus::OwnedObjectPath, Vec<u8>)> {
        let id = self.global.alloc_id();
        let path = format!("{IC_PREFIX}/{id}");
        let ctx = InputContext::new(id, self.global.clone(), self.variant);
        server
            .at(path.as_str(), ctx)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("注册 {path} 失败: {e}")))?;

        // 记下调用方（客户端）的唯一名，它断开时据此销毁这个 IC
        if let Some(owner) = header.sender() {
            self.global
                .register_context(owner.as_str().to_string(), path.clone());
        }

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
// Controller1：状态栏 / 外部工具读输入法状态
// ---------------------------------------------------------------------------
//
// 真 fcitx5 在 /controller 上开了 org.fcitx.Fcitx.Controller1。omarchy 的状态栏
// （indicator.py）每 500ms 读一次 CurrentInputMethodGroup，读不到就整个输入法
// 指示器失效；某些应用也靠它判断「现在是不是中文」。xime 之前完全没有这个
// 对象，DBus 上一片 Unknown object，这就是「别的应用输入法像是坏了」的一部分。

/// fcitx5 的输入法条目结构：a(ssssssb)
type ImEntry = (
    String, // 唯一名
    String, // 显示名
    String, // 图标
    String, // 文字图标
    String, // 标签（状态栏直接显示这个）
    String, // 语言
    bool,   // 是否可配置
);

const GROUP: &str = "Default";

fn entry_zh() -> ImEntry {
    (
        IM_ZH.to_string(),
        "拼音".to_string(),
        String::new(),
        String::new(),
        "中".to_string(),
        "zh_CN".to_string(),
        false,
    )
}

fn entry_en() -> ImEntry {
    (
        IM_EN.to_string(),
        "English".to_string(),
        String::new(),
        String::new(),
        "英".to_string(),
        "en_US".to_string(),
        false,
    )
}

struct Controller {
    global: Arc<Global>,
}

#[interface(name = "org.fcitx.Fcitx.Controller1")]
impl Controller {
    fn current_input_method(&self) -> String {
        self.global.im_name().to_string()
    }

    fn current_input_method_group(&self) -> String {
        GROUP.to_string()
    }

    /// 这里是 **6 个独立出参**（不是单个结构体）：omarchy 的状态栏直接按
    /// `info[4]` 取标签、`info[5]` 取语言，多包一层会 IndexError 把指示器打挂。
    fn current_input_method_info(&self) -> (String, String, String, String, String, String) {
        let e = if self.global.chinese() {
            entry_zh()
        } else {
            entry_en()
        };
        (e.0, e.1, e.2, e.3, e.4, e.5)
    }

    fn available_input_methods(&self) -> Vec<ImEntry> {
        vec![entry_en(), entry_zh()]
    }

    fn input_method_groups(&self) -> Vec<String> {
        vec![GROUP.to_string()]
    }

    fn input_method_group_info(&self, _group: &str) -> (String, Vec<ImEntry>) {
        (String::new(), vec![entry_en(), entry_zh()])
    }

    #[zbus(name = "SetCurrentIM")]
    fn set_current_im(&self, im: &str) -> bool {
        self.global.set_im(im)
    }

    fn set_current_input_method_group(&self, _group: &str) -> bool {
        true
    }

    fn activate(&self) {
        self.global.set_im(IM_ZH);
    }

    fn deactivate(&self) {
        self.global.set_im(IM_EN);
    }

    fn toggle(&self) {
        self.global.toggle();
    }

    fn refresh(&self) {}

    fn reload_config(&self) {}

    fn configure(&self) {}

    async fn restart(&self) {}

    async fn exit(&self) {}

    #[zbus(name = "CurrentUI")]
    fn current_ui(&self) -> String {
        "xime".to_string()
    }

    fn can_restart(&self) -> bool {
        false
    }

    fn check_update(&self) -> bool {
        false
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
            .serve_at(CONTROLLER_PATH, Controller {
                global: global.clone(),
            })
            .build()
            .await
            .map_err(|e| format!("连上总线并申请 {SERVICE_MAIN} 失败（可能有 fcitx5 在跑）: {e}"))?;

        // 老名字尽力而为，失败不影响主名
        if let Err(e) = conn.request_name(SERVICE_LEGACY).await {
            eprintln!("xime-dbus: 附加服务名 {SERVICE_LEGACY} 未申请到（{e}），通常无碍");
        }

        eprintln!("xime-dbus: 已接管 {SERVICE_MAIN}，等待客户端连接");

        // 监听客户端断开：NameOwnerChanged 的 new_owner 变空 = 该唯一名被总线回收，
        // 据此销毁它创建的 input context，避免 busctl tree 里残留 /inputcontext/N。
        let dbus = zbus::fdo::DBusProxy::new(&conn)
            .await
            .map_err(|e| format!("建 DBusProxy 失败: {e}"))?;
        let mut stream = dbus
            .receive_name_owner_changed()
            .await
            .map_err(|e| format!("监听 NameOwnerChanged 失败: {e}"))?;
        let server = conn.object_server().clone();
        while let Some(sig) = stream.next().await {
            let args = match sig.args() {
                Ok(a) => a,
                Err(_) => continue,
            };
            if args.new_owner().is_some() {
                continue;
            }
            let owner = args.name().as_str().to_string();
            for p in global.take_contexts(&owner) {
                let _ = server.remove::<InputContext, _>(p.as_str()).await;
                global.log(&format!("客户端 {owner} 断开，回收 {p}"));
            }
        }
        Ok::<(), String>(())
    })
}
