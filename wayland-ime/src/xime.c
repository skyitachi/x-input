#include "xime.h"

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <unistd.h>

#include "dict.h"
#include "render.h"
#include "utf8.h"

#define XIME_VERSION "0.2.0"

static struct xime_state *g_state;
static volatile sig_atomic_t g_stop;

void xime_log(const char *fmt, ...)
{
    va_list ap;

    if (g_state && !g_state->verbose)
        return;
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fputc('\n', stderr);
}

static void on_signal(int sig)
{
    (void)sig;
    g_stop = 1;
}

static void flush_display(struct xime_state *st)
{
    if (st->display)
        wl_display_flush(st->display);
}

/* ---------- 输入法状态同步 ---------- */

static void fill_cand_ptrs(struct xime_state *st, const char **out, int *n)
{
    int i;

    for (i = 0; i < st->engine.ncand; i++)
        out[i] = st->engine.cands[i];
    *n = st->engine.ncand;
}

static void refresh(struct xime_state *st)
{
    const char *ptrs[XIME_MAX_CANDS];
    int n = 0;

    if (!st->im)
        return;
    if (st->inline_cands) {
        char buf[256];
        size_t len;

        engine_inline_preedit(&st->engine, buf, sizeof(buf));
        len = strlen(buf);
        zwp_input_method_v2_set_preedit_string(st->im, buf, (int32_t)len, (int32_t)len);
        zwp_input_method_v2_commit(st->im, st->done_serial);
        xime_popup_hide(st);
    } else {
        size_t len = strlen(st->engine.preedit);

        fill_cand_ptrs(st, ptrs, &n);
        zwp_input_method_v2_set_preedit_string(st->im, st->engine.preedit, (int32_t)len, (int32_t)len);
        zwp_input_method_v2_commit(st->im, st->done_serial);
        if (n > 0)
            xime_popup_update(st, ptrs, n);
        else
            xime_popup_hide(st);
    }
    flush_display(st);
}

static void commit_text(struct xime_state *st, const char *text)
{
    if (!st->im || !text)
        return;
    if (*text)
        zwp_input_method_v2_commit_string(st->im, text);
    zwp_input_method_v2_set_preedit_string(st->im, "", 0, 0);
    zwp_input_method_v2_commit(st->im, st->done_serial);
    engine_reset(&st->engine);
    xime_popup_hide(st);
    flush_display(st);
}

/* ---------- 虚拟键盘：把不消费的按键回注给应用 ---------- */

static void vk_ensure(struct xime_state *st)
{
    if (st->vk || !st->vk_manager || !st->seat)
        return;
    st->vk = zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(st->vk_manager, st->seat);
}

static void vk_send_keymap(struct xime_state *st, const char *map, uint32_t size)
{
    int fd;

    vk_ensure(st);
    if (!st->vk)
        return;
    fd = memfd_create("xime-keymap", MFD_CLOEXEC);
    if (fd < 0)
        return;
    if (write(fd, map, size) != (ssize_t)size) {
        close(fd);
        return;
    }
    lseek(fd, 0, SEEK_SET);
    zwp_virtual_keyboard_v1_keymap(st->vk, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, size);
    /* fd 已在 marshal 时被 libwayland 复制，flush 后即可关闭 */
    flush_display(st);
    close(fd);
    st->keymap_sent = 1;
}

static void forward_key(struct xime_state *st, uint32_t time, uint32_t key, uint32_t state)
{
    if (!st->vk)
        return;
    zwp_virtual_keyboard_v1_modifiers(st->vk, st->mods_depressed, st->mods_latched, st->mods_locked,
                                      st->group);
    zwp_virtual_keyboard_v1_key(st->vk, time, key, state);
}

/* ---------- 键盘事件 ---------- */

static void grab_keymap(void *data, struct zwp_input_method_keyboard_grab_v2 *grab, uint32_t format,
                        int32_t fd, uint32_t size)
{
    struct xime_state *st = data;
    char *map;

    (void)grab;
    if (format != WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1) {
        close(fd);
        return;
    }
    map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (map == MAP_FAILED) {
        close(fd);
        return;
    }
    if (!st->xkb_ctx)
        st->xkb_ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (st->xkb_ctx) {
        struct xkb_keymap *km = xkb_keymap_new_from_string(st->xkb_ctx, map, XKB_KEYMAP_FORMAT_TEXT_V1,
                                                          XKB_KEYMAP_COMPILE_NO_FLAGS);

        if (km) {
            if (st->xkb_state)
                xkb_state_unref(st->xkb_state);
            if (st->xkb_keymap)
                xkb_keymap_unref(st->xkb_keymap);
            st->xkb_keymap = km;
            st->xkb_state = xkb_state_new(km);
            xime_log("keymap 已载入");
        }
    }
    vk_send_keymap(st, map, size);
    munmap(map, size);
    close(fd);
}

static void grab_modifiers(void *data, struct zwp_input_method_keyboard_grab_v2 *grab, uint32_t serial,
                           uint32_t mods_depressed, uint32_t mods_latched, uint32_t mods_locked,
                           uint32_t group)
{
    struct xime_state *st = data;

    (void)grab;
    (void)serial;
    st->mods_depressed = mods_depressed;
    st->mods_latched = mods_latched;
    st->mods_locked = mods_locked;
    st->group = group;
    if (st->xkb_state)
        xkb_state_update_mask(st->xkb_state, mods_depressed, mods_latched, mods_locked, 0, 0, group);
}

static void grab_repeat_info(void *data, struct zwp_input_method_keyboard_grab_v2 *grab, int32_t rate,
                             int32_t delay)
{
    struct xime_state *st = data;

    (void)grab;
    st->repeat_rate = rate;
    st->repeat_delay = delay;
}

static int has_command_mod(struct xime_state *st)
{
    if (!st->xkb_state)
        return 0;
    return xkb_state_mod_name_is_active(st->xkb_state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_EFFECTIVE) > 0
           || xkb_state_mod_name_is_active(st->xkb_state, XKB_MOD_NAME_ALT, XKB_STATE_MODS_EFFECTIVE) > 0
           || xkb_state_mod_name_is_active(st->xkb_state, XKB_MOD_NAME_LOGO, XKB_STATE_MODS_EFFECTIVE) > 0;
}

static int ascii_of(xkb_keysym_t sym, char *out)
{
    if (sym >= XKB_KEY_a && sym <= XKB_KEY_z) {
        *out = (char)('a' + (sym - XKB_KEY_a));
        return 1;
    }
    if (sym >= XKB_KEY_A && sym <= XKB_KEY_Z) {
        *out = (char)('a' + (sym - XKB_KEY_A));
        return 1;
    }
    if (sym >= XKB_KEY_0 && sym <= XKB_KEY_9) {
        *out = (char)('0' + (sym - XKB_KEY_0));
        return 1;
    }
    return 0;
}

static const char *punct_of(xkb_keysym_t sym)
{
    switch (sym) {
    case XKB_KEY_comma:
        return "，";
    case XKB_KEY_period:
        return "。";
    case XKB_KEY_question:
        return "？";
    case XKB_KEY_exclam:
        return "！";
    case XKB_KEY_colon:
        return "：";
    case XKB_KEY_semicolon:
        return "；";
    case XKB_KEY_parenleft:
        return "（";
    case XKB_KEY_parenright:
        return "）";
    case XKB_KEY_bracketleft:
        return "【";
    case XKB_KEY_bracketright:
        return "】";
    case XKB_KEY_less:
        return "《";
    case XKB_KEY_greater:
        return "》";
    case XKB_KEY_slash:
        return "、";
    case XKB_KEY_quotedbl:
        return "“”";
    case XKB_KEY_apostrophe:
        return "’";
    default:
        return NULL;
    }
}

static void grab_key(void *data, struct zwp_input_method_keyboard_grab_v2 *grab, uint32_t serial,
                     uint32_t time, uint32_t key, uint32_t state)
{
    struct xime_state *st = data;
    xkb_keysym_t sym = XKB_KEY_NoSymbol;
    int pressed = (state == WL_KEYBOARD_KEY_STATE_PRESSED);
    char ch;

    (void)grab;
    (void)serial;

    if (st->xkb_state)
        sym = xkb_state_key_get_one_sym(st->xkb_state, key + 8);

    if (!pressed) {
        if (key < 768 && st->consumed[key])
            st->consumed[key] = false;
        else
            forward_key(st, time, key, state);
        flush_display(st);
        return;
    }

    /* Shift：中英切换 */
    if ((sym == XKB_KEY_Shift_L || sym == XKB_KEY_Shift_R) && !has_command_mod(st)) {
        if (st->engine.ncand > 0)
            commit_text(st, st->engine.cands[0]);
        else if (engine_active(&st->engine))
            commit_text(st, st->engine.input);
        st->chinese = !st->chinese;
        if (key < 768)
            st->consumed[key] = true;
        xime_log("切换到%s模式", st->chinese ? "中文" : "英文");
        return;
    }

    if (has_command_mod(st) || !st->chinese) {
        forward_key(st, time, key, state);
        flush_display(st);
        return;
    }

    /* 数字选词 */
    if (sym >= XKB_KEY_1 && sym <= XKB_KEY_9) {
        int idx = (int)(sym - XKB_KEY_1);

        if (engine_active(&st->engine)) {
            if (idx < st->engine.ncand)
                commit_text(st, st->engine.cands[idx]);
            else
                commit_text(st, st->engine.input);
            if (key < 768)
            st->consumed[key] = true;
            return;
        }
        forward_key(st, time, key, state);
        flush_display(st);
        return;
    }

    if (sym == XKB_KEY_BackSpace) {
        if (engine_active(&st->engine)) {
            engine_backspace(&st->engine);
            refresh(st);
            if (key < 768)
            st->consumed[key] = true;
            return;
        }
        forward_key(st, time, key, state);
        flush_display(st);
        return;
    }

    if (sym == XKB_KEY_space) {
        if (st->engine.ncand > 0)
            commit_text(st, st->engine.cands[0]);
        else if (engine_active(&st->engine))
            commit_text(st, st->engine.input);
        else {
            forward_key(st, time, key, state);
            flush_display(st);
            return;
        }
        if (key < 768)
            st->consumed[key] = true;
        return;
    }

    if (sym == XKB_KEY_Return || sym == XKB_KEY_KP_Enter) {
        if (engine_active(&st->engine)) {
            commit_text(st, st->engine.input);
            if (key < 768)
            st->consumed[key] = true;
            return;
        }
        forward_key(st, time, key, state);
        flush_display(st);
        return;
    }

    if (sym == XKB_KEY_Escape) {
        if (engine_active(&st->engine)) {
            engine_reset(&st->engine);
            refresh(st);
            if (key < 768)
            st->consumed[key] = true;
            return;
        }
        forward_key(st, time, key, state);
        flush_display(st);
        return;
    }

    if (ascii_of(sym, &ch)) {
        if (ch >= '0' && ch <= '9') { /* 数字但无输入串：交给应用 */
            forward_key(st, time, key, state);
            flush_display(st);
            return;
        }
        engine_push_char(&st->engine, ch);
        refresh(st);
        if (key < 768)
            st->consumed[key] = true;
        return;
    }

    {
        const char *p = punct_of(sym);

        if (p) {
            if (st->engine.ncand > 0) {
                commit_text(st, st->engine.cands[0]);
                commit_text(st, p);
            } else {
                commit_text(st, p);
            }
            if (key < 768)
            st->consumed[key] = true;
            return;
        }
    }

    forward_key(st, time, key, state);
    flush_display(st);
}

static const struct zwp_input_method_keyboard_grab_v2_listener grab_listener = {
    .keymap = grab_keymap,
    .key = grab_key,
    .modifiers = grab_modifiers,
    .repeat_info = grab_repeat_info,
};

/* ---------- input method 事件 ---------- */

static void im_activate(void *data, struct zwp_input_method_v2 *im)
{
    struct xime_state *st = data;

    (void)im;
    st->active = 1;
    engine_reset(&st->engine);
    memset(st->consumed, 0, sizeof(st->consumed));
    xime_popup_role_create(st);
    if (!st->grab) {
        st->grab = zwp_input_method_v2_grab_keyboard(st->im);
        if (st->grab)
            zwp_input_method_keyboard_grab_v2_add_listener(st->grab, &grab_listener, st);
    }
    zwp_input_method_v2_set_preedit_string(st->im, "", 0, 0);
    zwp_input_method_v2_commit(st->im, st->done_serial);
    flush_display(st);
    xime_log("activate");
}

static void im_deactivate(void *data, struct zwp_input_method_v2 *im)
{
    struct xime_state *st = data;

    (void)im;
    st->active = 0;
    engine_reset(&st->engine);
    if (st->grab) {
        zwp_input_method_keyboard_grab_v2_release(st->grab);
        st->grab = NULL;
    }
    xime_popup_role_destroy(st);
    memset(st->consumed, 0, sizeof(st->consumed));
    zwp_input_method_v2_set_preedit_string(st->im, "", 0, 0);
    zwp_input_method_v2_commit(st->im, st->done_serial);
    flush_display(st);
    xime_log("deactivate");
}

static void im_surrounding_text(void *data, struct zwp_input_method_v2 *im, const char *text,
                               uint32_t cursor, uint32_t anchor)
{
    (void)data;
    (void)im;
    (void)text;
    (void)cursor;
    (void)anchor;
}

static void im_text_change_cause(void *data, struct zwp_input_method_v2 *im, uint32_t cause)
{
    (void)data;
    (void)im;
    (void)cause;
}

static void im_content_type(void *data, struct zwp_input_method_v2 *im, uint32_t hint, uint32_t purpose)
{
    (void)data;
    (void)im;
    (void)hint;
    (void)purpose;
}

static void im_done(void *data, struct zwp_input_method_v2 *im)
{
    struct xime_state *st = data;

    (void)im;
    st->done_serial++;
}

static void im_unavailable(void *data, struct zwp_input_method_v2 *im)
{
    struct xime_state *st = data;

    (void)im;
    fprintf(stderr,
            "xime: 合成器报告 input-method 不可用（很可能已有另一个输入法占用了这个 seat）\n");
    st->running = false;
    g_stop = 1;
}

static const struct zwp_input_method_v2_listener im_listener = {
    .activate = im_activate,
    .deactivate = im_deactivate,
    .surrounding_text = im_surrounding_text,
    .text_change_cause = im_text_change_cause,
    .content_type = im_content_type,
    .done = im_done,
    .unavailable = im_unavailable,
};

/* ---------- seat ---------- */

static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t caps)
{
    (void)data;
    (void)seat;
    (void)caps;
}

static void seat_name(void *data, struct wl_seat *seat, const char *name)
{
    (void)data;
    (void)seat;
    xime_log("seat: %s", name);
}

static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_capabilities,
    .name = seat_name,
};

/* ---------- registry ---------- */

static void try_init_objects(struct xime_state *st)
{
    if (!st->im && st->im_manager && st->seat) {
        st->im = zwp_input_method_manager_v2_get_input_method(st->im_manager, st->seat);
        if (st->im)
            zwp_input_method_v2_add_listener(st->im, &im_listener, st);
    }
    vk_ensure(st);
}

static void registry_global(void *data, struct wl_registry *registry, uint32_t name, const char *iface,
                            uint32_t version)
{
    struct xime_state *st = data;

    if (strcmp(iface, wl_compositor_interface.name) == 0) {
        st->compositor = wl_registry_bind(registry, name, &wl_compositor_interface, version < 4 ? version : 4);
    } else if (strcmp(iface, wl_shm_interface.name) == 0) {
        st->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    } else if (strcmp(iface, wl_seat_interface.name) == 0) {
        st->seat = wl_registry_bind(registry, name, &wl_seat_interface, version < 5 ? version : 5);
        wl_seat_add_listener(st->seat, &seat_listener, st);
    } else if (strcmp(iface, zwp_input_method_manager_v2_interface.name) == 0) {
        st->im_manager = wl_registry_bind(registry, name, &zwp_input_method_manager_v2_interface, 1);
        st->have_ime_global = true;
    } else if (strcmp(iface, zwp_virtual_keyboard_manager_v1_interface.name) == 0) {
        st->vk_manager = wl_registry_bind(registry, name, &zwp_virtual_keyboard_manager_v1_interface, 1);
    } else {
        xime_log("忽略全局对象 %s (v%u)", iface, version);
        return;
    }
    xime_log("绑定 %s v%u", iface, version);
    try_init_objects(st);
}

static void registry_global_remove(void *data, struct wl_registry *registry, uint32_t name)
{
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

/* ---------- 单实例锁 ---------- */

static int take_lock(void)
{
    const char *rt = getenv("XDG_RUNTIME_DIR");
    char path[512];
    int fd;

    snprintf(path, sizeof(path), "%s/xime.lock", rt && *rt ? rt : "/tmp");
    fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    if (fd < 0)
        return -1;
    if (flock(fd, LOCK_EX | LOCK_NB) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

/* ---------- 调试/自检 ---------- */

static int render_test(const char *path, const char *input)
{
    struct xime_engine e;
    const char *ptrs[XIME_MAX_CANDS];
    struct xime_canvas c;
    int w = 0, h = 0, i;

    engine_reset(&e);
    for (i = 0; input[i]; i++)
        engine_push_char(&e, input[i]);
    for (i = 0; i < e.ncand; i++)
        ptrs[i] = e.cands[i];
    if (e.ncand == 0) {
        fprintf(stderr, "xime: 输入 \"%s\" 没有候选\n", input);
        return 1;
    }
    xime_popup_measure(ptrs, e.ncand, &w, &h);
    c.px = calloc((size_t)w * (size_t)h, sizeof(uint32_t));
    c.w = w;
    c.h = h;
    if (!c.px)
        return 1;
    xime_popup_paint(&c, ptrs, e.ncand);
    if (canvas_write_bmp(&c, path) != 0) {
        fprintf(stderr, "xime: 写入 %s 失败\n", path);
        free(c.px);
        return 1;
    }
    free(c.px);
    printf("preedit=%s 候选=%d 尺寸=%dx%d -> %s\n", e.preedit, e.ncand, w, h, path);
    return 0;
}

/* 打印某个拼音串的候选，用于调引擎/查词库命中情况 */
static int query_test(const char *input)
{
    struct xime_engine e;
    int i;

    engine_reset(&e);
    for (i = 0; input[i]; i++)
        engine_push_char(&e, input[i]);
    printf("拼音: %s\n", e.preedit);
    printf("候选: %d\n", e.ncand);
    for (i = 0; i < e.ncand; i++)
        printf("  %d. %-24s cost=%.2f\n", i + 1, e.cands[i], e.cost[i]);
    return e.ncand > 0 ? 0 : 1;
}

static void usage(const char *argv0)
{
    printf("xime " XIME_VERSION " - Wayland 原生输入法\n\n"
           "用法: %s [选项]\n\n"
           "  --check              连接合成器，检查所需协议是否可用后退出\n"
           "  --inline-candidates  候选内联显示在 preedit 里（无候选窗，零字体依赖）\n"
           "  --dict PATH          额外词表\n"
           "  --query 拼音          打印该拼音串的候选（调参/验证词库用）\n"
           "  --render-test PATH [拼音]\n"
           "                       把候选条渲染成 BMP，用于离线检查字形/排版\n"
           "  --verbose            输出调试日志\n"
           "  --version            显示版本\n",
           argv0);
}

int main(int argc, char **argv)
{
    struct xime_state st;
    const char *extra_dict = NULL;
    const char *render_path = NULL;
    const char *render_input = "nihao";
    int lock_fd = -1;
    int check_only = 0;
    int i;

    memset(&st, 0, sizeof(st));
    st.running = true;
    st.chinese = true;
    g_state = &st;
    st.popup.fd = -1;

    dict_init(NULL);
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--verbose") == 0) {
            st.verbose = true;
        } else if (strcmp(argv[i], "--inline-candidates") == 0) {
            st.inline_cands = true;
        } else if (strcmp(argv[i], "--version") == 0) {
            printf("xime " XIME_VERSION "\n");
            return 0;
        } else if (strcmp(argv[i], "--help") == 0 || strcmp(argv[i], "-h") == 0) {
            usage(argv[0]);
            return 0;
        } else if (strcmp(argv[i], "--dict") == 0 && i + 1 < argc) {
            extra_dict = argv[++i];
        } else if (strcmp(argv[i], "--query") == 0 && i + 1 < argc) {
            dict_init(extra_dict);
            return query_test(argv[i + 1]);
        } else if (strcmp(argv[i], "--render-test") == 0 && i + 1 < argc) {
            render_path = argv[++i];
            if (i + 1 < argc && argv[i + 1][0] != '-')
                render_input = argv[++i];
        } else if (strcmp(argv[i], "--check") == 0) {
            check_only = 1;
        } else {
            fprintf(stderr, "xime: 未知参数 %s\n", argv[i]);
            usage(argv[0]);
            return 2;
        }
    }
    if (extra_dict)
        dict_init(extra_dict);

    if (render_path)
        return render_test(render_path, render_input);

    st.display = wl_display_connect(NULL);
    if (!st.display) {
        fprintf(stderr, "xime: 无法连接 Wayland 合成器（检查 WAYLAND_DISPLAY / XDG_RUNTIME_DIR）\n");
        return 1;
    }
    st.registry = wl_display_get_registry(st.display);
    wl_registry_add_listener(st.registry, &registry_listener, &st);
    wl_display_roundtrip(st.display);

    if (check_only) {
        printf("输入法所需协议检查：\n");
        printf("  wl_compositor        : %s\n", st.compositor ? "可用" : "缺失");
        printf("  wl_shm               : %s\n", st.shm ? "可用" : "缺失（候选窗将不可用）");
        printf("  input-method-v2      : %s\n", st.have_ime_global ? "可用" : "缺失");
        printf("  virtual-keyboard-v1  : %s\n", st.vk_manager ? "可用" : "缺失（非消费按键无法回注）");
        if (!st.have_ime_global)
            printf("\n合成器没有提供 zwp_input_method_v2。\n"
                   "  KDE    : 需要在 kwinrc 的 [Wayland] 段设置 InputMethod，或用 xime-setup 自动完成\n"
                   "  GNOME  : Mutter 不实现该协议，本方案不支持 GNOME 的原生输入\n"
                   "  wlroots: sway/Hyprland/labwc 默认提供，直接 exec 本程序即可\n");
        return st.have_ime_global ? 0 : 1;
    }

    if (!st.im) {
        fprintf(stderr, "xime: 合成器未提供 zwp_input_method_manager_v2，无法作为输入法运行。\n"
                        "       用 `xime --check` 查看详细原因（GNOME 不支持此协议）。\n");
        return 1;
    }

    lock_fd = take_lock();
    if (lock_fd < 0)
        xime_log("已有实例在运行（锁获取失败），继续启动可能与前者冲突");

    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);

    fprintf(stderr, "xime: 就绪（Shift 切换中英）\n");

    while (st.running && !g_stop) {
        if (wl_display_dispatch(st.display) < 0) {
            if (errno == EINTR)
                continue;
            fprintf(stderr, "xime: 与合成器的连接断开\n");
            break;
        }
    }

    if (st.grab) {
        zwp_input_method_keyboard_grab_v2_release(st.grab);
        st.grab = NULL;
    }
    xime_popup_role_destroy(&st);
    if (st.vk)
        zwp_virtual_keyboard_v1_destroy(st.vk);
    if (st.im)
        zwp_input_method_v2_destroy(st.im);
    if (st.xkb_state)
        xkb_state_unref(st.xkb_state);
    if (st.xkb_keymap)
        xkb_keymap_unref(st.xkb_keymap);
    if (st.xkb_ctx)
        xkb_context_unref(st.xkb_ctx);
    if (lock_fd >= 0)
        close(lock_fd);
    wl_display_disconnect(st.display);
    return 0;
}
