#include "popup.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include "render.h"
#include "xime.h"

#define POPUP_PAD_X 10
#define POPUP_PAD_Y 6
#define ITEM_PAD_X 9
#define ITEM_GAP 4
#define POPUP_RADIUS 10

static uint32_t col_bg;
static uint32_t col_border;
static uint32_t col_text;
static uint32_t col_dim;
static uint32_t col_accent;
static uint32_t col_sel_text;
static int colors_ready;

static void init_colors(void)
{
    if (colors_ready)
        return;
    col_bg = xime_argb(247, 251, 252, 254);
    col_border = xime_argb(36, 0, 0, 0);
    col_text = xime_argb(255, 23, 24, 28);
    col_dim = xime_argb(255, 122, 128, 140);
    col_accent = xime_argb(255, 47, 107, 255);
    col_sel_text = xime_argb(255, 255, 255, 255);
    colors_ready = 1;
}

static int item_label(char *buf, size_t cap, int idx, const char *cand)
{
    return snprintf(buf, cap, "%d. %s", idx + 1, cand);
}

int xime_popup_measure(const char **cands, int n, int *w, int *h)
{
    unsigned int width = POPUP_PAD_X * 2;
    int i;

    init_colors();
    for (i = 0; i < n; i++) {
        char label[128];

        item_label(label, sizeof(label), i, cands[i]);
        width += canvas_text_width(label) + ITEM_PAD_X * 2;
        if (i)
            width += ITEM_GAP;
    }
    *w = (int)width;
    *h = xime_line_height() + POPUP_PAD_Y * 2;
    return 0;
}

void xime_popup_paint(struct xime_canvas *c, const char **cands, int n)
{
    int x = POPUP_PAD_X;
    int i;

    init_colors();
    canvas_clear(c, 0);

    /* 底 + 1px 描边 */
    canvas_round_rect(c, 0, 0, c->w, c->h, POPUP_RADIUS, col_border);
    canvas_round_rect(c, 1, 1, c->w - 2, c->h - 2, POPUP_RADIUS - 1, col_bg);

    for (i = 0; i < n; i++) {
        char label[128];
        unsigned int tw;
        int item_w, text_y = POPUP_PAD_Y;

        item_label(label, sizeof(label), i, cands[i]);
        tw = canvas_text_width(label);
        item_w = (int)tw + ITEM_PAD_X * 2;

        if (i == 0) {
            canvas_round_rect(c, x, text_y - 2, item_w, xime_line_height() + 4, 7, col_accent);
            canvas_text(c, x + ITEM_PAD_X, text_y, label, col_sel_text);
        } else {
            canvas_text(c, x + ITEM_PAD_X, text_y, label, col_text);
        }
        x += item_w + ITEM_GAP;
    }
}

static void popup_surface_rect(void *data, struct zwp_input_popup_surface_v2 *surf, int32_t x, int32_t y,
                               int32_t width, int32_t height)
{
    struct xime_state *st = data;

    (void)surf;
    st->have_rect = 1;
    st->rect_x = x;
    st->rect_y = y;
    st->rect_w = width;
    st->rect_h = height;
}

static const struct zwp_input_popup_surface_v2_listener popup_surface_listener = {
    .text_input_rectangle = popup_surface_rect,
};

static void buffer_release(void *data, struct wl_buffer *buffer)
{
    (void)data;
    (void)buffer;
}

static const struct wl_buffer_listener buffer_listener = {
    .release = buffer_release,
};

static void destroy_buffer(struct xime_popup *p)
{
    if (p->buffer) {
        wl_buffer_destroy(p->buffer);
        p->buffer = NULL;
    }
    if (p->pool) {
        wl_shm_pool_destroy(p->pool);
        p->pool = NULL;
    }
    if (p->data && p->data != MAP_FAILED) {
        munmap(p->data, p->size);
        p->data = NULL;
    }
    if (p->fd >= 0) {
        close(p->fd);
        p->fd = -1;
    }
    p->w = p->h = 0;
    p->size = 0;
}

static int ensure_buffer(struct xime_state *st, int w, int h)
{
    struct xime_popup *p = &st->popup;
    size_t size;
    int fd;

    if (p->buffer && p->w == w && p->h == h)
        return 0;
    if (!st->shm)
        return -1;

    destroy_buffer(p);
    size = (size_t)w * (size_t)h * 4u;
    fd = memfd_create("xime-popup", MFD_CLOEXEC);
    if (fd < 0)
        return -1;
    if (ftruncate(fd, (off_t)size) < 0) {
        close(fd);
        return -1;
    }
    p->data = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p->data == MAP_FAILED) {
        p->data = NULL;
        close(fd);
        return -1;
    }
    p->fd = fd;
    p->size = size;
    p->pool = wl_shm_create_pool(st->shm, fd, (int32_t)size);
    if (!p->pool) {
        destroy_buffer(p);
        return -1;
    }
    p->buffer = wl_shm_pool_create_buffer(p->pool, 0, w, h, w * 4, WL_SHM_FORMAT_ARGB8888);
    if (!p->buffer) {
        destroy_buffer(p);
        return -1;
    }
    wl_buffer_add_listener(p->buffer, &buffer_listener, p);
    p->w = w;
    p->h = h;
    return 0;
}

void xime_popup_role_create(struct xime_state *st)
{
    struct xime_popup *p = &st->popup;
    struct wl_region *empty;

    if (p->surface)
        return;
    if (!st->compositor || !st->im)
        return;

    p->surface = wl_compositor_create_surface(st->compositor);
    if (!p->surface)
        return;
    /* 候选窗不接收输入事件 */
    empty = wl_compositor_create_region(st->compositor);
    if (empty) {
        wl_surface_set_input_region(p->surface, empty);
        wl_region_destroy(empty);
    }
    st->im_popup = zwp_input_method_v2_get_input_popup_surface(st->im, p->surface);
    if (st->im_popup)
        zwp_input_popup_surface_v2_add_listener(st->im_popup, &popup_surface_listener, st);
    wl_surface_commit(p->surface);
}

void xime_popup_role_destroy(struct xime_state *st)
{
    struct xime_popup *p = &st->popup;

    if (p->surface) {
        wl_surface_attach(p->surface, NULL, 0, 0);
        wl_surface_commit(p->surface);
    }
    if (st->im_popup) {
        zwp_input_popup_surface_v2_destroy(st->im_popup);
        st->im_popup = NULL;
    }
    if (p->surface) {
        wl_surface_destroy(p->surface);
        p->surface = NULL;
    }
    destroy_buffer(p);
    p->visible = 0;
}

void xime_popup_update(struct xime_state *st, const char **cands, int n)
{
    struct xime_popup *p = &st->popup;
    struct xime_canvas c;
    int w = 0, h = 0;

    if (!p->surface || n <= 0) {
        xime_popup_hide(st);
        return;
    }
    xime_popup_measure(cands, n, &w, &h);
    if (w <= 0 || h <= 0)
        return;
    if (ensure_buffer(st, w, h) < 0)
        return;

    c.px = (uint32_t *)p->data;
    c.w = w;
    c.h = h;
    xime_popup_paint(&c, cands, n);

    wl_surface_attach(p->surface, p->buffer, 0, 0);
    wl_surface_damage_buffer(p->surface, 0, 0, w, h);
    wl_surface_commit(p->surface);
    p->visible = 1;
}

void xime_popup_hide(struct xime_state *st)
{
    struct xime_popup *p = &st->popup;

    if (!p->surface || !p->visible)
        return;
    wl_surface_attach(p->surface, NULL, 0, 0);
    wl_surface_commit(p->surface);
    p->visible = 0;
}
