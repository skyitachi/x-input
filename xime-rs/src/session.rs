//! 三个前端共用的输入会话状态机。
//!
//! Wayland 前端、fcitx5 DBus 前端、IBus 前端共用这一份
//! 「按键 → 引擎状态迁移 → 要不要吞键 / 提交什么 / 推什么 UI」逻辑。
//! 这里不出现任何 Wayland 或 D-Bus 类型，协议层留在各自的模块里。
//!
//! 抽出来的原因：GNOME（Mutter）不实现 `zwp_input_method_v2`，xime 要靠
//! IBus 前端才能在 GNOME 上工作，而 IBus 与 fcitx5 DBus 的按键语义一致
//! （都是 X11 keysym + modifier mask），没有理由维护两套组字/选词行为。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::en;
use crate::engine::Engine;
use crate::zh::Variant;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 状态栏（omarchy 的 indicator）认这两个名字：keyboard-us = 英文直通，
/// pinyin = 中文组字。
pub(crate) const IM_EN: &str = "keyboard-us";
pub(crate) const IM_ZH: &str = "pinyin";

/// 修饰键位（与 fcitx5-qt 的 flags.h、X11 modifier mask 一致）
pub(crate) const ST_SHIFT: u32 = 1 << 0;
pub(crate) const ST_CAPSLOCK: u32 = 1 << 1;
pub(crate) const ST_CTRL: u32 = 1 << 2;
#[allow(dead_code)]
pub(crate) const ST_ALT: u32 = 1 << 3;

/// X11 keysym
pub(crate) const KS_SPACE: u32 = 0x20;
pub(crate) const KS_BACKSPACE: u32 = 0xFF08;
pub(crate) const KS_RETURN: u32 = 0xFF0D;
pub(crate) const KS_ESCAPE: u32 = 0xFF1B;
pub(crate) const KS_SHIFT_L: u32 = 0xFFE1;
pub(crate) const KS_SHIFT_R: u32 = 0xFFE2;
/// 翻页键：`-` / `=` 与 PageUp / PageDown（fcitx5 拼音的默认绑定）
pub(crate) const KS_MINUS: u32 = 0x2D;
pub(crate) const KS_EQUAL: u32 = 0x3D;
pub(crate) const KS_PAGE_UP: u32 = 0xFF55;
pub(crate) const KS_PAGE_DOWN: u32 = 0xFF56;

/// 候选窗排版：2 = 水平排列（Phase 0.5 实测微信能正确渲染）
pub(crate) const LAYOUT_HORIZONTAL: i32 = 2;

/// 每页候选数：必须正好 9，数字键 1-9 才与之一一对应
pub(crate) const PAGE_SIZE: usize = 9;

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

pub(crate) struct Global {
    pub(crate) inner: Mutex<GlobalInner>,
}

pub(crate) struct GlobalInner {
    pub(crate) chinese: bool,
    /// 英文态是否启用「前缀补全 + 候选窗」。
    /// 默认关闭：fcitx5 的 keyboard-us 不出候选窗，xime 也照此处理（纯透传）。
    pub(crate) en_completion: bool,
    pub(crate) variant: Variant,
    pub(crate) verbose: bool,
    /// 日志前缀，让三个前端的日志能一眼区分
    pub(crate) tag: &'static str,
    pub(crate) next_id: u32,
    /// DBus 客户端唯一名 → 它创建的 input context 路径列表。
    /// 客户端断开（唯一名被总线回收）时据此销毁它的全部 IC，避免 busctl tree 残留。
    pub(crate) contexts: HashMap<String, Vec<String>>,
}

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
    pub(crate) fn new(variant: Variant, verbose: bool, tag: &'static str) -> Arc<Global> {
        Arc::new(Global {
            inner: Mutex::new(GlobalInner {
                // 多前端共享中英状态（见 im_state 模块）：文件里有就以文件为准
                chinese: crate::im_state::read().unwrap_or(true),
                en_completion: en_completion_enabled(),
                variant,
                verbose,
                tag,
                next_id: 0,
                contexts: HashMap::new(),
            }),
        })
    }

    pub(crate) fn log(&self, msg: &str) {
        let (on, tag) = match self.inner.lock() {
            Ok(g) => (g.verbose, g.tag),
            Err(_) => (false, "xime"),
        };
        if on {
            eprintln!("{tag}: {msg}");
        }
    }

    pub(crate) fn chinese(&self) -> bool {
        self.inner.lock().map(|g| g.chinese).unwrap_or(true)
    }

    pub(crate) fn en_completion(&self) -> bool {
        self.inner.lock().map(|g| g.en_completion).unwrap_or(false)
    }

    pub(crate) fn variant(&self) -> Variant {
        self.inner.lock().map(|g| g.variant).unwrap_or(Variant::Hans)
    }

    /// 简繁切换（Ctrl+Shift+F），返回切换后的形态，调用方据此就地重算
    pub(crate) fn toggle_variant(&self) -> Variant {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Variant::Hans,
        };
        g.variant = g.variant.toggled();
        let v = g.variant;
        if g.verbose {
            let tag = g.tag;
            eprintln!("{tag}: 输出形态切换为{}", v.label());
        }
        v
    }

    /// 按输入法名设置中英状态，供状态栏 / 外部工具调用
    pub(crate) fn set_im(&self, name: &str) -> bool {
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
            let tag = g.tag;
            eprintln!("{tag}: 切到 {name}（{}）", if zh { "中文" } else { "英文" });
        }
        true
    }

    pub(crate) fn im_name(&self) -> &'static str {
        if self.chinese() {
            IM_ZH
        } else {
            IM_EN
        }
    }

    pub(crate) fn toggle(&self) -> bool {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return true,
        };
        g.chinese = !g.chinese;
        let now = g.chinese;
        // 同步给其它前端，避免各自漂移
        crate::im_state::write(now);
        if g.verbose {
            let tag = g.tag;
            eprintln!("{tag}: 切换为{}", if now { "中文" } else { "英文" });
        }
        now
    }

    pub(crate) fn alloc_id(&self) -> u32 {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return 1,
        };
        g.next_id += 1;
        g.next_id
    }

    /// 把其它前端刚改过的中英状态同步过来。
    ///
    /// 几个进程分别服务不同应用：在终端（Wayland）里按 Shift 切英文，Qt / GNOME
    /// 这边必须跟着变，否则用户会遇到「同一个键在两边表现不同」且毫无提示。
    pub(crate) fn sync_shared(&self) {
        let Some(zh) = crate::im_state::read() else {
            return;
        };
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if g.chinese == zh {
            return;
        }
        g.chinese = zh;
        if g.verbose {
            let tag = g.tag;
            eprintln!("{tag}: 同步为{}（另一前端切换）", if zh { "中文" } else { "英文" });
        }
    }

    /// 记录某个客户端（owner 唯一名）名下新注册的 input context 路径。
    pub(crate) fn register_context(&self, owner: String, path: String) {
        if let Ok(mut g) = self.inner.lock() {
            g.contexts.entry(owner).or_default().push(path);
        }
    }

    /// 取走某个客户端名下的全部 IC 路径（并清空该条目），用于断开时回收。
    pub(crate) fn take_contexts(&self, owner: &str) -> Vec<String> {
        self.inner
            .lock()
            .map(|mut g| g.contexts.remove(owner).unwrap_or_default())
            .unwrap_or_default()
    }

    /// 从所有客户端的清单里摘掉一个已销毁的 IC 路径。
    ///
    /// 客户端自己发 DestroyIC 时用：路径要立刻从清单里移除，否则它断开连接时
    /// 还会对这个已不存在的对象再 remove 一次，日志里出现一串无害但误导的报错。
    pub(crate) fn forget_context(&self, path: &str) {
        if let Ok(mut g) = self.inner.lock() {
            for v in g.contexts.values_mut() {
                v.retain(|p| p != path);
            }
            g.contexts.retain(|_, v| !v.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// 一次按键的产物
// ---------------------------------------------------------------------------

/// 一次按键处理后需要对外做的事，全部在锁外执行（信号发送是 async）
pub(crate) struct Outcome {
    pub(crate) consumed: bool,
    pub(crate) commit: Option<String>,
    /// Some 表示要推送一次 UI（组字串 / 候选）
    pub(crate) ui: Option<Snapshot>,
}

pub(crate) struct Snapshot {
    pub(crate) preedit: String,
    /// 当前页的候选（最多 PAGE_SIZE 条）
    pub(crate) cands: Vec<String>,
    pub(crate) sel: usize,
    pub(crate) has_prev: bool,
    pub(crate) has_next: bool,
}

impl Snapshot {
    pub(crate) fn empty() -> Snapshot {
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
    pub(crate) fn ignore() -> Outcome {
        Outcome {
            consumed: false,
            commit: None,
            ui: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 会话内部状态
// ---------------------------------------------------------------------------

pub(crate) struct CtxInner {
    /// 中文：拼音解码
    pub(crate) engine: Engine,
    /// 英文：前缀补全（中文模式下始终为空，切换时两边一起清）
    pub(crate) en: en::EnEngine,
    /// 候选页码。中文候选最多 128 条，一屏只放 9 条，需翻页
    pub(crate) page: usize,
    pub(crate) focused: bool,
    /// 当前焦点是密码框：全键透传
    pub(crate) sensitive: bool,
    /// Shift 按下但未松开：松开时决定是切换中英还是输入大写
    pub(crate) shift_pending: bool,
    /// 上一次「按下」是否被消费。松开事件跟着它走，避免「按下吞了、松开没吞」
    /// 导致客户端把字符又插进缓冲区。
    pub(crate) last_consumed: bool,
}

impl CtxInner {
    /// 两个引擎互斥，切换中英 / 放弃组字时一起清，避免残留串串味
    pub(crate) fn reset_all(&mut self) {
        self.engine.reset();
        self.en.reset();
        self.page = 0;
    }
}

// ---------------------------------------------------------------------------
// 按键处理
// ---------------------------------------------------------------------------

/// 中文模式的按键处理：组字、候选、翻页
pub(crate) fn key_zh(st: &mut CtxInner, keyval: u32) -> Outcome {
    let composing = st.engine.active();

    if composing {
        match keyval {
            // 空格 / 回车：提交当前页首候选
            KS_SPACE | KS_RETURN => {
                let text = pick(st.page * PAGE_SIZE, &st.engine.cands)
                    .unwrap_or_else(|| st.engine.input.clone());
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
            pick(st.page * PAGE_SIZE, &st.engine.cands)
                .unwrap_or_else(|| st.engine.input.clone())
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
pub(crate) fn key_en(st: &mut CtxInner, keyval: u32, state: u32) -> Outcome {
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

pub(crate) fn pick(idx: usize, cands: &[String]) -> Option<String> {
    cands.get(idx).cloned()
}

/// 取一次 UI 快照。中文按页切片，英文本来就只够一页。
pub(crate) fn snapshot(st: &CtxInner) -> Snapshot {
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

pub(crate) fn ascii_letter(keyval: u32) -> Option<char> {
    let c = char::from_u32(keyval)?;
    if c.is_ascii_alphabetic() {
        Some(c)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 一次按键的完整决策
// ---------------------------------------------------------------------------

/// 按键进来后的全部判断：松开跟随、密码框透传、Ctrl 组合、Shift 记账、
/// 中英分派。三个前端共用同一份，保证「同一个键在哪儿都一个表现」。
pub(crate) fn handle_key(
    global: &Global,
    st: &mut CtxInner,
    keyval: u32,
    state: u32,
    release: bool,
) -> Outcome {
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
        global.toggle();
        st.last_consumed = true;
        return Outcome {
            consumed: true,
            commit: None,
            ui: Some(Snapshot::empty()),
        };
    }

    // Ctrl+Shift+F 简繁切换（与 fcitx5 一致）
    if ctrl && shift && (keyval == 'f' as u32 || keyval == 'F' as u32) {
        let v = global.toggle_variant();
        st.engine.set_variant(v);
        st.page = 0;
        st.shift_pending = false;
        st.last_consumed = true;
        let snap = snapshot(st);
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
    if !global.chinese() && !global.en_completion() {
        if st.en.active() {
            st.reset_all();
        }
        st.last_consumed = false;
        return Outcome::ignore();
    }

    let out = if global.chinese() {
        key_zh(st, keyval)
    } else {
        key_en(st, keyval, state)
    };
    st.last_consumed = out.consumed;
    out
}
