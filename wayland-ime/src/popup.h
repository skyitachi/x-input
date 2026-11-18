#ifndef XIME_POPUP_H
#define XIME_POPUP_H

#include <stddef.h>
#include <wayland-client.h>

#include "render.h"

struct xime_state;

/* 候选窗：一块 zwp_input_popup_surface_v2 包住的 wl_surface，由合成器贴到光标附近。
 * 内容完全由自己用 wl_shm + 预烘焙字形图集绘制，不依赖任何 GUI 工具包。 */
struct xime_popup {
    struct wl_surface *surface;
    struct wl_shm_pool *pool;
    struct wl_buffer *buffer;
    void *data;
    size_t size;
    int fd;
    int w, h;
    int visible;
};

/* activate 时：创建 wl_surface 并绑定 input_popup 角色 */
void xime_popup_role_create(struct xime_state *st);
/* 销毁角色（deactivate / 退出时调用） */
void xime_popup_role_destroy(struct xime_state *st);

/* 计算并上屏候选 */
void xime_popup_update(struct xime_state *st, const char **cands, int n);
void xime_popup_hide(struct xime_state *st);

int xime_popup_measure(const char **cands, int n, int *w, int *h);
void xime_popup_paint(struct xime_canvas *c, const char **cands, int n);

#endif
