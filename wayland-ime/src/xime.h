#ifndef XIME_H
#define XIME_H

#include <stdbool.h>
#include <stdint.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>

#include "engine.h"
#include "input-method-v2.h"
#include "popup.h"
#include "virtual-keyboard.h"

struct xime_state {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_compositor *compositor;
    struct wl_shm *shm;
    struct wl_seat *seat;

    struct zwp_input_method_manager_v2 *im_manager;
    struct zwp_input_method_v2 *im;
    struct zwp_virtual_keyboard_manager_v1 *vk_manager;
    struct zwp_virtual_keyboard_v1 *vk;

    struct zwp_input_method_keyboard_grab_v2 *grab;
    struct zwp_input_popup_surface_v2 *im_popup;

    struct xkb_context *xkb_ctx;
    struct xkb_keymap *xkb_keymap;
    struct xkb_state *xkb_state;
    int keymap_sent;

    bool active;
    bool chinese;
    bool verbose;
    bool inline_cands;
    bool running;
    bool have_ime_global;

    /* 每个键码的消费状态：按下时决定，抬起时复用。
     * evdev 键码最大到 KEY_MAX(0x2ff)，留足空间避免越界写。 */
    bool consumed[768];
    uint32_t mods_depressed, mods_latched, mods_locked, group;
    uint32_t done_serial; /* 收到的 done 事件数，commit 时作为 serial 使用 */
    int32_t repeat_rate, repeat_delay;

    /* 文字输入区矩形（input_popup_surface 的 text_input_rectangle 事件） */
    int have_rect;
    int32_t rect_x, rect_y, rect_w, rect_h;

    struct xime_engine engine;
    struct xime_popup popup;
};

void xime_log(const char *fmt, ...);

#endif
