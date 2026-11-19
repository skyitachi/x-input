// uinput-inject —— 用 /dev/uinput 向合成器注入输入，用于无人值守的端到端测试。
//
// 为什么不用 wtype：wtype 上传的 xkb keymap 只声明了零星几个键，
// 输入法解析后回退到内置 US 布局，keycode 对不上，`nihao` 会变成 `12345`。
// uinput 虚拟设备走合成器的正常 XKB，keymap 完整。
//
// 前提：当前用户需在 input 组，且 /dev/uinput 为 root:input 0660
//       （Arch: 写 /etc/udev/rules.d/60-uinput.rules，见 docs）
//
// 编译: cc -O2 -o uinput-inject uinput-inject.c
// 用法:
//   uinput-inject move <dx> <dy>      相对移动光标
//   uinput-inject click [dx] [dy]     可选先移动，再左键单击
//   uinput-inject type <text>         逐字符输入（a-z A-Z 0-9 空格 回车 退格 . , = -）
//   uinput-inject combo ctrl space    组合键（ctrl/shift/alt/super，键可为字母或 space）
//   uinput-inject tap shift           单击一个命名键（shift/ctrl/alt/space/esc/字母）
//
// 设备创建后固定等 2 秒再发事件：libinput 需要时间枚举并打开新节点，
// 太早写入事件会被丢弃。
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <linux/uinput.h>
#include <linux/input-event-codes.h>
#include <sys/ioctl.h>

static int emit(int fd, unsigned type, unsigned code, int value) {
    struct input_event ev;
    memset(&ev, 0, sizeof ev);
    ev.type = type; ev.code = code; ev.value = value;
    return write(fd, &ev, sizeof ev) == sizeof ev ? 0 : -1;
}

static int keycode_of(char c) {
    if (c >= 'a' && c <= 'z') {
        static const int t[] = {
            KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I,
            KEY_J, KEY_K, KEY_L, KEY_M, KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R,
            KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z };
        return t[c - 'a'];
    }
    if (c >= 'A' && c <= 'Z') return keycode_of((char)(c - 'A' + 'a'));
    if (c >= '0' && c <= '9') {
        // 注意顺序按 '1'..'9','0' 排布，索引用 c - '1'，否则整体偏移一位
        static const int t[] = { KEY_1, KEY_2, KEY_3, KEY_4, KEY_5,
                                 KEY_6, KEY_7, KEY_8, KEY_9 };
        return c == '0' ? KEY_0 : t[c - '1'];
    }
    switch (c) {
    case ' ':  return KEY_SPACE;
    case '\n': return KEY_ENTER;
    case '\b': return KEY_BACKSPACE;
    case '.':  return KEY_DOT;
    case ',':  return KEY_COMMA;
    case '=':  return KEY_EQUAL;
    case '-':  return KEY_MINUS;
    case '/':  return KEY_SLASH;
    case ';':  return KEY_SEMICOLON;
    case '[':  return KEY_LEFTBRACE;
    case ']':  return KEY_RIGHTBRACE;
    case '\\': return KEY_BACKSLASH;
    default:   return -1;
    }
}

/// 创建虚拟设备，返回 fd；失败返回 -1
static int create_dev(const char *name, int is_pointer) {
    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0) { perror("open /dev/uinput"); return -1; }

    if (is_pointer) {
        ioctl(fd, UI_SET_EVBIT, EV_KEY);
        ioctl(fd, UI_SET_KEYBIT, BTN_LEFT);
        ioctl(fd, UI_SET_EVBIT, EV_REL);
        ioctl(fd, UI_SET_RELBIT, REL_X);
        ioctl(fd, UI_SET_RELBIT, REL_Y);
    } else {
        ioctl(fd, UI_SET_EVBIT, EV_KEY);
        for (int c = 0; c < 256; c++) {
            int kc = keycode_of((char)c);
            if (kc >= 0) ioctl(fd, UI_SET_KEYBIT, kc);
        }
        // 修饰键不在 keycode_of 的字符表里，单独开位
        ioctl(fd, UI_SET_KEYBIT, KEY_LEFTCTRL);
        ioctl(fd, UI_SET_KEYBIT, KEY_LEFTSHIFT);
        ioctl(fd, UI_SET_KEYBIT, KEY_LEFTALT);
        ioctl(fd, UI_SET_KEYBIT, KEY_LEFTMETA);
    }

    struct uinput_setup us;
    memset(&us, 0, sizeof us);
    us.id.bustype = BUS_USB;
    us.id.vendor = 0x1234;
    us.id.product = is_pointer ? 0x5679 : 0x567A;
    strncpy(us.name, name, UINPUT_MAX_NAME_SIZE - 1);

    if (ioctl(fd, UI_DEV_SETUP, &us) < 0) { perror("UI_DEV_SETUP"); close(fd); return -1; }
    if (ioctl(fd, UI_DEV_CREATE) < 0) { perror("UI_DEV_CREATE"); close(fd); return -1; }
    return fd;
}

static void destroy_dev(int fd) {
    ioctl(fd, UI_DEV_DESTROY);
    close(fd);
}

static void move_by(int fd, int dx, int dy) {
    if (dx) emit(fd, EV_REL, REL_X, dx);
    if (dy) emit(fd, EV_REL, REL_Y, dy);
    if (dx || dy) emit(fd, EV_SYN, SYN_REPORT, 0);
    usleep(250000);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
            "用法:\n"
            "  uinput-inject move <dx> <dy>\n"
            "  uinput-inject click [dx] [dy]\n"
            "  uinput-inject type <text>\n"
            "  uinput-inject combo <mods> <key>\n"
            "  uinput-inject tap <key>\n");
        return 2;
    }
    const char *cmd = argv[1];

    if (strcmp(cmd, "move") == 0) {
        if (argc < 4) { fprintf(stderr, "move 需要 dx dy\n"); return 2; }
        int fd = create_dev("xime-inject-mouse", 1);
        if (fd < 0) return 1;
        sleep(2);
        move_by(fd, atoi(argv[2]), atoi(argv[3]));
        destroy_dev(fd);
        printf("移动 %s,%s\n", argv[2], argv[3]);
        return 0;
    }

    if (strcmp(cmd, "click") == 0) {
        int dx = argc > 2 ? atoi(argv[2]) : 0;
        int dy = argc > 3 ? atoi(argv[3]) : 0;
        int fd = create_dev("xime-inject-mouse", 1);
        if (fd < 0) return 1;
        sleep(2);
        move_by(fd, dx, dy);
        emit(fd, EV_KEY, BTN_LEFT, 1);
        emit(fd, EV_SYN, SYN_REPORT, 0);
        usleep(60000);
        emit(fd, EV_KEY, BTN_LEFT, 0);
        emit(fd, EV_SYN, SYN_REPORT, 0);
        usleep(120000);
        destroy_dev(fd);
        printf("点击完成\n");
        return 0;
    }

    if (strcmp(cmd, "type") == 0) {
        if (argc < 3) { fprintf(stderr, "type 需要文本\n"); return 2; }
        const char *text = argv[2];
        int fd = create_dev("xime-inject-kbd", 0);
        if (fd < 0) return 1;
        sleep(2);
        for (const char *p = text; *p; p++) {
            int kc = keycode_of(*p);
            if (kc < 0) continue;
            emit(fd, EV_KEY, kc, 1);
            emit(fd, EV_SYN, SYN_REPORT, 0);
            usleep(35000);
            emit(fd, EV_KEY, kc, 0);
            emit(fd, EV_SYN, SYN_REPORT, 0);
            usleep(35000);
        }
        destroy_dev(fd);
        printf("已输入 %zu 个字符\n", strlen(text));
        return 0;
    }

    if (strcmp(cmd, "tap") == 0) {
        // tap shift / tap space / tap a —— 单击一个命名键（按下+松开）
        if (argc < 3) { fprintf(stderr, "tap 需要一个键名\n"); return 2; }
        int kc;
        if (!strcasecmp(argv[2], "shift"))      kc = KEY_LEFTSHIFT;
        else if (!strcasecmp(argv[2], "ctrl"))  kc = KEY_LEFTCTRL;
        else if (!strcasecmp(argv[2], "alt"))   kc = KEY_LEFTALT;
        else if (!strcasecmp(argv[2], "space")) kc = KEY_SPACE;
        else if (!strcasecmp(argv[2], "esc"))   kc = KEY_ESC;
        else kc = keycode_of(argv[2][0]);
        if (kc < 0) { fprintf(stderr, "未知键 %s\n", argv[2]); return 2; }

        int fd = create_dev("xime-inject-kbd", 0);
        if (fd < 0) return 1;
        sleep(2);
        emit(fd, EV_KEY, kc, 1);
        emit(fd, EV_SYN, SYN_REPORT, 0);
        usleep(45000);
        emit(fd, EV_KEY, kc, 0);
        emit(fd, EV_SYN, SYN_REPORT, 0);
        usleep(45000);
        destroy_dev(fd);
        printf("已单击 %s\n", argv[2]);
        return 0;
    }

    if (strcmp(cmd, "combo") == 0) {
        // combo ctrl space / combo ctrl+shift f
        if (argc < 4) { fprintf(stderr, "combo 需要 <修饰键> <键>\n"); return 2; }
        int mods[4], nmod = 0;
        char mbuf[64];
        strncpy(mbuf, argv[2], sizeof mbuf - 1);
        mbuf[sizeof mbuf - 1] = 0;
        for (char *t = strtok(mbuf, "+"); t && nmod < 4; t = strtok(NULL, "+")) {
            if (!strcasecmp(t, "ctrl"))       mods[nmod++] = KEY_LEFTCTRL;
            else if (!strcasecmp(t, "shift")) mods[nmod++] = KEY_LEFTSHIFT;
            else if (!strcasecmp(t, "alt"))   mods[nmod++] = KEY_LEFTALT;
            else if (!strcasecmp(t, "super")) mods[nmod++] = KEY_LEFTMETA;
            else { fprintf(stderr, "未知修饰键 %s\n", t); return 2; }
        }
        int kc = strcasecmp(argv[3], "space") == 0 ? KEY_SPACE : keycode_of(argv[3][0]);
        if (kc < 0) { fprintf(stderr, "未知键 %s\n", argv[3]); return 2; }

        int fd = create_dev("xime-inject-kbd", 0);
        if (fd < 0) return 1;
        sleep(2);
        for (int i = 0; i < nmod; i++) {
            emit(fd, EV_KEY, mods[i], 1);
            emit(fd, EV_SYN, SYN_REPORT, 0);
            usleep(20000);
        }
        emit(fd, EV_KEY, kc, 1);
        emit(fd, EV_SYN, SYN_REPORT, 0);
        usleep(40000);
        emit(fd, EV_KEY, kc, 0);
        emit(fd, EV_SYN, SYN_REPORT, 0);
        usleep(20000);
        for (int i = nmod - 1; i >= 0; i--) {
            emit(fd, EV_KEY, mods[i], 0);
            emit(fd, EV_SYN, SYN_REPORT, 0);
            usleep(20000);
        }
        destroy_dev(fd);
        printf("已发送 %s+%s\n", argv[2], argv[3]);
        return 0;
    }

    fprintf(stderr, "未知子命令: %s\n", cmd);
    return 2;
}
