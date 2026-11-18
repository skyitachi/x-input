#include "utf8.h"

#include <string.h>

int utf8_decode(const char *s, uint32_t *cp)
{
    const unsigned char *p = (const unsigned char *)s;
    uint32_t c;
    int n, i;

    if (p[0] == 0) {
        *cp = 0;
        return 0;
    }
    if (p[0] < 0x80) {
        *cp = p[0];
        return 1;
    }
    if ((p[0] & 0xE0) == 0xC0) {
        n = 1;
        c = p[0] & 0x1F;
    } else if ((p[0] & 0xF0) == 0xE0) {
        n = 2;
        c = p[0] & 0x0F;
    } else if ((p[0] & 0xF8) == 0xF0) {
        n = 3;
        c = p[0] & 0x07;
    } else {
        *cp = 0xFFFD;
        return 1;
    }
    for (i = 1; i <= n; i++) {
        if ((p[i] & 0xC0) != 0x80) {
            *cp = 0xFFFD;
            return i;
        }
        c = (c << 6) | (uint32_t)(p[i] & 0x3F);
    }
    *cp = c;
    return n + 1;
}

size_t utf8_encode(uint32_t cp, char out[4])
{
    if (cp < 0x80) {
        out[0] = (char)cp;
        return 1;
    }
    if (cp < 0x800) {
        out[0] = (char)(0xC0 | (cp >> 6));
        out[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp < 0x10000) {
        out[0] = (char)(0xE0 | (cp >> 12));
        out[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (cp >> 18));
    out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
    out[3] = (char)(0x80 | (cp & 0x3F));
    return 4;
}

size_t utf8_strlen(const char *s)
{
    size_t n = 0;
    uint32_t cp;
    int l;

    while ((l = utf8_decode(s, &cp)) > 0) {
        s += l;
        n++;
    }
    return n;
}

size_t utf8_append(char *dst, size_t cap, size_t len, const char *src)
{
    size_t sl = strlen(src);

    if (len + sl + 1 > cap)
        sl = (len + 1 < cap) ? cap - len - 1 : 0;
    memcpy(dst + len, src, sl);
    len += sl;
    dst[len] = 0;
    return len;
}
