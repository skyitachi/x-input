//! 协议请求构造：把 wire 层收窄成一组具名函数。
//!
//! 每个函数对应 XML 里一个 request，opcode 都在注释里标明来源。
//! 这里只做编码，不持有状态——对象 ID 由调用方（main）管理。

use crate::wire::{Conn, Msg, DISPLAY_ID};
use std::io;
use std::os::unix::io::RawFd;

// 全局接口名（wl_registry.global 里出现的字符串）
pub const IFACE_COMPOSITOR: &str = "wl_compositor";
pub const IFACE_SHM: &str = "wl_shm";
pub const IFACE_SEAT: &str = "wl_seat";
pub const IFACE_INPUT_METHOD: &str = "zwp_input_method_manager_v2";
pub const IFACE_VIRTUAL_KEYBOARD: &str = "zwp_virtual_keyboard_manager_v1";
pub const IFACE_OUTPUT: &str = "wl_output";
pub const IFACE_FRACTIONAL_SCALE: &str = "wp_fractional_scale_manager_v1";
pub const IFACE_VIEWPORTER: &str = "wp_viewporter";

// --- wl_output（事件解析用，wire 格式见 wayland.xml）
// event 3: scale(int factor) —— 整数缩放，since v2
// --- wp_fractional_scale_v1（事件解析用）
// event 0: preferred_scale(uint scale) —— scale × 120

// --- wl_display
pub fn get_registry(c: &mut Conn, id: u32) -> io::Result<()> {
    // request 1
    c.send(DISPLAY_ID, 1, Msg::new().new_id(id))
}

pub fn sync(c: &mut Conn, id: u32) -> io::Result<()> {
    // request 0
    c.send(DISPLAY_ID, 0, Msg::new().new_id(id))
}

// --- wl_registry
pub fn bind(c: &mut Conn, reg: u32, name: u32, iface: &str, version: u32, id: u32) -> io::Result<()> {
    // request 0: bind(name uint, id new_id) + 契约要求的接口名与版本
    c.send(reg, 0, Msg::new().u32(name).string(iface).u32(version).new_id(id))
}

// --- wl_compositor
pub fn create_surface(c: &mut Conn, comp: u32, id: u32) -> io::Result<()> {
    // request 0
    c.send(comp, 0, Msg::new().new_id(id))
}

pub fn create_region(c: &mut Conn, comp: u32, id: u32) -> io::Result<()> {
    // request 1
    c.send(comp, 1, Msg::new().new_id(id))
}

// --- wl_region
pub fn region_destroy(c: &mut Conn, region: u32) -> io::Result<()> {
    // request 0
    c.send(region, 0, &Msg::new())
}

// --- wl_shm
pub fn create_pool(c: &mut Conn, shm: u32, id: u32, fd: RawFd, size: i32) -> io::Result<()> {
    // request 0: create_pool(id new_id, fd fd, size int)
    c.send(shm, 0, Msg::new().new_id(id).fd(fd).i32(size))
}

// --- wl_shm_pool
pub fn pool_create_buffer(
    c: &mut Conn,
    pool: u32,
    id: u32,
    offset: i32,
    w: i32,
    h: i32,
    stride: i32,
    format: u32,
) -> io::Result<()> {
    // request 0
    c.send(pool, 0, Msg::new().new_id(id).i32(offset).i32(w).i32(h).i32(stride).u32(format))
}

// request 1
pub fn pool_destroy(c: &mut Conn, pool: u32) -> io::Result<()> {
    c.send(pool, 1, &Msg::new())
}

// --- wl_buffer
// request 0
pub fn buffer_destroy(c: &mut Conn, buffer: u32) -> io::Result<()> {
    c.send(buffer, 0, &Msg::new())
}

// --- wl_surface
pub fn surface_attach(c: &mut Conn, surf: u32, buffer: Option<u32>, x: i32, y: i32) -> io::Result<()> {
    // request 1: attach(buffer object, x int, y int)
    let b = buffer.unwrap_or(0);
    c.send(surf, 1, Msg::new().u32(b).i32(x).i32(y))
}

pub fn surface_damage(c: &mut Conn, surf: u32, x: i32, y: i32, w: i32, h: i32) -> io::Result<()> {
    // request 2
    c.send(surf, 2, Msg::new().i32(x).i32(y).i32(w).i32(h))
}

pub fn surface_commit(c: &mut Conn, surf: u32) -> io::Result<()> {
    // request 6
    c.send(surf, 6, &Msg::new())
}

pub fn surface_set_input_region(c: &mut Conn, surf: u32, region: u32) -> io::Result<()> {
    // request 5：默认输入区是整个 surface，候选窗不应吃鼠标事件，故设为空区
    c.send(surf, 5, Msg::new().u32(region))
}

pub fn surface_damage_buffer(c: &mut Conn, surf: u32, x: i32, y: i32, w: i32, h: i32) -> io::Result<()> {
    // request 9（since v4）：按缓冲像素坐标标脏，比 surface 坐标更适合高 DPI
    c.send(surf, 9, Msg::new().i32(x).i32(y).i32(w).i32(h))
}

// --- wp_fractional_scale_manager_v1
pub fn get_fractional_scale(c: &mut Conn, mgr: u32, surf: u32, id: u32) -> io::Result<()> {
    // request 1: get_fractional_scale(id new_id, surface object)
    c.send(mgr, 1, Msg::new().new_id(id).u32(surf))
}

// --- wp_viewporter（opcode 1: get_viewport(id new_id, surface object)）
pub fn get_viewport(c: &mut Conn, mgr: u32, surf: u32, id: u32) -> io::Result<()> {
    c.send(mgr, 1, Msg::new().new_id(id).u32(surf))
}

// --- wp_viewport.set_destination（opcode 2: int width, int height）
pub fn viewport_set_destination(c: &mut Conn, vp: u32, w: i32, h: i32) -> io::Result<()> {
    c.send(vp, 2, Msg::new().i32(w).i32(h))
}

// --- zwp_input_method_manager_v2
pub fn im_get_input_method(c: &mut Conn, mgr: u32, seat: u32, id: u32) -> io::Result<()> {
    // request 0: get_input_method(seat object, input_method new_id)
    c.send(mgr, 0, Msg::new().u32(seat).new_id(id))
}

// --- zwp_input_method_v2
pub fn im_commit_string(c: &mut Conn, im: u32, text: &str) -> io::Result<()> {
    // request 0
    c.send(im, 0, Msg::new().string(text))
}

pub fn im_set_preedit(c: &mut Conn, im: u32, text: &str, begin: i32, end: i32) -> io::Result<()> {
    // request 1
    c.send(im, 1, Msg::new().string(text).i32(begin).i32(end))
}

/// request 3。serial 必须等于已收到的 done 事件计数，否则严格实现的合成器会丢弃本次状态。
pub fn im_commit(c: &mut Conn, im: u32, serial: u32) -> io::Result<()> {
    c.send(im, 3, Msg::new().u32(serial))
}

pub fn im_get_popup(c: &mut Conn, im: u32, id: u32, surface: u32) -> io::Result<()> {
    // request 4: get_input_popup_surface(id new_id, surface object)
    c.send(im, 4, Msg::new().new_id(id).u32(surface))
}

pub fn im_grab_keyboard(c: &mut Conn, im: u32, id: u32) -> io::Result<()> {
    // request 5
    c.send(im, 5, Msg::new().new_id(id))
}

// --- zwp_virtual_keyboard_manager_v1
pub fn vk_create(c: &mut Conn, mgr: u32, seat: u32, id: u32) -> io::Result<()> {
    // request 0: create_virtual_keyboard(seat object, id new_id)
    c.send(mgr, 0, Msg::new().u32(seat).new_id(id))
}

// --- zwp_virtual_keyboard_v1
pub fn vk_keymap(c: &mut Conn, vk: u32, format: u32, fd: RawFd, size: u32) -> io::Result<()> {
    // request 0
    c.send(vk, 0, Msg::new().u32(format).fd(fd).u32(size))
}

pub fn vk_key(c: &mut Conn, vk: u32, time: u32, key: u32, state: u32) -> io::Result<()> {
    // request 1
    c.send(vk, 1, Msg::new().u32(time).u32(key).u32(state))
}

pub fn vk_modifiers(
    c: &mut Conn,
    vk: u32,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) -> io::Result<()> {
    // request 2
    c.send(vk, 2, Msg::new().u32(depressed).u32(latched).u32(locked).u32(group))
}
