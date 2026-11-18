/* mock_compositor - 协议级集成测试
 *
 * 用 libwayland-server 起一个最小合成器，只实现 xime 需要的全局对象：
 *   wl_compositor / wl_shm / wl_seat / zwp_input_method_manager_v2 / zwp_virtual_keyboard_manager_v1
 * 然后按真实合成器的时序驱动一遍：activate -> grab -> keymap -> 按键 -> 期待 commit_string。
 *
 * 这样在没有真实 Wayland 会话的机器上也能验证协议往返是否 100% 正确。
 */

#include <linux/input-event-codes.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <wayland-server.h>
#include <xkbcommon/xkbcommon.h>

#include "input-method-v2-server.h"
#include "virtual-keyboard-server.h"

static struct {
    struct wl_display *display;
    struct wl_resource *im;
    struct wl_resource *grab;
    struct wl_resource *vk;

    int saw_popup_surface;
    int saw_grab_request;
    int saw_vk_keymap;
    int saw_vk_key;
    uint32_t vk_key_code;

    int preedit_events;
    char last_preedit[256];
    char committed[512];
    int commit_count;
    uint32_t last_serial;

    int sent_activate, sent_keymap, sent_keys, sent_esc;
} T;

/* ---------- wl_surface / wl_region / wl_shm 的最小桩 ---------- */

static void res_destroy(struct wl_client *c, struct wl_resource *r)
{
    (void)c;
    wl_resource_destroy(r);
}

static void surf_attach(struct wl_client *c, struct wl_resource *r, struct wl_resource *buffer, int32_t x,
                        int32_t y)
{
    (void)c;
    (void)r;
    (void)buffer;
    (void)x;
    (void)y;
}

static void surf_damage(struct wl_client *c, struct wl_resource *r, int32_t x, int32_t y, int32_t w, int32_t h)
{
    (void)c;
    (void)r;
    (void)x;
    (void)y;
    (void)w;
    (void)h;
}

static void surf_commit(struct wl_client *c, struct wl_resource *r)
{
    (void)c;
    (void)r;
}

static void surf_set_input_region(struct wl_client *c, struct wl_resource *r, struct wl_resource *region)
{
    (void)c;
    (void)r;
    (void)region;
}

static void surf_set_buffer_scale(struct wl_client *c, struct wl_resource *r, int32_t scale)
{
    (void)c;
    (void)r;
    (void)scale;
}

static const struct wl_surface_interface surface_impl = {
    .destroy = res_destroy,
    .attach = surf_attach,
    .damage = surf_damage,
    .frame = (void *)0,
    .set_opaque_region = surf_set_input_region,
    .set_input_region = surf_set_input_region,
    .commit = surf_commit,
    .set_buffer_transform = (void *)0,
    .set_buffer_scale = surf_set_buffer_scale,
    .damage_buffer = surf_damage,
};

static void region_add(struct wl_client *c, struct wl_resource *r, int32_t x, int32_t y, int32_t w, int32_t h)
{
    (void)c;
    (void)r;
    (void)x;
    (void)y;
    (void)w;
    (void)h;
}

static const struct wl_region_interface region_impl = {
    .destroy = res_destroy,
    .add = region_add,
    .subtract = region_add,
};

static void buffer_impl_destroy(struct wl_client *c, struct wl_resource *r)
{
    (void)c;
    wl_resource_destroy(r);
}

static const struct wl_buffer_interface buffer_impl = {
    .destroy = buffer_impl_destroy,
};

static void pool_create_buffer(struct wl_client *c, struct wl_resource *r, uint32_t id, int32_t offset,
                               int32_t w, int32_t h, int32_t stride, uint32_t format)
{
    struct wl_resource *buf;

    (void)r;
    (void)offset;
    (void)w;
    (void)h;
    (void)stride;
    (void)format;
    buf = wl_resource_create(c, &wl_buffer_interface, 1, id);
    if (buf)
        wl_resource_set_implementation(buf, &buffer_impl, NULL, NULL);
}

static void pool_resize(struct wl_client *c, struct wl_resource *r, int32_t size)
{
    (void)c;
    (void)r;
    (void)size;
}

static const struct wl_shm_pool_interface pool_impl = {
    .create_buffer = pool_create_buffer,
    .destroy = res_destroy,
    .resize = pool_resize,
};

static void shm_create_pool(struct wl_client *c, struct wl_resource *r, uint32_t id, int32_t fd, int32_t size)
{
    struct wl_resource *pool;

    (void)size;
    close(fd);
    pool = wl_resource_create(c, &wl_shm_pool_interface, wl_resource_get_version(r), id);
    if (pool)
        wl_resource_set_implementation(pool, &pool_impl, NULL, NULL);
}

static const struct wl_shm_interface shm_impl = {
    .create_pool = shm_create_pool,
};

static void bind_shm(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
    struct wl_resource *r = wl_resource_create(c, &wl_shm_interface, version, id);

    (void)data;
    wl_resource_set_implementation(r, &shm_impl, NULL, NULL);
    wl_shm_send_format(r, WL_SHM_FORMAT_ARGB8888);
}

static void compositor_create_surface(struct wl_client *c, struct wl_resource *r, uint32_t id)
{
    struct wl_resource *s = wl_resource_create(c, &wl_surface_interface, wl_resource_get_version(r), id);

    if (s)
        wl_resource_set_implementation(s, &surface_impl, NULL, NULL);
}

static void compositor_create_region(struct wl_client *c, struct wl_resource *r, uint32_t id)
{
    struct wl_resource *reg = wl_resource_create(c, &wl_region_interface, wl_resource_get_version(r), id);

    if (reg)
        wl_resource_set_implementation(reg, &region_impl, NULL, NULL);
}

static const struct wl_compositor_interface compositor_impl = {
    .create_surface = compositor_create_surface,
    .create_region = compositor_create_region,
};

static void bind_compositor(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
    struct wl_resource *r = wl_resource_create(c, &wl_compositor_interface, version, id);

    (void)data;
    wl_resource_set_implementation(r, &compositor_impl, NULL, NULL);
}

/* ---------- wl_seat ---------- */

static void seat_get_pointer(struct wl_client *c, struct wl_resource *r, uint32_t id)
{
    (void)c;
    (void)r;
    (void)id;
}

static void seat_get_keyboard(struct wl_client *c, struct wl_resource *r, uint32_t id)
{
    (void)c;
    (void)r;
    (void)id;
}

static void seat_get_touch(struct wl_client *c, struct wl_resource *r, uint32_t id)
{
    (void)c;
    (void)r;
    (void)id;
}

static void seat_release(struct wl_client *c, struct wl_resource *r)
{
    (void)c;
    wl_resource_destroy(r);
}

static const struct wl_seat_interface seat_impl = {
    .get_pointer = seat_get_pointer,
    .get_keyboard = seat_get_keyboard,
    .get_touch = seat_get_touch,
    .release = seat_release,
};

static void bind_seat(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
    struct wl_resource *r = wl_resource_create(c, &wl_seat_interface, version, id);

    (void)data;
    wl_resource_set_implementation(r, &seat_impl, NULL, NULL);
    wl_seat_send_capabilities(r, WL_SEAT_CAPABILITY_KEYBOARD);
    if (version >= 2)
        wl_seat_send_name(r, "seat0");
}

/* ---------- zwp_input_method_v2（服务端） ---------- */

static void im_commit_string(struct wl_client *c, struct wl_resource *r, const char *text)
{
    (void)c;
    (void)r;
    strncat(T.committed, text, sizeof(T.committed) - strlen(T.committed) - 1);
    printf("  [im] commit_string(\"%s\")\n", text);
}

static void im_set_preedit(struct wl_client *c, struct wl_resource *r, const char *text, int32_t cb,
                           int32_t ce)
{
    (void)c;
    (void)r;
    (void)cb;
    (void)ce;
    T.preedit_events++;
    snprintf(T.last_preedit, sizeof(T.last_preedit), "%s", text);
    printf("  [im] set_preedit_string(\"%s\")\n", text);
}

static void im_delete_surrounding(struct wl_client *c, struct wl_resource *r, uint32_t before,
                                  uint32_t after)
{
    (void)c;
    (void)r;
    (void)before;
    (void)after;
}

static void im_commit(struct wl_client *c, struct wl_resource *r, uint32_t serial)
{
    (void)c;
    (void)r;
    T.commit_count++;
    T.last_serial = serial;
}

static void im_get_popup(struct wl_client *c, struct wl_resource *r, uint32_t id, struct wl_resource *surface)
{
    struct wl_resource *popup;

    (void)r;
    (void)surface;
    popup = wl_resource_create(c, &zwp_input_popup_surface_v2_interface, 1, id);
    if (popup)
        wl_resource_set_implementation(popup, NULL, NULL, NULL);
    T.saw_popup_surface = 1;
    printf("  [im] get_input_popup_surface\n");
}

static void im_grab_keyboard(struct wl_client *c, struct wl_resource *r, uint32_t id)
{
    (void)r;
    T.grab = wl_resource_create(c, &zwp_input_method_keyboard_grab_v2_interface, 1, id);
    if (T.grab)
        wl_resource_set_implementation(T.grab, NULL, NULL, NULL);
    T.saw_grab_request = 1;
    printf("  [im] grab_keyboard\n");
}

static void im_destroy(struct wl_client *c, struct wl_resource *r)
{
    (void)c;
    wl_resource_destroy(r);
}

static const struct zwp_input_method_v2_interface im_impl = {
    .commit_string = im_commit_string,
    .set_preedit_string = im_set_preedit,
    .delete_surrounding_text = im_delete_surrounding,
    .commit = im_commit,
    .get_input_popup_surface = im_get_popup,
    .grab_keyboard = im_grab_keyboard,
    .destroy = im_destroy,
};

static void manager_get_im(struct wl_client *c, struct wl_resource *r, struct wl_resource *seat, uint32_t id)
{
    (void)r;
    (void)seat;
    T.im = wl_resource_create(c, &zwp_input_method_v2_interface, 1, id);
    if (T.im)
        wl_resource_set_implementation(T.im, &im_impl, NULL, NULL);
    printf("  [im] get_input_method -> input_method_v2\n");
}

static void manager_destroy(struct wl_client *c, struct wl_resource *r)
{
    (void)c;
    wl_resource_destroy(r);
}

static const struct zwp_input_method_manager_v2_interface im_manager_impl = {
    .get_input_method = manager_get_im,
    .destroy = manager_destroy,
};

static void bind_im_manager(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
    struct wl_resource *r = wl_resource_create(c, &zwp_input_method_manager_v2_interface, version, id);

    (void)data;
    wl_resource_set_implementation(r, &im_manager_impl, NULL, NULL);
}

/* ---------- zwp_virtual_keyboard_v1（服务端） ---------- */

static void vk_keymap(struct wl_client *c, struct wl_resource *r, uint32_t format, int32_t fd, uint32_t size)
{
    (void)c;
    (void)r;
    (void)format;
    (void)size;
    close(fd);
    T.saw_vk_keymap = 1;
    printf("  [vk] keymap（合成器收到，可回注按键）\n");
}

static void vk_key(struct wl_client *c, struct wl_resource *r, uint32_t time, uint32_t key, uint32_t state)
{
    (void)c;
    (void)r;
    (void)time;
    T.saw_vk_key = 1;
    T.vk_key_code = key;
    printf("  [vk] key(code=%u, state=%u)\n", key, state);
}

static void vk_modifiers(struct wl_client *c, struct wl_resource *r, uint32_t d, uint32_t l, uint32_t lo,
                         uint32_t g)
{
    (void)c;
    (void)r;
    (void)d;
    (void)l;
    (void)lo;
    (void)g;
}

static const struct zwp_virtual_keyboard_v1_interface vk_impl = {
    .keymap = vk_keymap,
    .key = vk_key,
    .modifiers = vk_modifiers,
    .destroy = im_destroy,
};

static void vk_manager_create(struct wl_client *c, struct wl_resource *r, struct wl_resource *seat, uint32_t id)
{
    (void)r;
    (void)seat;
    T.vk = wl_resource_create(c, &zwp_virtual_keyboard_v1_interface, 1, id);
    if (T.vk)
        wl_resource_set_implementation(T.vk, &vk_impl, NULL, NULL);
    printf("  [vk] create_virtual_keyboard\n");
}

static const struct zwp_virtual_keyboard_manager_v1_interface vk_manager_impl = {
    .create_virtual_keyboard = vk_manager_create,
};

static void bind_vk_manager(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
    struct wl_resource *r = wl_resource_create(c, &zwp_virtual_keyboard_manager_v1_interface, version, id);

    (void)data;
    wl_resource_set_implementation(r, &vk_manager_impl, NULL, NULL);
}

/* ---------- 驱动 ---------- */

static char *make_us_keymap(uint32_t *out_size)
{
    struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_rule_names names = {.rules = NULL, .model = NULL, .layout = "us", .variant = NULL,
                                   .options = NULL};
    struct xkb_keymap *km = xkb_keymap_new_from_names(ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    char *str = xkb_keymap_get_as_string(km, XKB_KEYMAP_FORMAT_TEXT_V1);

    *out_size = (uint32_t)strlen(str) + 1;
    xkb_keymap_unref(km);
    xkb_context_unref(ctx);
    return str;
}

static void send_keymap(void)
{
    uint32_t size = 0;
    char *map = make_us_keymap(&size);
    int fd = memfd_create("keymap", MFD_CLOEXEC);

    if (fd >= 0 && map) {
        if (write(fd, map, size) != (ssize_t)size) {
            close(fd);
            fd = -1;
        } else {
            lseek(fd, 0, SEEK_SET);
            zwp_input_method_keyboard_grab_v2_send_keymap(T.grab, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, size);
            close(fd);
        }
    }
    zwp_input_method_keyboard_grab_v2_send_repeat_info(T.grab, 25, 600);
    free(map);
}

static void send_key(uint32_t code, uint32_t state)
{
    zwp_input_method_keyboard_grab_v2_send_key(T.grab, 0, 100, code, state);
}

/* 真实合成器在修饰键按下/松开后会补一个 modifiers 事件；只发 key 事件的话，
 * 输入法算出来的 keysym 永远是小写，Shift 组合测试会假失败。 */
static void send_modifiers(uint32_t depressed, uint32_t latched, uint32_t locked, uint32_t group)
{
    zwp_input_method_keyboard_grab_v2_send_modifiers(T.grab, 101, depressed, latched, locked, group);
}

static void send_char(uint32_t evdev)
{
    send_key(evdev, WL_KEYBOARD_KEY_STATE_PRESSED);
    send_key(evdev, WL_KEYBOARD_KEY_STATE_RELEASED);
}

/* ---- 可替换的按键序列 ------------------------------------------------------
 * 默认场景固定是「nihao + 空格 -> 你好」。设 MOCK_KEYS 可以换成别的序列，
 * 用来测同一套协议下的其它输入路径（例如英文模式的前缀补全）：
 *
 *   MOCK_KEYS="hel space" MOCK_EXPECT="help" XIME_ARGS="--en" ./mock_compositor
 *
 * 写法：小写字母＝该键，大写字母＝Shift+该键，另支持 space / enter / esc / shift / backspace。
 * MOCK_EXPECT 是期望的 commit_string，默认「你好」。
 */
static uint32_t g_seq[64];
static uint8_t g_seq_shift[64];
static size_t g_seq_n;
static int g_custom;
static int g_need_esc;
static const char *g_expect = "你好";

static uint32_t letter_code(char ch)
{
    static const uint32_t tab[26] = {
        KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I,
        KEY_J, KEY_K, KEY_L, KEY_M, KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R,
        KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z,
    };
    if (ch >= 'a' && ch <= 'z')
        return tab[ch - 'a'];
    if (ch >= 'A' && ch <= 'Z')
        return tab[ch - 'A'];
    return 0;
}

static void push_key(uint32_t code, int shift)
{
    if (g_seq_n >= sizeof(g_seq) / sizeof(g_seq[0]))
        return;
    g_seq[g_seq_n] = code;
    g_seq_shift[g_seq_n] = (uint8_t)shift;
    g_seq_n++;
}

static void parse_keys(const char *s)
{
    char buf[512];
    char *tok;

    snprintf(buf, sizeof(buf), "%s", s);
    for (tok = strtok(buf, " ,"); tok; tok = strtok(NULL, " ,")) {
        if (!strcmp(tok, "space"))
            push_key(KEY_SPACE, 0);
        else if (!strcmp(tok, "enter"))
            push_key(KEY_ENTER, 0);
        else if (!strcmp(tok, "backspace"))
            push_key(KEY_BACKSPACE, 0);
        else if (!strcmp(tok, "esc")) {
            push_key(KEY_ESC, 0);
            g_need_esc = 1;
        } else if (!strcmp(tok, "shift"))
            push_key(KEY_LEFTSHIFT, 0);
        else {
            /* 普通 token 逐字符展开：'hel' 就是 h、e、l 三个键，
             * 连续写一串字母比写 'h e l' 更接近真实打字。 */
            const char *p;

            for (p = tok; *p; p++) {
                uint32_t c = letter_code(*p);

                if (c)
                    push_key(c, *p >= 'A' && *p <= 'Z');
            }
        }
    }
}

static void step(void)
{
    if (T.im && !T.sent_activate) {
        T.sent_activate = 1;
        printf("-> activate + done\n");
        zwp_input_method_v2_send_activate(T.im);
        zwp_input_method_v2_send_done(T.im);
        wl_display_flush_clients(T.display);
        return;
    }
    if (T.grab && !T.sent_keymap) {
        T.sent_keymap = 1;
        printf("-> grab 已建立，发送 keymap\n");
        send_keymap();
        wl_display_flush_clients(T.display);
        return;
    }
    if (T.sent_keymap && !T.sent_keys) {
        size_t i;

        T.sent_keys = 1;
        if (g_custom) {
            printf("-> 键入自定义序列（%zu 键）\n", g_seq_n);
            for (i = 0; i < g_seq_n; i++) {
                if (g_seq_shift[i]) {
                    send_key(KEY_LEFTSHIFT, WL_KEYBOARD_KEY_STATE_PRESSED);
                    send_modifiers(1, 0, 0, 0);
                }
                send_char(g_seq[i]);
                if (g_seq_shift[i]) {
                    send_key(KEY_LEFTSHIFT, WL_KEYBOARD_KEY_STATE_RELEASED);
                    send_modifiers(0, 0, 0, 0);
                }
            }
        } else {
            uint32_t seq[] = {KEY_N, KEY_I, KEY_H, KEY_A, KEY_O, KEY_SPACE};

            printf("-> 键入 n i h a o space\n");
            for (i = 0; i < sizeof(seq) / sizeof(seq[0]); i++)
                send_char(seq[i]);
        }
        wl_display_flush_clients(T.display);
        return;
    }
    if (T.sent_keys && !T.sent_esc && T.committed[0] && (g_need_esc || !g_custom)) {
        T.sent_esc = 1;
        printf("-> 输入串已清空，发送 Esc（应被回注给应用）\n");
        send_char(KEY_ESC);
        wl_display_flush_clients(T.display);
    }
}

int main(void)
{
    struct wl_event_loop *loop;
    pid_t child = -1;
    int i, ok = 1;
    const char *rt = "/tmp/xime-test-rt";

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    setenv("XDG_RUNTIME_DIR", rt, 1);
    mkdir(rt, 0700);

    T.display = wl_display_create();
    if (!T.display) {
        fprintf(stderr, "无法创建 wl_display\n");
        return 2;
    }
    if (wl_display_add_socket(T.display, "xime-test") != 0) {
        fprintf(stderr, "无法监听 wayland socket\n");
        return 2;
    }
    wl_global_create(T.display, &wl_compositor_interface, 4, NULL, bind_compositor);
    wl_global_create(T.display, &wl_shm_interface, 1, NULL, bind_shm);
    wl_global_create(T.display, &wl_seat_interface, 7, NULL, bind_seat);
    wl_global_create(T.display, &zwp_input_method_manager_v2_interface, 1, NULL, bind_im_manager);
    wl_global_create(T.display, &zwp_virtual_keyboard_manager_v1_interface, 1, NULL, bind_vk_manager);

    loop = wl_display_get_event_loop(T.display);

    /* XIME_CLIENT 可指定被测客户端；设为 "none" 时只起服务端，供外部进程连接 */
    const char *keys = getenv("MOCK_KEYS");
    const char *exp = getenv("MOCK_EXPECT");

    if (keys && *keys) {
        g_custom = 1;
        parse_keys(keys);
    }
    if (exp && *exp)
        g_expect = exp;

    const char *client = getenv("XIME_CLIENT");
    if (!client)
        client = "./build/xime";
    if (strcmp(client, "none") != 0) {
        child = fork();
        if (child == 0) {
            char *argv[12];
            char extra[256];
            const char *ea = getenv("XIME_ARGS");
            int argc = 0;

            setenv("WAYLAND_DISPLAY", "xime-test", 1);
            argv[argc++] = (char *)client;
            argv[argc++] = (char *)"--verbose";
            if (ea) {
                char *tok;

                snprintf(extra, sizeof(extra), "%s", ea);
                for (tok = strtok(extra, " "); tok && argc < 11; tok = strtok(NULL, " "))
                    argv[argc++] = tok;
            }
            argv[argc] = NULL;
            execv(client, argv);
            _exit(127);
        }
    }

    for (i = 0; i < 200; i++) {
        wl_event_loop_dispatch(loop, 30);
        wl_display_flush_clients(T.display);
        step();
        if (T.saw_vk_key && T.committed[0])
            break;
    }

    printf("\n=== 断言 ===\n");
    if (!T.saw_popup_surface) {
        printf("FAIL 候选窗 surface 未创建\n");
        ok = 0;
    } else {
        printf("ok   候选窗 surface 已创建\n");
    }
    if (!T.saw_grab_request) {
        printf("FAIL 未请求键盘 grab（收不到按键）\n");
        ok = 0;
    } else {
        printf("ok   键盘 grab 已建立\n");
    }
    if (T.preedit_events < 3) {
        printf("FAIL preedit 更新次数过少（%d）\n", T.preedit_events);
        ok = 0;
    } else {
        printf("ok   preedit 持续更新（%d 次，最后 \"%s\"）\n", T.preedit_events, T.last_preedit);
    }
    if (strcmp(T.committed, g_expect) != 0) {
        printf("FAIL 提交内容应为 \"%s\"，实际 \"%s\"\n", g_expect, T.committed);
        ok = 0;
    } else {
        printf("ok   空格选中首选并提交 \"%s\"\n", g_expect);
    }
    if (T.commit_count < 3) {
        printf("FAIL commit 次数过少（%d）\n", T.commit_count);
        ok = 0;
    } else {
        printf("ok   commit() 调用了 %d 次，最后一次 serial=%u\n", T.commit_count, T.last_serial);
    }
    if (!T.saw_vk_keymap) {
        printf("FAIL 虚拟键盘未收到 keymap\n");
        ok = 0;
    } else {
        printf("ok   虚拟键盘已同步 keymap\n");
    }
    if (!g_need_esc && g_custom) {
        printf("--   自定义序列未含 esc，跳过回注校验\n");
    } else if (!T.saw_vk_key || T.vk_key_code != KEY_ESC) {
        printf("FAIL Esc 未被回注给应用（code=%u）\n", T.vk_key_code);
        ok = 0;
    } else {
        printf("ok   非消费按键（Esc）已通过虚拟键盘回注\n");
    }

    if (child > 0) {
        int waited = 0;

        kill(child, SIGTERM);
        while (waitpid(child, NULL, WNOHANG) == 0 && waited < 30) {
            usleep(20000);
            waited++;
        }
        kill(child, SIGKILL);
        waitpid(child, NULL, WNOHANG);
    }
    wl_display_destroy(T.display);
    printf("\n%s\n", ok ? "全部通过" : "存在失败项");
    return ok ? 0 : 1;
}
