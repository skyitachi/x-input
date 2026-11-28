/* mkemoji - 把彩色 emoji 字体烘焙成 ARGB 图集（构建期用，运行时不依赖 freetype）
 *
 * 和 mkfont-ft 的分工：mkfont-ft 产出 8bpp alpha 单色图集给正文用，
 * 这个工具产出 32bpp 彩色图集给 emoji 用。彩色字形没法压成 alpha——
 * NotoColorEmoji 是 CBDT 位图字体，FreeType 以 FT_PIXEL_MODE_BGRA 交回
 * 一整块带颜色的像素，只能原样存下来。
 *
 * 用法: mkemoji <emoji.txt> <emoji.ttf> <out.bin> [px]
 *   emoji.txt 每行第一个字段是一个 emoji 字符（由 scripts/gen-emoji.py 生成）
 *
 * 输出布局（全部小端）:
 *   magic(8) "XIMEEJ01"
 *   6×u32: cell_w, cell_h, cols, baseline, px, glyph_count
 *   glyph_count × (u32 cp, u16 cell, u16 adv)
 *   ncell × cell_w × cell_h × u32  （ARGB8888，已预乘）
 */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <ft2build.h>
#include FT_FREETYPE_H

#define MAX_GLYPHS 8192

/* BMP 范围内的码点用位图去重；BMP 之外（emoji 主体）线性查，量小无所谓 */
static unsigned char g_seen[0x10000 / 8];

/* UTF-8 解码：返回消耗的字节数，cp 写入 *out。非法字节当作 U+FFFD 前进 1 字节。 */
static int u8_decode(const unsigned char *s, unsigned int *out)
{
    if (s[0] < 0x80) {
        *out = s[0];
        return 1;
    }
    if ((s[0] & 0xE0) == 0xC0) {
        *out = ((unsigned int)(s[0] & 0x1F) << 6) | (s[1] & 0x3F);
        return 2;
    }
    if ((s[0] & 0xF0) == 0xE0) {
        *out = ((unsigned int)(s[0] & 0x0F) << 12) | ((unsigned int)(s[1] & 0x3F) << 6) |
               (s[2] & 0x3F);
        return 3;
    }
    if ((s[0] & 0xF8) == 0xF0) {
        *out = ((unsigned int)(s[0] & 0x07) << 18) | ((unsigned int)(s[1] & 0x3F) << 12) |
               ((unsigned int)(s[2] & 0x3F) << 6) | (s[3] & 0x3F);
        return 4;
    }
    *out = 0xFFFD;
    return 1;
}

static int cmp_cp(const void *a, const void *b)
{
    unsigned int x = *(const unsigned int *)a, y = *(const unsigned int *)b;

    return (x > y) - (x < y);
}

/* 双线性缩放一块 BGRA 位图到目标框内，等比、居中。dst 是 cell 的起点 + 内偏移。 */
static void scale_into(const unsigned char *src, int sw, int sh, int spitch,
                       unsigned char *dst, int dpitch, int box, int *used)
{
    int dw, dh, ox, oy, x, y;

    if (sw <= 0 || sh <= 0) {
        *used = 0;
        return;
    }
    /* 等比缩放到 box 以内 */
    if (sw >= sh) {
        dw = box;
        dh = (int)((long)sh * box / sw);
    } else {
        dh = box;
        dw = (int)((long)sw * box / sh);
    }
    if (dw < 1)
        dw = 1;
    if (dh < 1)
        dh = 1;
    ox = (box - dw) / 2;
    oy = (box - dh) / 2;

    for (y = 0; y < dh; y++) {
        double sy = (y + 0.5) * sh / dh - 0.5;
        int y0 = (int)sy;
        double fy = sy - y0;
        int y1;

        if (y0 < 0) {
            y0 = 0;
            fy = 0;
        }
        y1 = y0 + 1;
        if (y1 >= sh) {
            y1 = sh - 1;
            fy = 0;
        }
        for (x = 0; x < dw; x++) {
            double sx = (x + 0.5) * sw / dw - 0.5;
            int x0 = (int)sx;
            double fx = sx - x0;
            int x1, c;

            if (x0 < 0) {
                x0 = 0;
                fx = 0;
            }
            x1 = x0 + 1;
            if (x1 >= sw) {
                x1 = sw - 1;
                fx = 0;
            }
            for (c = 0; c < 4; c++) {
                const unsigned char *r0 = src + (size_t)y0 * spitch;
                const unsigned char *r1 = src + (size_t)y1 * spitch;

                double v00 = r0[x0 * 4 + c], v10 = r0[x1 * 4 + c];
                double v01 = r1[x0 * 4 + c], v11 = r1[x1 * 4 + c];
                double v = v00 * (1 - fx) * (1 - fy) + v10 * fx * (1 - fy) +
                           v01 * (1 - fx) * fy + v11 * fx * fy;

                dst[(size_t)(oy + y) * dpitch + (size_t)(ox + x) * 4 + c] =
                    (unsigned char)(v + 0.5);
            }
        }
    }
    *used = 1;
}

int main(int argc, char **argv)
{
    const char *txt_path, *font_path, *out_path;
    int box = 20;
    FILE *f;
    unsigned int *cps;
    int ncp = 0, cap = 0;
    FT_Library lib;
    FT_Face face;
    int i, strike = -1, cell, cols, rows, ncell;
    unsigned char *atlas;
    unsigned short *cells;
    unsigned short *advs;

    if (argc < 4) {
        fprintf(stderr, "用法: %s <emoji.txt> <emoji.ttf> <out.bin> [px]\n", argv[0]);
        return 2;
    }
    txt_path = argv[1];
    font_path = argv[2];
    out_path = argv[3];
    if (argc > 4) {
        box = atoi(argv[4]);
        if (box < 8 || box > 64)
            box = 20;
    }

    /* 1. 读词表，收集每行第一个字符 */
    f = fopen(txt_path, "rb");
    if (!f) {
        fprintf(stderr, "mkemoji: 打不开 %s\n", txt_path);
        return 1;
    }
    {
        char line[4096];

        cap = 2048;
        cps = malloc((size_t)cap * sizeof(unsigned int));
        while (fgets(line, sizeof(line), f)) {
            const unsigned char *p = (const unsigned char *)line;
            unsigned int cp;

            if (line[0] == '#' || line[0] == '\n')
                continue;
            if (u8_decode(p, &cp) <= 0)
                continue;
            if (cp < 0x10000) {
                unsigned int byte = cp >> 3, bit = 1u << (cp & 7);

                if (g_seen[byte] & bit)
                    continue;
                g_seen[byte] = (unsigned char)(g_seen[byte] | bit);
            } else {
                int k, dup = 0;

                for (k = 0; k < ncp; k++)
                    if (cps[k] == cp) {
                        dup = 1;
                        break;
                    }
                if (dup)
                    continue;
            }
            if (ncp == cap) {
                cap *= 2;
                cps = realloc(cps, (size_t)cap * sizeof(unsigned int));
            }
            cps[ncp++] = cp;
        }
    }
    fclose(f);
    if (ncp == 0) {
        fprintf(stderr, "mkemoji: %s 里没有可用的 emoji\n", txt_path);
        return 1;
    }
    qsort(cps, (size_t)ncp, sizeof(unsigned int), cmp_cp);

    /* 2. 打开字体。emoji 字体是纯位图字体（CBDT/sbix），必须选中一个固定 strike */
    if (FT_Init_FreeType(&lib)) {
        fprintf(stderr, "mkemoji: FreeType 初始化失败\n");
        return 1;
    }
    if (FT_New_Face(lib, font_path, 0, &face)) {
        fprintf(stderr, "mkemoji: 无法打开字体 %s\n", font_path);
        return 1;
    }
    if (face->num_fixed_sizes > 0) {
        int best = 0;

        for (i = 1; i < face->num_fixed_sizes; i++) {
            if (face->available_sizes[i].height > face->available_sizes[best].height)
                best = i;
        }
        strike = best;
        if (FT_Select_Size(face, best)) {
            fprintf(stderr, "mkemoji: 选择位图 strike 失败\n");
            return 1;
        }
        fprintf(stderr, "mkemoji: strike %dx%d\n",
                face->available_sizes[best].width, face->available_sizes[best].height);
    } else {
        fprintf(stderr, "mkemoji: 警告：字体没有位图 strike，可能不是彩色 emoji 字体\n");
    }

    /* 3. 版面。cell 与正文图集对齐（正文 18px 时 cell 高 24），emoji 居中绘制。
     * 基线取 cell 底部附近，advance 固定为一个 cell 宽再加一点间距。 */
    cell = box + 4;
    cols = 64;
    rows = (ncp + cols - 1) / cols;
    ncell = cols * rows;
    atlas = calloc((size_t)ncell * (size_t)cell * (size_t)cell * 4, 1);
    cells = malloc((size_t)ncp * sizeof(unsigned short));
    advs = malloc((size_t)ncp * sizeof(unsigned short));
    if (!atlas || !cells || !advs) {
        fprintf(stderr, "mkemoji: 内存不足\n");
        return 1;
    }

    for (i = 0; i < ncp; i++) {
        unsigned int cp = cps[i];
        FT_GlyphSlot g;
        unsigned char *dst;

        if (!FT_Get_Char_Index(face, cp)) {
            cells[i] = 0xFFFF;
            advs[i] = (unsigned short)cell;
            continue;
        }
        if (FT_Load_Char(face, cp, FT_LOAD_RENDER | FT_LOAD_COLOR)) {
            cells[i] = 0xFFFF;
            advs[i] = (unsigned short)cell;
            continue;
        }
        g = face->glyph;
        cells[i] = (unsigned short)i;
        advs[i] = (unsigned short)cell;

        /* 图集是一整张大图：每行 cols 个格子，所以格内的像素行要跨过整行宽度
         * （cols*cell*4 字节），而不是格子自身的宽度。这里搞错就会把每个
         * emoji 横向铺进右边相邻的格子里。 */
        dst = &atlas[((size_t)(i / cols) * cell) * (size_t)cols * cell * 4 +
                     (size_t)(i % cols) * cell * 4];

        if (g->bitmap.pixel_mode == FT_PIXEL_MODE_BGRA) {
            int used;

            scale_into(g->bitmap.buffer, (int)g->bitmap.width, (int)g->bitmap.rows,
                       (int)g->bitmap.pitch, dst, cols * cell * 4, box, &used);
            if (!used) {
                cells[i] = 0xFFFF;
                continue;
            }
        } else if (g->bitmap.pixel_mode == FT_PIXEL_MODE_GRAY) {
            /* 非彩色回退：灰度当 alpha，涂成单色（深灰，跟正文同色系） */
            int x, y;

            for (y = 0; y < (int)g->bitmap.rows && y < box; y++) {
                const unsigned char *src =
                    g->bitmap.buffer + (size_t)y * (size_t)g->bitmap.pitch;
                int oy = (box - (int)g->bitmap.rows) / 2 + y;
                int ox = (box - (int)g->bitmap.width) / 2;

                if (oy < 0 || oy >= cell)
                    continue;
                for (x = 0; x < (int)g->bitmap.width; x++) {
                    int px = ox + x;
                    unsigned char a = src[x];

                    if (px < 0 || px >= cell || !a)
                        continue;
                    {
                        unsigned char *p = dst + (size_t)oy * cols * cell * 4 + (size_t)px * 4;
                        /* 预乘的深灰 */
                        p[0] = (unsigned char)(0x22 * a / 255);
                        p[1] = (unsigned char)(0x22 * a / 255);
                        p[2] = (unsigned char)(0x22 * a / 255);
                        p[3] = a;
                    }
                }
            }
        } else {
            cells[i] = 0xFFFF;
            continue;
        }

        /* FreeType 给的是非预乘 BGRA，转成预乘 ARGB 并换字节序（BGRA -> 内存里的 ARGB8888） */
        {
            int y, x;

            for (y = 0; y < cell; y++) {
                unsigned char *row = dst + (size_t)y * cols * cell * 4;

                for (x = 0; x < cell; x++) {
                    unsigned char *p = row + (size_t)x * 4;
                    unsigned int a = p[3], b = p[0], gr = p[1], r = p[2];

                    if (!a)
                        continue;
                    p[0] = (unsigned char)(b * a / 255);  /* B（预乘后仍按 BGRA 存） */
                    p[1] = (unsigned char)(gr * a / 255);
                    p[2] = (unsigned char)(r * a / 255);
                }
            }
        }
    }


    /* 4. 写文件 */
    {
        FILE *bf = fopen(out_path, "wb");
        uint32_t hdr[6];
        int n = 0;

        if (!bf) {
            fprintf(stderr, "mkemoji: 无法写入 %s\n", out_path);
            return 1;
        }
        for (i = 0; i < ncp; i++)
            if (cells[i] != 0xFFFF)
                n++;
        fwrite("XIMEEJ01", 1, 8, bf);
        hdr[0] = (uint32_t)cell;
        hdr[1] = (uint32_t)cell;
        hdr[2] = (uint32_t)cols;
        hdr[3] = (uint32_t)cell; /* baseline：emoji 占满整格 */
        hdr[4] = (uint32_t)box;
        hdr[5] = (uint32_t)n;
        fwrite(hdr, sizeof(uint32_t), 6, bf);
        for (i = 0; i < ncp; i++) {
            if (cells[i] == 0xFFFF)
                continue;
            fwrite(&cps[i], sizeof(uint32_t), 1, bf);
            fwrite(&cells[i], sizeof(unsigned short), 1, bf);
            fwrite(&advs[i], sizeof(unsigned short), 1, bf);
        }
        fwrite(atlas, 1, (size_t)ncell * (size_t)cell * (size_t)cell * 4, bf);
        fclose(bf);
        fprintf(stderr, "mkemoji: %d/%d 个字形，cell %dx%d，strike=%d\n",
                n, ncp, cell, cell, strike);
    }

    FT_Done_Face(face);
    FT_Done_FreeType(lib);
    free(atlas);
    free(cps);
    free(cells);
    free(advs);
    return 0;
}
