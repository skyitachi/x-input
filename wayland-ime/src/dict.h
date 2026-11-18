#ifndef XIME_DICT_H
#define XIME_DICT_H

#include <stddef.h>

/* 一条候选：拼音键长度 + 汉字串 + 词频。val 指向静态词库或用户词表的只读内存。 */
struct xime_hit {
    const char *val;
    unsigned int freq;
    unsigned char len;     /* 该词条拼音键的字节长度 */
    unsigned char partial; /* 1 表示输入串只是它拼音的前缀（补全候选） */
};

/* 载入内置词库，并（可选）合并外部词表。extra_path 可为 NULL。返回载入的外部词条数。 */
int dict_init(const char *extra_path);

/* 精确查表：p 为待查拼音（plen 字节）。同一拼音的多个候选按词频降序写入 out。
 * 返回写入条数。 */
int dict_exact(const char *p, int plen, struct xime_hit *out, int cap);

/* 前缀查表：找拼音以 p 开头、且比 p 至多多出 max_extra 个字符的词条，
 * 按词频降序取前 cap 条。用于「nih -> 你好」这类未输入完整音节的补全。 */
int dict_prefix(const char *p, int plen, int max_extra, struct xime_hit *out, int cap);

/* 简拼匹配：s 形如 "nh" / "zgrm"，与词条声母串做精确匹配。返回命中条数。 */
int dict_initials(const char *s, int len, struct xime_hit *out, int cap);

/* 是否为合法拼音音节 */
int dict_is_syllable(const char *s);

/* 最长合法音节长度 */
size_t dict_max_syllable_len(void);

/* 遍历所有候选串（用于构建字形图集的字符集） */
void dict_foreach_value(void (*cb)(const char *val, void *ud), void *ud);

/* 内置词条总数（不含用户词表） */
int dict_word_count(void);

#endif
