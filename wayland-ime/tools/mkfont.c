/* mkfont - 构建期字形烘焙器
 *
 * 读取系统里的 CJK 字体，把「词典里出现的全部字符 + ASCII」光栅化成
 * 8bpp alpha 图集，输出成 C 源文件。这样运行时既不需要 freetype/fontconfig，
 * 也不需要携带 TTF，二进制内自带可显示汉字的最小字形集。
 *
 * 用法: mkfont <font.ttf|font.ttc> <out> [px] [extra_chars.txt] [gamma] [black]
 *
 * gamma < 1 会压暗抗锯齿边缘（stem darkening），black 会把低于该 alpha 的灰雾
 * 直接归零。两者合起来是为了弥补 stb 不做 hinting 的短板：FreeType 的 hinting
 * 把竖笔对齐到整像素、一格 100% 覆盖，而位图里竖笔只有 1.3~1.5 像素宽，永远摊在
 * 两个 60~80% 的灰像素上。压暗 + 去灰能把这两列推到接近实心。
 */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

#include "../src/dict.h"
#include "../src/utf8.h"

/* 超采样倍数：先按 px*SS 光栅化再降采样，边缘更干净。
 * SS=1 表示直接在目标尺寸光栅化，边缘过渡只有 1 像素宽，更像 FreeType 的
 * 直出结果（实测有墨像素更少、实心占比更高）；SS=2 更平滑但会摊出 2 像素宽的灰雾。 */
#ifndef SS
#define SS 2
#endif

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
    float scale_ss; /* 超采样用的放大 scale */
    int baseline_ss;
    int ascent, descent, line_gap;
    int cell_w, cell_h, baseline, cols, rows, ncell;
    double gamma_v = 1.0;
    int black = 0;
    unsigned char lut[256];
    uint32_t *cps;
    unsigned short *cells; /* 每个 cp 的格子序号，0xFFFF 表示缺字 */
    unsigned short *advs;
    FILE *out;
    int have_glyph = 0;

    if (argc < 3) {
        fprintf(stderr, "用法: %s <font.ttf|ttc> <out> [px] [extra_chars.txt] [gamma]\n", argv[0]);
        return 2;
    }
    font_path = argv[1];
    out_path = argv[2];
    if (argc > 3)
        px = atoi(argv[3]);
    if (px < 10 || px > 48)
        px = 16;
    if (argc > 5 && atof(argv[5]) > 0.05)
        gamma_v = atof(argv[5]);
    if (argc > 6)
        black = atoi(argv[6]);
    if (black < 0)
        black = 0;
    if (black > 200)
        black = 200;

    /* alpha 的对比曲线：先把 <=black 的灰雾压到 0，再按 gamma 调整中间调。
     * 目的是把「半透明的边缘」变成「明确的实心/无」，也就是用位图模拟 hinting
     * 的效果——实测 FreeType 带 hinting 渲染时，有墨像素中有 37% 是完全实心的，
     * 而不做这一步只有 27%，视觉上就是「灰、糊」。 */
    for (i = 0; i < 256; i++) {
        int v = i - black;

        if (v < 0)
            v = 0;
        v = (int)(255.0 * pow((double)v / (double)(255 - black), gamma_v) + 0.5);
        lut[i] = (unsigned char)(v > 255 ? 255 : v);
    }

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

    /* 用 em 归一化，不用行高归一化。
     *
     * ScaleForPixelHeight 把 (ascent - descent) 缩到 px，而 CJK 字体的行高
     * 约 1.45 em（Noto Sans CJK: 1160 - (-288) = 1448 / em 1000），于是
     * 真正的汉字只有 px * 1000/1448 ≈ 0.69 px —— 设 22px 实际只画到 15px。
     * 15px 的汉字笔画宽度不到 1.5 像素，几乎全部像素都落在抗锯齿的中间调
     * 上（ASCII 剖面里竖笔是 "+#-#" 而不是 "@@@"），视觉上就是发灰发糊。
     *
     * ScaleForMappingEmToPixels 让 1 em = px，汉字才真是 px 见方。 */
    scale = stbtt_ScaleForMappingEmToPixels(&font, (float)px);
    /* 超采样：先按 SS 倍像素光栅化，再盒式降采样回目标尺寸。
     * stb 不做 hinting，靠 SS×SS 子像素平均把边缘压平。 */
    scale_ss = stbtt_ScaleForMappingEmToPixels(&font, (float)px * SS);
    stbtt_GetFontVMetrics(&font, &ascent, &descent, &line_gap);
    /* 格子只需容纳墨迹，不必容纳整个行高：汉字约占 0.88 em（基线上），
     * 拉丁下伸部分约 0.22 em（基线下）。px*1.3 高、基线在 px+1 处两者都够，
     * 又不会像按行高算那样上下各留一大片空白。 */
    cell_w = px + 2;
    cell_h = (int)((float)px * 1.3f) + 1;
    baseline = px + 1;
    if (baseline > cell_h - 2)
        baseline = cell_h - 2;
    baseline_ss = baseline * SS;

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

        stbtt_GetCodepointBitmapBox(&font, cp, scale_ss, scale_ss, &x0, &y0, &x1, &y1);
        gw = x1 - x0;
        gh = y1 - y0;
        if (gw <= 0 || gh <= 0 || gw > cell_w * SS + 16 || gh > cell_h * SS + 16) {
            cells[i] = 0xFFFF;
            continue;
        }
        tmp = calloc((size_t)gw * (size_t)gh, 1);
        if (!tmp) {
            cells[i] = 0xFFFF;
            continue;
        }
        stbtt_MakeCodepointBitmap(&font, tmp, gw, gh, gw, scale_ss, scale_ss, (int)cp);
        cells[i] = (unsigned short)i;
        cps[i] = cp;
        /* 降采样：cell 里每个像素取 2x 图上的 SS×SS 子块平均 */
        for (yy = 0; yy < cell_h; yy++) {
            int dy = baseline_ss + y0; /* 字形在 2x cell 里的顶边 */
            unsigned char *dstrow;

            dstrow = &atlas[((size_t)(i / cols) * cell_h + yy) * cols * cell_w + (size_t)(i % cols) * cell_w];
            for (xx = 0; xx < cell_w; xx++) {
                int acc = 0, sxs, sys;

                for (sys = 0; sys < SS; sys++) {
                    int ty = yy * SS + sys - dy;

                    if (ty < 0 || ty >= gh)
                        continue;
                    for (sxs = 0; sxs < SS; sxs++) {
                        int tx = xx * SS + sxs - x0;

                        if (tx < 0 || tx >= gw)
                            continue;
                        acc += tmp[(size_t)ty * gw + tx];
                    }
                }
                if (acc)
                    dstrow[xx] = lut[(acc + SS * SS / 2) / (SS * SS)];
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
