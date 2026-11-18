#ifndef XIME_RENDER_H
#define XIME_RENDER_H

#include <stdint.h>

/* 32 位 ARGB8888 画布，像素为预乘 alpha，可直接送 wl_shm */
struct xime_canvas {
    uint32_t *px;
    int w;
    int h;
};

uint32_t xime_argb(uint8_t a, uint8_t r, uint8_t g, uint8_t b);

void canvas_clear(struct xime_canvas *c, uint32_t color);
void canvas_round_rect(struct xime_canvas *c, int x, int y, int w, int h, int radius, uint32_t color);
void canvas_rect_alpha(struct xime_canvas *c, int x, int y, int w, int h, uint32_t color);

/* 行高（含上下缘） */
int xime_line_height(void);
/* 以 (x, y_top) 为左上角绘制一行 UTF-8 文本 */
void canvas_text(struct xime_canvas *c, int x, int y_top, const char *s, uint32_t color);
unsigned int canvas_text_width(const char *s);

/* 把画布写成 24 位 BMP，用于构建期/调试期肉眼校验渲染结果 */
int canvas_write_bmp(const struct xime_canvas *c, const char *path);

#endif
