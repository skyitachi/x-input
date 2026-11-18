/* mkfont - 构建期字形烘焙器
 *
 * 读取系统里的 CJK 字体，把「词典里出现的全部字符 + ASCII」光栅化成
 * 8bpp alpha 图集，输出成 C 源文件。这样运行时既不需要 freetype/fontconfig，
 * 也不需要携带 TTF，二进制内自带可显示汉字的最小字形集。
 *
 * 用法: mkfont <font.ttf|font.ttc> <out> [px] [extra_chars.txt]
 */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../src/dict.h"
#include "../src/utf8.h"

#define STB_TRUETYPE_IMPLEMENTATION
#include "stb_truetype.h"

#define MAX_CPS 16384

static uint32_t g_cps[MAX_CPS];
static int g_ncp;
/* BMP 用位图去重，否则 12 万词条逐个线性查重会退化成 O(n^2) */
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
        for (i = 0; i < g_ncp; i++)  /* 增补平面字符极少，线性查足够 */
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

static unsigned char *read_file(const char *path, size_t *out_size)
{
    FILE *f = fopen(path, "rb");
    unsigned char *buf;
    long size;

    if (!f)
        return NULL;
    fseek(f, 0, SEEK_END);
    size = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (size <= 0) {
        fclose(f);
        return NULL;
    }
    buf = malloc((size_t)size + 1);
    if (!buf) {
        fclose(f);
        return NULL;
    }
    if (fread(buf, 1, (size_t)size, f) != (size_t)size) {
        free(buf);
        fclose(f);
        return NULL;
    }
    fclose(f);
    buf[size] = '\0'; /* 便于当作文本使用（额外字符集文件） */
    *out_size = (size_t)size;
    return buf;
}

int main(int argc, char **argv)
{
    const char *font_path, *out_path;
    int px = 16;
    size_t font_size = 0;
    unsigned char *font_data, *atlas;
    stbtt_fontinfo font;
    int offset = -1, i, idx;
    float scale;
    int ascent, descent, line_gap;
    int cell_w, cell_h, baseline, cols, rows, ncell;
    uint32_t *cps;
    unsigned short *cells; /* 每个 cp 的格子序号，0xFFFF 表示缺字 */
    unsigned short *advs;
    FILE *out;
    int have_glyph = 0;

    if (argc < 3) {
        fprintf(stderr, "用法: %s <font.ttf|ttc> <out> [px] [extra_chars.txt]\n", argv[0]);
        return 2;
    }
    font_path = argv[1];
    out_path = argv[2];
    if (argc > 3)
        px = atoi(argv[3]);
    if (px < 10 || px > 48)
        px = 16;

    dict_init(NULL);

    /* ASCII 可打印字符 + 词典里的全部汉字 */
    for (i = 0x20; i < 0x7F; i++)
        add_cp((uint32_t)i);
    dict_foreach_value(collect_value, NULL);

    /* 可选的额外字符集（UTF-8 文本）：简繁转换后的繁体用字不在词库里，
     * 但候选窗要直接显示它们，所以一起烘焙进来。 */
    if (argc > 4) {
        size_t extra_size = 0;
        char *extra = (char *)read_file(argv[4], &extra_size);
        const char *p;
        uint32_t cp;
        int l;

        if (!extra) {
            fprintf(stderr, "mkfont: 打不开字符集 %s\n", argv[4]);
            return 1;
        }
        extra[extra_size] = '\0'; /* read_file 已经多分配了 1 字节 */
        for (p = extra; (l = utf8_decode(p, &cp)) > 0; p += l)
            add_cp(cp);
        free(extra);
    }
    qsort(g_cps, (size_t)g_ncp, sizeof(uint32_t), cmp_cp);

    font_data = read_file(font_path, &font_size);
    if (!font_data) {
        fprintf(stderr, "mkfont: 打不开字体 %s\n", font_path);
        return 1;
    }

    /* TTC 里挑一个真的有汉字字形的 face */
    for (idx = 0; idx < 16; idx++) {
        int off = stbtt_GetFontOffsetForIndex(font_data, idx);
        stbtt_fontinfo tmp;

        if (off < 0)
            break;
        if (!stbtt_InitFont(&tmp, font_data, off))
            continue;
        if (offset < 0)
            offset = off;
        if (stbtt_FindGlyphIndex(&tmp, 0x4F60) > 0) { /* 你 */
            offset = off;
            have_glyph = 1;
            break;
        }
    }
    if (offset < 0 || !stbtt_InitFont(&font, font_data, offset)) {
        fprintf(stderr, "mkfont: 无法解析字体 %s\n", font_path);
        return 1;
    }
    if (!have_glyph)
        fprintf(stderr, "mkfont: 警告：字体 %s 里找不到汉字字形，候选窗将显示占位方块\n", font_path);

    scale = stbtt_ScaleForPixelHeight(&font, (float)px);
    stbtt_GetFontVMetrics(&font, &ascent, &descent, &line_gap);
    baseline = (int)((float)ascent * scale) + 1;
    cell_w = px + 2;
    cell_h = (int)((float)(ascent - descent) * scale) + 2;
    if (cell_h < px + 2)
        cell_h = px + 2;
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
        fprintf(stderr, "mkfont: 内存不足\n");
        return 1;
    }

    for (i = 0; i < g_ncp; i++) {
        uint32_t cp = g_cps[i];
        int gi = stbtt_FindGlyphIndex(&font, (int)cp);
        int adv = 0, lsb = 0;
        int x0, y0, x1, y1, gw, gh, yy, xx;
        unsigned char *tmp;

        if (gi <= 0) {
            cells[i] = 0xFFFF;
            advs[i] = (unsigned short)(px / 2);
            continue;
        }
        stbtt_GetGlyphHMetrics(&font, gi, &adv, &lsb);
        advs[i] = (unsigned short)((float)adv * scale + 0.5f);
        if (advs[i] == 0)
            advs[i] = (unsigned short)(px / 3);

        stbtt_GetCodepointBitmapBox(&font, cp, scale, scale, &x0, &y0, &x1, &y1);
        gw = x1 - x0;
        gh = y1 - y0;
        if (gw <= 0 || gh <= 0 || gw > cell_w + 8 || gh > cell_h + 8) {
            cells[i] = 0xFFFF;
            continue;
        }
        tmp = calloc((size_t)gw * (size_t)gh, 1);
        if (!tmp) {
            cells[i] = 0xFFFF;
            continue;
        }
        stbtt_MakeCodepointBitmap(&font, tmp, gw, gh, gw, scale, scale, (int)cp);
        cells[i] = (unsigned short)i;
        cps[i] = cp;
        for (yy = 0; yy < gh; yy++) {
            int dy = baseline + y0 + yy;
            if (dy < 0 || dy >= cell_h)
                continue;
            for (xx = 0; xx < gw; xx++) {
                int dx = x0 + xx;
                unsigned char a = tmp[(size_t)yy * gw + xx];
                unsigned char *dst;

                if (!a || dx < 0 || dx >= cell_w)
                    continue;
                dst = &atlas[((size_t)(i / cols) * cell_h + dy) * cols * cell_w + (size_t)(i % cols) * cell_w + dx];
                if (a > *dst)
                    *dst = a;
            }
        }
        free(tmp);
    }

    /* 输出为 .bin 时写紧凑二进制，供 Rust 版 include_bytes! 嵌入。
     * 布局：magic(8) + 6×u32 头 + glyph_count×8（cp, cell, adv）+ 图集像素。 */
    {
        const char *dot = out_path ? strrchr(out_path, '.') : NULL;

        if (dot && strcmp(dot, ".bin") == 0) {
            FILE *bf = fopen(out_path, "wb");
            uint32_t n = 0, hdr[6];

            if (!bf) {
                fprintf(stderr, "mkfont: 无法写入 %s\n", out_path);
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
                uint32_t cp;
                uint16_t cell, adv;

                if (cells[i] == 0xFFFF)
                    continue;
                cp = cps[i];
                cell = cells[i];
                adv = advs[i];
                fwrite(&cp, sizeof(cp), 1, bf);
                fwrite(&cell, sizeof(cell), 1, bf);
                fwrite(&adv, sizeof(adv), 1, bf);
            }
            fwrite(atlas, (size_t)ncell * (size_t)cell_w * (size_t)cell_h, 1, bf);
            fclose(bf);
            printf("生成 %s\n  cell %dx%d  格子 %d  字形 %u  像素 %zuKB\n", out_path, cell_w, cell_h,
                   ncell, n, ((size_t)ncell * cell_w * cell_h) / 1024);
            return 0;
        }
    }

    out = fopen(out_path, "w");
    if (!out) {
        fprintf(stderr, "mkfont: 无法写入 %s\n", out_path);
        return 1;
    }
    fprintf(out, "/* 由 tools/mkfont 自动生成，请勿手工编辑 */\n");
    fprintf(out, "#include \"font_atlas.h\"\n\n");
    fprintf(out, "const int xime_cell_w = %d;\n", cell_w);
    fprintf(out, "const int xime_cell_h = %d;\n", cell_h);
    fprintf(out, "const int xime_atlas_cols = %d;\n", cols);
    fprintf(out, "const int xime_baseline = %d;\n", baseline);
    fprintf(out, "const int xime_font_px = %d;\n\n", px);
    fprintf(out, "const int xime_glyph_count = %d;\n", g_ncp);
    fprintf(out, "const struct xime_glyph xime_glyphs[] = {\n");
    for (i = 0; i < g_ncp; i++) {
        if (cells[i] == 0xFFFF)
            continue;
        fprintf(out, "    {0x%04X, %u, %u},\n", cps[i], (unsigned)cells[i], (unsigned)advs[i]);
    }
    fprintf(out, "};\n\n");
    fprintf(out, "const unsigned char xime_font_atlas[] =\n");
    {
        size_t total = (size_t)ncell * (size_t)cell_w * (size_t)cell_h;
        size_t k;

        for (k = 0; k < total; k++) {
            if (k % 24 == 0)
                fprintf(out, "\n    \"");
            fprintf(out, "\\%03o", atlas[k]);
            if (k % 24 == 23 || k + 1 == total)
                fprintf(out, "\"");
        }
        fprintf(out, ";\n");
    }
    fclose(out);

    printf("mkfont: %s -> %s\n", font_path, out_path);
    printf("        字体索引=%d 像素=%d 格子=%dx%d 基线=%d 图集=%dx%d 字形=%d\n", offset, px, cell_w, cell_h,
           baseline, cols, rows, g_ncp);
    free(atlas);
    free(cps);
    free(cells);
    free(advs);
    free(font_data);
    return 0;
}
