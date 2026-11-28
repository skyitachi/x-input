//! IBus 前端：让 xime 在 GNOME 上工作。
//!
//! 背景：Mutter 至今（51）不实现 `zwp_input_method_v2`，wayland.app 的合成器
//! 支持表里它就是 `x`；GNOME 的输入法只有 IBus 一条路。更要紧的是 GTK4 应用
//! 和 Firefox / Chromium / Electron 的 Wayland 模式**完全无视 `GTK_IM_MODULE`**，
//! 只走 `text-input-v3` → mutter → IBus，所以 `--dbus` 前端在 GNOME 上覆盖不到
//! 它们。要补齐，只能让 xime 自己变成一个 IBus 引擎。
//!
//! 架构（与 fcitx5 DBus 前端并列的第三个前端，三者共用 `session` 状态机）：
//!
//! - 进程连 **ibus-daemon 自己的总线**，申请 `org.freedesktop.IBus.Xime`
//! - 在 `/org/freedesktop/IBus/Factory` 暴露 `org.freedesktop.IBus.Factory`
//! - ibus-daemon 读到 component XML 后按需 exec 本进程，再向这个 Factory 要引擎
//! - 引擎对象落在 `/org/freedesktop/IBus/Engine/<n>`，实现 `org.freedesktop.IBus.Engine`
//! - 候选窗交给 IBus / gnome-shell 画：GNOME 不支持 `wlr-layer-shell`，
//!   自绘浮窗在 GNOME 上没有可靠落点，这也是这条路线唯一的功能让步
//!
//! **最容易踩的坑**：ibus-daemon 默认**不开在会话总线上**，而是自建一个私有
//! D-Bus 服务（例如 `unix:path=/run/user/1000/ibus/dbus-XXXX`），把地址写进
//! `~/.config/ibus/bus/<machine-id>-unix-<display>`，文件里是 `IBUS_ADDRESS=...`。
//! 客户端和引擎都必须按 libibus 的规则去解析这个地址；直接连
//! `DBUS_SESSION_BUS_ADDRESS` 会连到另一条总线，daemon 永远看不到引擎，
//! 表现为 `ibus engine xime` 卡 15 秒后报 `Set global engine failed:
//! Timeout was reached`，而引擎进程自己还活得好好的。
//!
//! 协议依据（ibus 1.5.34 源码，逐字段核对）：
//! - `src/ibusengine.c` / `src/ibusfactory.c` 的 introspection XML
//! - `src/ibusserializable.c`：对象序列化以 `s`(GType 名) 打头，后接 `a{sv}`
//! - `src/ibustext.c`：`(s a{sv} s v)` —— 文本 + 属性表
//! - `src/ibuslookuptable.c`：`(s a{sv} u u b b i av av)` —— 候选 + 标签

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::Variant as ZVariant;
use zbus::{Connection, ObjectPath, OwnedObjectPath};

use crate::en;
use crate::engine::Engine;
use crate::session::{CtxInner, Global, Outcome, PAGE_SIZE, handle_key, snapshot};
use crate::zh::Variant;

/// 组件名 = 总线名。ibus-daemon 按 component 的 `<name>` 推导引擎进程的
/// 总线名，所以这里必须与 component XML 的 `<name>` 完全一致。
const BUS_NAME: &str = "org.freedesktop.IBus.Xime";

const FACTORY_PATH: &str = "/org/freedesktop/IBus/Factory";
const ENGINE_PATH_PREFIX: &str = "/org/freedesktop/IBus/Engine";

/// CreateEngine 的参数：component XML 里 `<engines><engine><name>` 的值
const ENGINE_NAME: &str = "xime";

/// 允许用环境变量临时顶替总线名。只在排查「ibus-daemon 认不到引擎」时用：
/// 把 xime 挂到别的 component 名下，就能判断问题出在握手协议还是 component 定义。
fn bus_name() -> String {
    std::env::var("XIME_IBUS_BUS_NAME").unwrap_or_else(|_| BUS_NAME.to_string())
}

/// X11 modifier mask 里的「按键释放」位（IBus 与 fcitx 用同一套）
const RELEASE_MASK: u32 = 1 << 30;

// ---------------------------------------------------------------------------
// IBus 可序列化对象的构造
//
// 这几个 variant 必须逐字节与 ibus 的 serialize 顺序一致：客户端（gtk/qt 的
// ibus 模块、gnome-shell 的 panel）是按位置反序列化的，多一个少一个字段都会
// 让候选窗整个不出来。
// ---------------------------------------------------------------------------

/// `IBusAttrList`：`(s "IBusAttrList", a{sv} {}, av [])`
fn attr_list() -> ZVariant<'static> {
    ZVariant::new((
        "IBusAttrList",
        HashMap::<String, ZVariant<'static>>::new(),
        Vec::<ZVariant<'static>>::new(),
    ))
}

/// `IBusText`：`(s "IBusText", a{sv} {}, s 文本, v 属性表)`
fn ibus_text(text: &str) -> ZVariant<'static> {
    ZVariant::new((
        "IBusText",
        HashMap::<String, ZVariant<'static>>::new(),
        text.to_string(),
        attr_list(),
    ))
}

/// `IBusLookupTable`：
/// `(s, a{sv} {}, u 每页条数, u 光标位置, b 光标可见, b 循环, i 排列方向, av 候选, av 标签)`
fn lookup_table(cands: &[String], cursor: u32) -> ZVariant<'static> {
    let candidates: Vec<ZVariant<'static>> = cands.iter().map(|c| ibus_text(c)).collect();
    // 标签留空：gnome-shell 会用数字自动编号，和「数字键 1-9 选词」对得上
    let labels: Vec<ZVariant<'static>> = Vec::new();
    ZVariant::new((
        "IBusLookupTable",
        HashMap::<String, ZVariant<'static>>::new(),
        PAGE_SIZE as u32,
        cursor,
        true,
        true,
        crate::session::LAYOUT_HORIZONTAL,
        candidates,
        labels,
    ))
}

// ---------------------------------------------------------------------------
// 引擎对象
// ---------------------------------------------------------------------------

struct IbusEngine {
    /// 自己的对象路径，发信号时要带
    path: String,
    global: Arc<Global>,
    inner: Mutex<CtxInner>,
}

impl IbusEngine {
    fn new(path: String, global: Arc<Global>, variant: Variant) -> IbusEngine {
        let mut engine = Engine::new();
        engine.set_variant(variant);
        IbusEngine {
            path,
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

    /// 把一次按键的产物翻译成 IBus 信号。
    ///
    /// 顺序有讲究：先提交、再更新预编辑、最后更新候选表。反过来的话，
    /// gnome-shell 会先看到候选表变空、再看到提交，候选窗会闪一下。
    async fn publish(&self, conn: &Connection, out: &Outcome) -> zbus::Result<()> {
        let emitter = match SignalEmitter::new(conn, self.path.as_str()) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };

        if let Some(text) = &out.commit {
            self.global.log(&format!("CommitText({text})"));
            IbusEngine::commit_text(&emitter, ibus_text(text)).await?;
        }

        if let Some(ui) = &out.ui {
            let show_preedit = !ui.preedit.is_empty();
            IbusEngine::update_preedit_text(
                &emitter,
                ibus_text(&ui.preedit),
                ui.preedit.chars().count() as u32,
                show_preedit,
                0,
            )
            .await?;

            let show_table = !ui.cands.is_empty();
            IbusEngine::update_lookup_table(
                &emitter,
                lookup_table(&ui.cands, ui.sel as u32),
                show_table,
            )
            .await?;
        }

        Ok(())
    }

    /// 翻页：IBus 既可能在 ProcessKeyEvent 里送 `-`/`=`，也可能直接调
    /// PageUp/PageDown 方法（面板上的箭头就会走这条），两边都要认。
    async fn flip_page(&self, conn: &Connection, forward: bool) -> zbus::Result<()> {
        let out = {
            let mut st = match self.inner.lock() {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };
            let total = st.engine.cands.len();
            if forward {
                if (st.page + 1) * PAGE_SIZE < total {
                    st.page += 1;
                }
            } else {
                st.page = st.page.saturating_sub(1);
            }
            Outcome {
                consumed: true,
                commit: None,
                ui: Some(snapshot(&st)),
            }
        };
        self.publish(conn, &out).await
    }
}

#[zbus::interface(name = "org.freedesktop.IBus.Engine")]
impl IbusEngine {
    // ---- 信号 ----

    /// 提交文本。参数是 `v`（IBusText），不是裸字符串。
    #[zbus(signal)]
    async fn commit_text(emitter: &SignalEmitter<'_>, text: ZVariant<'_>) -> zbus::Result<()>;

    /// 预编辑：`(v 文本, u 光标, b 是否显示, u 模式)`
    #[zbus(signal)]
    async fn update_preedit_text(
        emitter: &SignalEmitter<'_>,
        text: ZVariant<'_>,
        cursor_pos: u32,
        visible: bool,
        mode: u32,
    ) -> zbus::Result<()>;

    /// 候选表：`(v 表, b 是否显示)`
    #[zbus(signal)]
    async fn update_lookup_table(
        emitter: &SignalEmitter<'_>,
        table: ZVariant<'_>,
        visible: bool,
    ) -> zbus::Result<()>;

    // ---- 方法 ----

    /// 核心入口。返回 true 表示这个键被输入法吃掉，不再交给应用。
    async fn process_key_event(
        &self,
        keyval: u32,
        _keycode: u32,
        state: u32,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<bool> {
        let release = (state & RELEASE_MASK) != 0;
        self.global.log(&format!(
            "ProcessKeyEvent keyval=0x{keyval:x} state=0x{state:x} release={release}"
        ));
        let out = {
            let mut st = match self.inner.lock() {
                Ok(s) => s,
                Err(_) => return Ok(false),
            };
            handle_key(&self.global, &mut st, keyval, state, release)
        };
        if let Err(e) = self.publish(conn, &out).await {
            eprintln!("xime-ibus: 发送信号失败: {e}");
        }
        Ok(out.consumed)
    }

    async fn focus_in(&self) -> zbus::fdo::Result<()> {
        self.global.log("FocusIn");
        if let Ok(mut st) = self.inner.lock() {
            st.focused = true;
        }
        self.global.sync_shared();
        Ok(())
    }

    async fn focus_out(&self) -> zbus::fdo::Result<()> {
        self.global.log("FocusOut");
        if let Ok(mut st) = self.inner.lock() {
            st.focused = false;
            st.reset_all();
        }
        Ok(())
    }

    /// ibus-daemon 在切换引擎 / 失焦时会调，必须清干净，
    /// 否则下次聚焦还挂着上次的拼音串
    async fn reset(&self) -> zbus::fdo::Result<()> {
        self.global.log("Reset");
        if let Ok(mut st) = self.inner.lock() {
            st.reset_all();
        }
        Ok(())
    }

    async fn enable(&self) -> zbus::fdo::Result<()> {
        self.global.log("Enable");
        Ok(())
    }

    async fn disable(&self) -> zbus::fdo::Result<()> {
        self.global.log("Disable");
        if let Ok(mut st) = self.inner.lock() {
            st.reset_all();
        }
        Ok(())
    }

    /// 能力位。IBus 没有「密码框」位，密码框靠 ContentType 判定；
    /// 这里先只记录，全键透传由 `CtxInner::sensitive` 负责。
    async fn set_capabilities(&self, caps: u32) -> zbus::fdo::Result<()> {
        self.global.log(&format!("SetCapabilities caps=0x{caps:x}"));
        Ok(())
    }

    /// 光标位置：GNOME 下候选窗由 gnome-shell 定位，这个信息用不上，
    /// 但必须应答，否则 daemon 会认为引擎卡死
    async fn set_cursor_location(
        &self,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> zbus::fdo::Result<()> {
        self.global.log("SetCursorLocation");
        Ok(())
    }

    async fn page_up(&self, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        let _ = self.flip_page(conn, false).await;
        Ok(())
    }

    async fn page_down(&self, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        let _ = self.flip_page(conn, true).await;
        Ok(())
    }

    async fn cursor_up(&self, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        let _ = self.flip_page(conn, false).await;
        Ok(())
    }

    async fn cursor_down(&self, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        let _ = self.flip_page(conn, true).await;
        Ok(())
    }

    async fn property_activate(&self, _name: &str, _state: u32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn property_show(&self, _name: &str) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn property_hide(&self, _name: &str) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn candidate_clicked(
        &self,
        _index: u32,
        _button: u32,
        _state: u32,
    ) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn set_surrounding_text(
        &self,
        _text: ZVariant<'_>,
        _cursor_pos: u32,
        _anchor_pos: u32,
    ) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 工厂对象
// ---------------------------------------------------------------------------

struct IbusFactory {
    global: Arc<Global>,
    next_id: Mutex<u32>,
}

#[zbus::interface(name = "org.freedesktop.IBus.Factory")]
impl IbusFactory {
    /// ibus-daemon 每需要一个输入上下文就来要一个引擎对象。
    /// 返回的是对象路径，不是对象本身。
    async fn create_engine(
        &self,
        name: &str,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.global
            .log(&format!("Factory.CreateEngine(name=\"{name}\")"));
        if name != ENGINE_NAME {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "xime 只提供 {ENGINE_NAME} 引擎，收到 {name}"
            )));
        }

        let id = {
            let mut n = match self.next_id.lock() {
                Ok(n) => n,
                Err(_) => return Err(zbus::fdo::Error::Failed("id 锁损坏".into())),
            };
            *n += 1;
            *n
        };
        let path = format!("{ENGINE_PATH_PREFIX}/{id}");

        let variant = self.global.variant();
        let engine = IbusEngine::new(path.clone(), self.global.clone(), variant);
        if let Err(e) = server.at(path.as_str(), engine).await {
            return Err(zbus::fdo::Error::Failed(format!("注册引擎对象失败: {e}")));
        }
        self.global.log(&format!("创建引擎对象 {path}"));

        ObjectPath::try_from(path)
            .map(OwnedObjectPath::from)
            .map_err(|e| zbus::fdo::Error::Failed(format!("对象路径非法: {e}")))
    }
}

// ---------------------------------------------------------------------------
// ibus-daemon 地址解析
//
// 顺序与 libibus 的 ibus_get_address() 一致：
//   1. $IBUS_ADDRESS
//   2. $IBUS_ADDRESS_FILE 指向的文件
//   3. ~/.config/ibus/bus/<machine-id>-unix-<display>
//   4. 兜底：扫上面那个目录，取 daemon 还活着且最新的一个
// ---------------------------------------------------------------------------

/// `~/.config/ibus/bus`（遵循 XDG_CONFIG_HOME）
fn bus_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("ibus").join("bus")
}

/// 机器标识：libibus 优先读 `/var/lib/dbus/machine-id`，再退到 `/etc/machine-id`
fn machine_id() -> Option<String> {
    for p in ["/var/lib/dbus/machine-id", "/etc/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// 地址文件后缀里的 display 部分：
/// - X11：`DISPLAY=:0` → `0`，文件名为 `<machine-id>-unix-0`
/// - Wayland：`WAYLAND_DISPLAY=wayland-0` → `wayland-0`，文件名为 `<machine-id>-unix-wayland-0`
fn display_suffix() -> Option<String> {
    if let Ok(d) = std::env::var("DISPLAY") {
        let d = d.trim();
        if !d.is_empty() {
            return Some(d.trim_start_matches(':').to_string());
        }
    }
    if let Ok(w) = std::env::var("WAYLAND_DISPLAY") {
        let w = w.trim();
        if !w.is_empty() {
            return Some(w.to_string());
        }
    }
    // 两个都没设时 libibus 也按 wayland-0 算
    Some("wayland-0".to_string())
}

/// 从地址文件里取 `IBUS_ADDRESS=` 那一行
fn read_address_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("IBUS_ADDRESS=") {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// 兜底：目录里挑一个 daemon 还在跑、且修改时间最新的地址文件。
/// 这样即使 display 后缀算错（多屏、嵌套合成器、远程会话）也能连上。
fn newest_live_address(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(SystemTime, String)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let alive = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("IBUS_DAEMON_PID="))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|pid| Path::new(&format!("/proc/{pid}")).exists())
            .unwrap_or(false);
        if !alive {
            continue;
        }
        let addr = match read_address_file(&path) {
            Some(a) => a,
            None => continue,
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, addr));
        }
    }
    best.map(|(_, a)| a)
}

fn resolve_address() -> Result<String, String> {
    if let Ok(a) = std::env::var("IBUS_ADDRESS") {
        let a = a.trim().to_string();
        if !a.is_empty() {
            return Ok(a);
        }
    }

    let dir = bus_dir();

    if let Ok(f) = std::env::var("IBUS_ADDRESS_FILE") {
        if let Some(a) = read_address_file(Path::new(f.trim())) {
            return Ok(a);
        }
    }

    if let Some(mid) = machine_id() {
        if let Some(suffix) = display_suffix() {
            let p = dir.join(format!("{mid}-unix-{suffix}"));
            if let Some(a) = read_address_file(&p) {
                return Ok(a);
            }
        }
    }

    if let Some(a) = newest_live_address(&dir) {
        return Ok(a);
    }

    Err(format!(
        "找不到 ibus-daemon 地址（已查 $IBUS_ADDRESS、{}）",
        dir.display()
    ))
}

/// 解析地址，并给 daemon 一点启动时间。
/// 手动执行 `xime --ibus`（daemon 还没起来）时也能等到地址文件出现。
fn wait_for_address(global: &Arc<Global>) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..25 {
        match resolve_address() {
            Ok(a) => return Ok(a),
            Err(e) => last = e,
        }
        if attempt == 0 {
            global.log(&format!("{last}，最多重试 5 秒"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(last)
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 以 IBus 引擎身份运行。需要 ibus-daemon 在跑（GNOME 默认就有）。
pub fn run(variant: Variant, verbose: bool) -> Result<(), String> {
    let global = Global::new(variant, verbose, "xime-ibus");
    let factory = IbusFactory {
        global: global.clone(),
        next_id: Mutex::new(0),
    };
    let name = bus_name();

    // 地址解析是纯文件 IO，放在进 async 之前，省得在主循环里睡
    let address = wait_for_address(&global)?;

    zbus::block_on(async move {
        let parsed = zbus::Address::from_str(&address)
            .map_err(|e| format!("IBus 地址无法解析（{address}）: {e}"))?;
        let conn = zbus::connection::Builder::address(parsed)
            .name(name.clone())
            .serve_at(FACTORY_PATH, factory)
            .build()
            .await
            .map_err(|e| {
                format!("连接 {address} 或以 {name} 注册失败（可能有另一个 xime --ibus 在跑）: {e}")
            })?;

        eprintln!(
            "xime-ibus: 已接管 {name}（总线 {address}），Factory 就绪于 {FACTORY_PATH}，等待 ibus-daemon 调用 CreateEngine"
        );
        let _ = &conn;

        // 常驻：所有工作都在 D-Bus 回调里，主线程无事可做
        futures_lite::future::pending::<()>().await;
        Ok::<(), String>(())
    })
}
