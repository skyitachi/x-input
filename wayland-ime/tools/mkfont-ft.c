/* mkfont-ft - 用 FreeType 烘焙字形图集（构建期用，运行时不依赖 freetype）
 *
 * mkfont.c 走的是 stb_truetype，它**完全不做 hinting**：汉字竖笔只有 1.3~1.5
 * 像素宽，永远摊在两个 60~80% 的灰像素上，视觉上就是"比 fcitx 糊"。
 * fcitx5 走 cairo + FreeType，hinting 把笔画对齐到整像素，一格 100% 覆盖。
 *
 * 这个工具在**构建期**调用系统 FreeType，用和 fcitx5 相同的光栅化路径生成
 * 位图，再嵌进二进制。运行时仍然零依赖——只是把 hinting 从运行期搬到构建期。
 *
 * 用法: mkfont-ft <font.ttf|ttc> <out.bin> [px] [extra_chars.txt] [mode]
 *   mode: light（默认，等效 hintslight）/ normal / mono / 后缀 -na 关闭自动 hint
 */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <ft2build.h>
#include FT_FREETYPE_H

#include "../src/dict.h"
#include "../src/utf8.h"

#define MAX_CPS 16384

static uint32_t g_cps[MAX_CPS];
static int g_ncp;
static unsigned char g_seen[0x10000 / 8];

static void add_cp(uint32_t cp)
{
    int i;

    if (cp < 0x20 || cp == 0x7F)
        return;
    if (cp < 0x10000) {
        unsigned int byte = cp >> 3, bit = 1u << (cp & 7);

        if (g_seen[byte] & bit)
            return;
        g_seen[byte] |= (unsigned char)bit;
    } else {
        for (i = 0; i < g_ncp; i++)
            if (g_cps[i] == cp)
                return;
    }
    if (g_ncp < MAX_CPS)
        g_cps[g_ncp++] = cp;
}

static void collect_value(const char *val, void *ud)
{
    uint32_t cp;
    int l;

    (void)ud;
    while ((l = utf8_decode(val, &cp)) > 0) {
        add_cp(cp);
        val += l;
    }
}

static int cmp_cp(const void *a, const void *b)
{
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;

    return (x > y) - (x < y);
}

int main(int argc, char **argv)
{
    const char *font_path, *out_path;
    int px = 18;
    const char *mode = "light";
    FT_Library lib;
    FT_Face face = NULL;
    FT_Int32 flags;
    long idx;
    int i, cell_w, cell_h, baseline, cols, rows, ncell, reported = 0;
    unsigned char *atlas;
    uint32_t *cps;
    unsigned short *cells, *advs;

    if (argc < 3) {
        fprintf(stderr, "用法: %s <font.ttf|ttc> <out.bin> [px] [extra_chars.txt] [mode]\n", argv[0]);
        return 2;
    }
    font_path = argv[1];
    out_path = argv[2];
    if (argc > 3)
        px = atoi(argv[3]);
    if (px < 10 || px > 48)
        px = 18;
    if (argc > 5)
        mode = argv[5];

    dict_init(NULL);
    for (i = 0x20; i < 0x7F; i++)
        add_cp((uint32_t)i);
    dict_foreach_value(collect_value, NULL);

    if (argc > 4) {
        FILE *f = fopen(argv[4], "rb");
        long sz;

        if (!f) {
            fprintf(stderr, "mkfont-ft: 打不开字符集 %s\n", argv[4]);
            return 1;
        }
        fseek(f, 0, SEEK_END);
        sz = ftell(f);
        fseek(f, 0, SEEK_SET);
        if (sz > 0) {
            char *buf = malloc((size_t)sz + 1);

            if (buf && fread(buf, 1, (size_t)sz, f) == (size_t)sz) {
                const char *p = buf;
                uint32_t cp;
                int l;

                buf[sz] = '\0';
                while ((l = utf8_decode(p, &cp)) > 0) {
                    add_cp(cp);
                    p += l;
                }
            }
            free(buf);
        }
        fclose(f);
    }
    qsort(g_cps, (size_t)g_ncp, sizeof(uint32_t), cmp_cp);

    if (FT_Init_FreeType(&lib)) {
        fprintf(stderr, "mkfont-ft: FreeType 初始化失败\n");
        return 1;
    }
    /* TTC 里挑一个真有汉字字形的 face */
    for (idx = 0; idx < 16; idx++) {
        FT_Face f;

        if (FT_New_Face(lib, font_path, idx, &f))
            break;
        if (!face)
            face = f;
        if (FT_Get_Char_Index(f, 0x4F60)) { /* 你 */
            if (face != f)
                FT_Done_Face(face);
            face = f;
            reported = 1;
            break;
        }
        if (f != face)
            FT_Done_Face(f);
    }
    if (!face) {
        fprintf(stderr, "mkfont-ft: 无法打开字体 %s\n", font_path);
        return 1;
    }
    if (!reported)
        fprintf(stderr, "mkfont-ft: 警告：字体里找不到汉字字形，候选窗将显示占位方块\n");

    if (FT_Set_Pixel_Sizes(face, 0, (FT_UInt)px)) {
        fprintf(stderr, "mkfont-ft: 不支持 %d 像素字号\n", px);
        return 1;
    }

    /* FT_LOAD_TARGET_LIGHT 只做竖直方向 hint（hintslight），对 CJK 最稳：
     * 横画对齐像素、竖画保持亚像素宽度，不会像 full hinting 那样把字挤变形。
     * mono 则直接 1bpp，最锐利但笔画断点多。 */
    if (strncmp(mode, "mono", 4) == 0)
        flags = FT_LOAD_RENDER | FT_LOAD_TARGET_MONO;
    else if (strncmp(mode, "normal", 6) == 0)
        flags = FT_LOAD_RENDER | FT_LOAD_TARGET_NORMAL;
    else
        flags = FT_LOAD_RENDER | FT_LOAD_TARGET_LIGHT;
    if (!strstr(mode, "-na"))
        flags |= FT_LOAD_FORCE_AUTOHINT; /* Noto CJK 本身没有 hint 指令，必须靠自动 hint */

    /* 版面：格子只装墨迹。汉字约占基线上 0.88 em，拉丁下伸约 0.22 em */
    cell_w = px + 2;
    cell_h = (int)((float)px * 1.3f) + 1;
    baseline = px + 1;
    if (baseline > cell_h - 2)
        baseline = cell_h - 2;

    cols = 64;
    rows = (g_ncp + cols - 1) / cols;
    ncell = cols * rows;
    atlas = calloc((size_t)ncell * (size_t)cell_w * (size_t)cell_h, 1);
    cps = calloc((size_t)ncell, sizeof(uint32_t));
    cells = malloc((size_t)g_ncp * sizeof(unsigned short));
    advs = malloc((size_t)g_ncp * sizeof(unsigned short));
    if (!atlas || !cps || !cells || !advs) {
        fprintf(stderr, "mkfont-ft: 内存不足\n");
        return 1;
    }

    for (i = 0; i < g_ncp; i++) {
        uint32_t cp = g_cps[i];
        FT_GlyphSlot g;
        int adv, x, y, dstx, dsty;

        if (!FT_Get_Char_Index(face, cp)) {
            cells[i] = 0xFFFF;
            advs[i] = (unsigned short)(px / 2);
            continue;
        }
        if (FT_Load_Char(face, cp, flags)) {
            cells[i] = 0xFFFF;
            advs[i] = (unsigned short)(px / 2);
            continue;
        }
        g = face->glyph;
        adv = (int)(g->advance.x >> 6);
        if (adv <= 0)
            adv = px / 2;
        advs[i] = (unsigned short)adv;
        cells[i] = (unsigned short)i;
        cps[i] = cp;

        dstx = g->bitmap_left;
        dsty = baseline - g->bitmap_top;
        if (getenv("XIME_FT_DEBUG") && (cp == 0x31 || cp == 0x4F60))
            fprintf(stderr, "dbg U+%04X w=%d rows=%d pitch=%d left=%d top=%d dstx=%d dsty=%d\n",
                    cp, g->bitmap.width, g->bitmap.rows, g->bitmap.pitch,
                    g->bitmap_left, g->bitmap_top, dstx, dsty);

        for (y = 0; y < (int)g->bitmap.rows; y++) {
            const unsigned char *src;
            unsigned char *dst;
            int row = dsty + y;

            if (row < 0 || row >= cell_h)
                continue;
            dst = &atlas[((size_t)(i / cols) * cell_h + (size_t)row) * cols * cell_w
                         + (size_t)(i % cols) * cell_w];
            if (g->bitmap.pixel_mode == FT_PIXEL_MODE_MONO) {
                src = g->bitmap.buffer + (size_t)y * g->bitmap.pitch;
                for (x = 0; x < (int)g->bitmap.width; x++) {
                    int col = dstx + x;

                    if (col < 0 || col >= cell_w)
                        continue;
                    dst[col] = (src[x >> 3] & (0x80 >> (x & 7))) ? 255 : 0;
                }
            } else {
                src = g->bitmap.buffer + (size_t)y * g->bitmap.pitch;
                for (x = 0; x < (int)g->bitmap.width; x++) {
                    int col = dstx + x;
                    unsigned char a = src[x];

                    if (col < 0 || col >= cell_w || !a)
                        continue;
                    dst[col] = a;
                }
            }
        }
    }

    {
        FILE *bf = fopen(out_path, "wb");
        uint32_t n = 0, hdr[6];

        if (!bf) {
            fprintf(stderr, "mkfont-ft: 无法写入 %s\n", out_path);
            return 1;
        }
        for (i = 0; i < g_ncp; i++)
            if (cells[i] != 0xFFFF)
                n++;
        fwrite("XIMEAT01", 1, 8, bf);
        hdr[0] = (uint32_t)cell_w;
        hdr[1] = (uint32_t)cell_h;
        hdr[2] = (uint32_t)cols;
        hdr[3] = (uint32_t)baseline;
        hdr[4] = (uint32_t)px;
        hdr[5] = n;
        fwrite(hdr, sizeof(uint32_t), 6, bf);
        for (i = 0; i < g_ncp; i++) {
            if (cells[i] == 0xFFFF)
                continue;
            {
                uint32_t rec[2];
                unsigned short tail[2];

                rec[0] = cps[i];
                tail[0] = cells[i];
                tail[1] = advs[i];
                fwrite(rec, sizeof(uint32_t), 1, bf);
                fwrite(tail, sizeof(unsigned short), 2, bf);
            }
        }
        fwrite(atlas, 1, (size_t)ncell * (size_t)cell_w * (size_t)cell_h, bf);
        fclose(bf);
        fprintf(stderr, "mkfont-ft: %d 个字形，cell %dx%d，mode=%s\n", (int)n, cell_w, cell_h, mode);
    }

    FT_Done_Face(face);
    FT_Done_FreeType(lib);
    free(atlas);
    free(cps);
    free(cells);
    free(advs);
    return 0;
}
