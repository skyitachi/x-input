/*
 * ti_probe —— 最小 text-input-v3 客户端
 *
 * 目的：把「输入法发给客户端的文字到底有没有到」这件事从应用里剥离出来。
 * 它开一个 400x200 的窗口，拿到 wl_seat 后就 enable text_input，
 * 然后把收到的 preedit_string / commit_string 原样打印。
 *
 * 用法（在图形会话里）：
 *     ./ti_probe            # 窗口出现后点一下让它拿到焦点，然后打字
 *     ./ti_probe > log 2>&1 &
 *
 * 判读：
 *     [ti] COMMIT "你好"   → 合成器的 IME 中继是通的，问题在具体应用
 *     完全没有 preedit/commit → 合成器没把 IME 输出转给客户端
 */
#define _GNU_SOURCE
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "text-input-unstable-v3-client-protocol.h"

static struct wl_display *g_dpy;
static struct wl_compositor *g_comp;
static struct wl_shm *g_shm;
static struct xdg_wm_base *g_wm;
static struct wl_seat *g_seat;
static struct zwp_text_input_manager_v3 *g_tim;
static struct zwp_text_input_v3 *g_ti;

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stdout, fmt, ap);
    va_end(ap);
    fputc('\n', stdout);
    fflush(stdout);
}

/* ---------- text-input-v3 ---------- */

static void ti_enter(void *d, struct zwp_text_input_v3 *ti, struct wl_surface *s) {
    (void)d; (void)s;
    say("[ti] enter → enable + content_type + cursor_rect");
    zwp_text_input_v3_enable(ti);
    /* 和真实应用一样声明内容类型：普通文本输入 */
    zwp_text_input_v3_set_content_type(ti, ZWP_TEXT_INPUT_V3_CONTENT_HINT_NONE,
                                       ZWP_TEXT_INPUT_V3_CONTENT_PURPOSE_NORMAL);
    /* 光标矩形给一个合法值，顺便验证输入法候选窗能不能摆对位置 */
    zwp_text_input_v3_set_cursor_rectangle(ti, 40, 60, 0, 28);
    zwp_text_input_v3_commit(ti);
}

static void ti_leave(void *d, struct zwp_text_input_v3 *ti, struct wl_surface *s) {
    (void)d; (void)s; (void)ti;
    say("[ti] leave");
}

static void ti_preedit(void *d, struct zwp_text_input_v3 *ti, const char *text,
                       int32_t begin, int32_t end) {
    (void)d; (void)ti;
    say("[ti] PREEDIT \"%s\" (cursor %d..%d)", text ? text : "", begin, end);
}

static void ti_commit_string(void *d, struct zwp_text_input_v3 *ti, const char *text) {
    (void)d; (void)ti;
    say("[ti] **COMMIT** \"%s\"", text ? text : "");
}

static void ti_delete_surrounding(void *d, struct zwp_text_input_v3 *ti,
                                  uint32_t before, uint32_t after) {
    (void)d; (void)ti;
    say("[ti] delete_surrounding %u/%u", before, after);
}

static void ti_done(void *d, struct zwp_text_input_v3 *ti, uint32_t serial) {
    (void)d; (void)ti;
    say("[ti] done(serial=%u)", serial);
}

static const struct zwp_text_input_v3_listener ti_listener = {
    .enter = ti_enter,
    .leave = ti_leave,
    .preedit_string = ti_preedit,
    .commit_string = ti_commit_string,
    .delete_surrounding_text = ti_delete_surrounding,
    .done = ti_done,
};

static void seat_caps(void *d, struct wl_seat *seat, uint32_t caps) {
    (void)d;
    if (!g_tim || !(caps & WL_SEAT_CAPABILITY_KEYBOARD) || g_ti)
        return;
    g_ti = zwp_text_input_manager_v3_get_text_input(g_tim, seat);
    zwp_text_input_v3_add_listener(g_ti, &ti_listener, NULL);
    say("[ti] text_input 已创建，等 enter");
}
static void seat_name(void *d, struct wl_seat *s, const char *n) { (void)d; (void)s; (void)n; }
static const struct wl_seat_listener seat_listener = { seat_caps, seat_name };

/* ---------- xdg-shell ---------- */

static void wm_ping(void *d, struct xdg_wm_base *wm, uint32_t serial) {
    (void)d; xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_listener = { wm_ping };

static void surf_configure(void *d, struct xdg_surface *s, uint32_t serial) {
    (void)d; xdg_surface_ack_configure(s, serial);
}
static const struct xdg_surface_listener surf_listener = { surf_configure };

static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *st) {
    (void)d; (void)t; (void)w; (void)h; (void)st;
}
static void top_close(void *d, struct xdg_toplevel *t) { (void)d; (void)t; exit(0); }
static const struct xdg_toplevel_listener top_listener = { top_configure, top_close };

/* ---------- shm 缓冲 ---------- */

static struct wl_buffer *make_buffer(int w, int h, int32_t **pix) {
    int stride = w * 4, size = stride * h;
    char tmpl[] = "/tmp/ti_probe-XXXXXX";
    int fd = mkstemp(tmpl);
    if (fd < 0)
        return NULL;
    unlink(tmpl);
    if (ftruncate(fd, size) < 0) {
        close(fd);
        return NULL;
    }
    void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    struct wl_shm_pool *pool = wl_shm_create_pool(g_shm, fd, size);
    struct wl_buffer *buf = wl_shm_pool_create_buffer(pool, 0, w, h, stride, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    if (p == MAP_FAILED)
        return NULL;
    /* 深灰底，方便肉眼确认窗口存在 */
    int32_t *px = p;
    for (int i = 0; i < w * h; i++)
        px[i] = 0xFF202020;
    *pix = px;
    return buf;
}

/* ---------- registry ---------- */

static void global_add(void *d, struct wl_registry *reg, uint32_t name,
                       const char *iface, uint32_t ver) {
    (void)d;
    if (!strcmp(iface, wl_compositor_interface.name))
        g_comp = wl_registry_bind(reg, name, &wl_compositor_interface, ver < 4 ? ver : 4);
    else if (!strcmp(iface, wl_shm_interface.name))
        g_shm = wl_registry_bind(reg, name, &wl_shm_interface, 1);
    else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        g_wm = wl_registry_bind(reg, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(g_wm, &wm_listener, NULL);
    } else if (!strcmp(iface, wl_seat_interface.name)) {
        g_seat = wl_registry_bind(reg, name, &wl_seat_interface, ver < 7 ? ver : 7);
        wl_seat_add_listener(g_seat, &seat_listener, NULL);
    } else if (!strcmp(iface, zwp_text_input_manager_v3_interface.name))
        g_tim = wl_registry_bind(reg, name, &zwp_text_input_manager_v3_interface, 1);
}
static void global_del(void *d, struct wl_registry *r, uint32_t n) { (void)d; (void)r; (void)n; }
static const struct wl_registry_listener reg_listener = { global_add, global_del };

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    g_dpy = wl_display_connect(NULL);
    if (!g_dpy) {
        fprintf(stderr, "连不上合成器（WAYLAND_DISPLAY？）\n");
        return 1;
    }
    struct wl_registry *reg = wl_display_get_registry(g_dpy);
    wl_registry_add_listener(reg, &reg_listener, NULL);
    wl_display_roundtrip(g_dpy);
    wl_display_roundtrip(g_dpy);

    if (!g_comp || !g_shm || !g_wm) {
        fprintf(stderr, "缺 wl_compositor / wl_shm / xdg_wm_base\n");
        return 1;
    }
    if (!g_tim) {
        fprintf(stderr, "合成器没暴露 zwp_text_input_manager_v3 —— 这个桌面用不了 IME\n");
        return 1;
    }

    struct wl_surface *surf = wl_compositor_create_surface(g_comp);
    struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(g_wm, surf);
    xdg_surface_add_listener(xs, &surf_listener, NULL);
    struct xdg_toplevel *tl = xdg_surface_get_toplevel(xs);
    xdg_toplevel_add_listener(tl, &top_listener, NULL);
    xdg_toplevel_set_title(tl, "ti_probe");
    xdg_toplevel_set_app_id(tl, "ti_probe");

    int32_t *pixels = NULL;
    struct wl_buffer *buf = make_buffer(400, 200, &pixels);
    if (!buf) {
        fprintf(stderr, "建 shm 缓冲失败\n");
        return 1;
    }
    wl_surface_attach(surf, buf, 0, 0);
    wl_surface_damage_buffer(surf, 0, 0, 400, 200);
    wl_surface_commit(surf);

    say("[ti] 窗口已映射（app_id=ti_probe）。点一下窗口让它拿到焦点，然后打字。");

    while (wl_display_dispatch(g_dpy) != -1)
        ;
    say("[ti] 连接断开");
    return 0;
}
