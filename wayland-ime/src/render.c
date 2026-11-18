#include "render.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

#include "font_atlas.h"
#include "utf8.h"

uint32_t xime_argb(uint8_t a, uint8_t r, uint8_t g, uint8_t b)
{
    /* 预乘 */
    unsigned ar = (unsigned)r * a / 255;
    unsigned ag = (unsigned)g * a / 255;
    unsigned ab = (unsigned)b * a / 255;

    return ((uint32_t)a << 24) | (ar << 16) | (ag << 8) | ab;
}

static inline void blend_px(uint32_t *dst, uint32_t src_pm, unsigned ga)
{
    unsigned sa = (((src_pm >> 24) & 0xFF) * ga) / 255;
    unsigned sr = (((src_pm >> 16) & 0xFF) * ga) / 255;
    unsigned sg = (((src_pm >> 8) & 0xFF) * ga) / 255;
    unsigned sb = ((src_pm & 0xFF) * ga) / 255;
    unsigned ia = 255 - sa;
    unsigned dr = (*dst >> 16) & 0xFF;
    unsigned dg = (*dst >> 8) & 0xFF;
    unsigned db = *dst & 0xFF;
    unsigned r = sr + dr * ia / 255;
    unsigned g = sg + dg * ia / 255;
    unsigned b = sb + db * ia / 255;

    *dst = ((sa > 255 ? 255 : sa) << 24) | (r << 16) | (g << 8) | b;
}

void canvas_clear(struct xime_canvas *c, uint32_t color)
{
    int i, n = c->w * c->h;

    for (i = 0; i < n; i++)
        c->px[i] = color;
}

void canvas_rect_alpha(struct xime_canvas *c, int x, int y, int w, int h, uint32_t color)
{
    int yy, xx;

    for (yy = y; yy < y + h; yy++) {
        if (yy < 0 || yy >= c->h)
            continue;
        for (xx = x; xx < x + w; xx++) {
            if (xx < 0 || xx >= c->w)
                continue;
            blend_px(&c->px[yy * c->w + xx], color, 255);
        }
    }
}

void canvas_round_rect(struct xime_canvas *c, int x, int y, int w, int h, int r, uint32_t color)
{
    int yy;

    if (r <= 0 || w <= 0 || h <= 0) {
        canvas_rect_alpha(c, x, y, w, h, color);
        return;
    }
    if (r * 2 > h)
        r = h / 2;
    if (r * 2 > w)
        r = w / 2;

    for (yy = 0; yy < h; yy++) {
        int inset = 0;
        int dy;
        int row = y + yy;

        if (row < 0 || row >= c->h)
            continue;
        if (yy < r)
            dy = r - yy - 1;
        else if (yy >= h - r)
            dy = yy - (h - r);
        else
            dy = 0;
        if (dy > 0) {
            double d = (double)r * r - (double)dy * dy;
            int dx = (d > 0) ? (int)(r - sqrt(d)) : r;
            inset = dx;
        }
        {
            int xx;

            for (xx = x + inset; xx < x + w - inset; xx++) {
                if (xx < 0 || xx >= c->w)
                    continue;
                blend_px(&c->px[row * c->w + xx], color, 255);
            }
        }
    }
}

static const struct xime_glyph *glyph_lookup(uint32_t cp)
{
    int lo = 0, hi = xime_glyph_count - 1;

    while (lo <= hi) {
        int mid = (lo + hi) / 2;
        if (xime_glyphs[mid].cp == cp)
            return &xime_glyphs[mid];
        if (xime_glyphs[mid].cp < cp)
            lo = mid + 1;
        else
            hi = mid - 1;
    }
    return NULL;
}

int xime_line_height(void)
{
    return xime_cell_h;
}

static unsigned int glyph_advance(uint32_t cp)
{
    const struct xime_glyph *g = glyph_lookup(cp);

    if (g)
        return g->adv;
    if (cp == ' ')
        return (unsigned int)(xime_font_px / 2);
    return (unsigned int)(xime_font_px / 2 + 2);
}

unsigned int canvas_text_width(const char *s)
{
    unsigned int w = 0;
    uint32_t cp;
    int l;

    while ((l = utf8_decode(s, &cp)) > 0) {
        w += glyph_advance(cp);
        s += l;
    }
    return w;
}

void canvas_text(struct xime_canvas *c, int x, int y_top, const char *s, uint32_t color)
{
    uint32_t cp;
    int l, cursor = x;

    while ((l = utf8_decode(s, &cp)) > 0) {
        const struct xime_glyph *g = glyph_lookup(cp);

        if (g) {
            int cx = g->cell % xime_atlas_cols;
            int cy = g->cell / xime_atlas_cols;
            const unsigned char *src = xime_font_atlas + (size_t)(cy * xime_cell_h) * xime_atlas_cols * xime_cell_w
                                       + (size_t)(cx * xime_cell_w);
            int gy, gx;

            for (gy = 0; gy < xime_cell_h; gy++) {
                int py = y_top + gy;

                if (py < 0 || py >= c->h)
                    continue;
                for (gx = 0; gx < xime_cell_w; gx++) {
                    unsigned a = src[(size_t)gy * xime_atlas_cols * xime_cell_w + gx];
                    int px = cursor + gx;

                    if (!a || px < 0 || px >= c->w)
                        continue;
                    blend_px(&c->px[py * c->w + px], color, a);
                }
            }
        } else if (cp != ' ') {
            /* 缺字：画一个空心方块占位，方便一眼看出图集漏字 */
            canvas_rect_alpha(c, cursor + 1, y_top + xime_baseline - xime_font_px + 1, xime_font_px - 2, 2,
                              color);
            canvas_rect_alpha(c, cursor + 1, y_top + xime_baseline - 1, xime_font_px - 2, 2, color);
        }
        cursor += (int)glyph_advance(cp);
        s += l;
    }
}

int canvas_write_bmp(const struct xime_canvas *c, const char *path)
{
    FILE *f = fopen(path, "wb");
    int row_size = (c->w * 3 + 3) & ~3;
    int img_size = row_size * c->h;
    unsigned char header[54];
    int x, y;

    if (!f)
        return -1;
    memset(header, 0, sizeof(header));
    header[0] = 'B';
    header[1] = 'M';
    {
        unsigned int fsize = 54u + (unsigned)img_size;
        memcpy(header + 2, &fsize, 4);
    }
    {
        unsigned int off = 54, hsz = 40, planes = 1, bpp = 24;
        int w = c->w, h = c->h;
        memcpy(header + 10, &off, 4);
        memcpy(header + 14, &hsz, 4);
        memcpy(header + 18, &w, 4);
        memcpy(header + 22, &h, 4);
        memcpy(header + 26, &planes, 2);
        memcpy(header + 28, &bpp, 2);
        memcpy(header + 34, &img_size, 4);
    }
    if (fwrite(header, 1, sizeof(header), f) != sizeof(header)) {
        fclose(f);
        return -1;
    }
    for (y = c->h - 1; y >= 0; y--) {
        int pad = row_size - c->w * 3;
        for (x = 0; x < c->w; x++) {
            uint32_t p = c->px[y * c->w + x];
            unsigned char bgr[3];

            bgr[0] = (unsigned char)(p & 0xFF);
            bgr[1] = (unsigned char)((p >> 8) & 0xFF);
            bgr[2] = (unsigned char)((p >> 16) & 0xFF);
            fwrite(bgr, 1, 3, f);
        }
        while (pad-- > 0)
            fputc(0, f);
    }
    fclose(f);
    return 0;
}
