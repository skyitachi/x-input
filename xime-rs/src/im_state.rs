//! 两个前端共用的中英状态。
//!
//! `xime-both` 会同时起两个进程：Wayland 前端（`xime`）和 DBus 前端
//! （`xime --dbus`）。它们的 `chinese` 状态原本各活各的，于是出现一个很难自证的现象：
//!
//! - 在终端里按一下 Shift，只有 Wayland 前端切到了英文；Qt 应用（走 DBus）那边
//!   仍是中文，看起来"两边不一样"。
//! - 反过来在 Qt 应用里按 Shift，只有 DBus 前端切到英文。之后在同一个应用里
//!   一直打不出中文，**而且界面上没有任何提示**——用户只会说"输入法坏了"。
//!
//! 这里用一个状态文件做汇合点：`$XDG_RUNTIME_DIR/xime-im-state`，内容 `zh` / `en`。
//! 切换时原子写，每次按键前读一次（页缓存命中，开销可忽略）。文件不存在时
//! 沿用各自的启动默认值，不影响单前端（只跑 `xime` 或只跑 `xime --dbus`）的用法。
//!
//! 放 `XDG_RUNTIME_DIR` 而不是 home：这是会话级状态，换了登录会话就该重置，
//! 也不该被 dotfiles 备份带走。

use std::fs;
use std::path::PathBuf;

/// 状态文件路径。`XDG_RUNTIME_DIR` 缺失（比如 cron 里）时返回 None，调用方当作"不同步"。
fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    let mut p = PathBuf::from(dir);
    p.push("xime-im-state");
    Some(p)
}

/// 读共享状态。`Some(true)` = 中文，`Some(false)` = 英文，`None` = 文件不存在/内容不认识。
pub fn read() -> Option<bool> {
    let s = fs::read_to_string(path()?).ok()?;
    match s.trim() {
        "zh" => Some(true),
        "en" => Some(false),
        _ => None,
    }
}

/// 写共享状态。先写临时文件再 rename，避免另一个进程读到写了一半的内容。
pub fn write(chinese: bool) {
    let Some(p) = path() else { return };
    let tmp = p.with_extension("tmp");
    if fs::write(&tmp, if chinese { "zh" } else { "en" }).is_ok() {
        let _ = fs::rename(&tmp, &p);
    }
}
