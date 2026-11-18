/* 字形图集接口。数据文件由 tools/mkfont 在构建期从系统 CJK 字体烘焙生成，
 * 运行时不需要 fontconfig / freetype / TTF 文件。
 */
#ifndef XIME_FONT_ATLAS_H
#define XIME_FONT_ATLAS_H

#include <stdint.h>

struct xime_glyph {
    uint32_t cp;   /* 码点 */
    uint16_t cell; /* 图集中的格子序号 */
    uint16_t adv;  /* 步进宽度（像素） */
};

extern const struct xime_glyph xime_glyphs[];
extern const int xime_glyph_count;
extern const unsigned char xime_font_atlas[]; /* cols*rows 个 cell，每 cell 为 cell_w*cell_h 字节的 8bpp alpha */
extern const int xime_cell_w;
extern const int xime_cell_h;
extern const int xime_atlas_cols;
extern const int xime_baseline; /* 格子顶部到基线 */
extern const int xime_font_px;

#endif
