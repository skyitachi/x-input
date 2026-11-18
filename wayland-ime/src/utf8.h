#ifndef XIME_UTF8_H
#define XIME_UTF8_H

#include <stddef.h>
#include <stdint.h>

/* 解码一个 UTF-8 字符，返回消耗的字节数（0 表示字符串结尾）。非法序列返回 1 并给出 U+FFFD。 */
int utf8_decode(const char *s, uint32_t *cp);

/* 编码一个码点，返回写入字节数（1..4），不写结尾 0。out 至少 4 字节。 */
size_t utf8_encode(uint32_t cp, char out[4]);

/* 字符串中有多少个码点 */
size_t utf8_strlen(const char *s);

/* 追加 src 到 dst（带容量检查），返回新的长度 */
size_t utf8_append(char *dst, size_t cap, size_t len, const char *src);

#endif
