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

use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use zbus::interface;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::Connection;

use crate::en;
use crate::engine::Engine;
use crate::session::{
    CtxInner, Global, Outcome, Snapshot, IM_EN, IM_ZH, KS_SHIFT_L, KS_SHIFT_R, LAYOUT_HORIZONTAL,
    PAGE_SIZE, pick, snapshot,
};
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
        crate::session::handle_key(&self.global, &mut st, keyval, state, release)
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
    let global = Global::new(variant, verbose, "xime-dbus");
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
