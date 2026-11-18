/* 词库：构建期生成的静态词表 + 运行期可选的用户词表。
 *
 * 静态词表按拼音字节序排好，因此"给定拼音找候选"是一次二分查找；
 * "给定拼音前缀找更长词条"则是二分定位后扫一段连续区间。
 * 12 万条词做这两件事都在几十微秒量级，不需要哈希表或 trie。
 */
#define _POSIX_C_SOURCE 200809L

#include "dict.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dict_gen.h"

/* 用户词表词频固定高于内置词，保证自造词/纠错优先命中 */
#define USER_FREQ 1000000u

struct user_word {
    char *key;
    char *val;
    unsigned int freq;
};

static struct user_word *g_user;
static int g_user_n;
static int g_user_cap;
static size_t g_max_syl = 1;
static int g_ready;

static const char *key_of(unsigned int off)
{
    return xime_dict_blob + off;
}

static const char *val_of(unsigned int off)
{
    return xime_dict_blob + off;
}

/* 比较 key 与查询串 p 的前 plen 字节：
 *   返回 0 表示 key 以 p 开头（key 可能更长，也可能恰好等于 p）；
 *   返回 <0 表示 key 在字典序上小于 p。 */
static int cmp_prefix(const char *key, const char *p, int plen)
{
    int i;

    for (i = 0; i < plen; i++) {
        unsigned char a = (unsigned char)key[i];
        unsigned char b = (unsigned char)p[i];

        if (a != b)
            return (int)a - (int)b;
        if (!a)
            return 0;
    }
    return 0;
}

static int lower_bound_static(const char *p, int plen)
{
    int lo = 0, hi = XIME_DICT_WORD_COUNT;

    while (lo < hi) {
        int mid = lo + (hi - lo) / 2;

        if (cmp_prefix(key_of(xime_dict_words[mid].key_off), p, plen) < 0)
            lo = mid + 1;
        else
            hi = mid;
    }
    return lo;
}

/* 保持 out[0..n) 按词频降序，最多 cap 条 */
static int push_hit(struct xime_hit *out, int n, int cap, const struct xime_hit *h)
{
    int i;

    for (i = 0; i < n; i++)
        if (out[i].val == h->val)
            return n;
    if (n < cap) {
        out[n] = *h;
        n++;
    } else if (h->freq <= out[n - 1].freq) {
        return n;
    } else {
        out[n - 1] = *h;
    }
    for (i = n - 1; i > 0 && out[i].freq > out[i - 1].freq; i--) {
        struct xime_hit tmp = out[i];

        out[i] = out[i - 1];
        out[i - 1] = tmp;
    }
    return n;
}

int dict_exact(const char *p, int plen, struct xime_hit *out, int cap)
{
    int i, n = 0;

    if (!g_ready || plen <= 0 || cap <= 0)
        return 0;
    i = lower_bound_static(p, plen);
    while (i < XIME_DICT_WORD_COUNT) {
        const struct xime_word_ref *w = &xime_dict_words[i];
        const char *k = key_of(w->key_off);

        if (cmp_prefix(k, p, plen) != 0)
            break;
        if ((int)strlen(k) == plen) {
            struct xime_hit h;

            h.val = val_of(w->val_off);
            h.freq = w->freq;
            h.len = (unsigned char)plen;
            h.partial = 0;
            n = push_hit(out, n, cap, &h);
            if (n == cap && w->freq < out[n - 1].freq)
                break; /* 同键条目按词频降序，后面的只会更小 */
        }
        i++;
    }
    for (i = 0; i < g_user_n; i++) {
        if (strcmp(g_user[i].key, p) == 0 && (int)strlen(g_user[i].key) == plen) {
            struct xime_hit h;

            h.val = g_user[i].val;
            h.freq = g_user[i].freq;
            h.len = (unsigned char)plen;
            h.partial = 0;
            n = push_hit(out, n, cap, &h);
        }
    }
    return n;
}

int dict_prefix(const char *p, int plen, int max_extra, struct xime_hit *out, int cap)
{
    int i, n = 0;

    if (!g_ready || plen <= 0 || cap <= 0)
        return 0;
    i = lower_bound_static(p, plen);
    while (i < XIME_DICT_WORD_COUNT) {
        const struct xime_word_ref *w = &xime_dict_words[i];
        const char *k = key_of(w->key_off);
        int kl;

        if (cmp_prefix(k, p, plen) != 0)
            break;
        kl = (int)strlen(k);
        if (kl > plen && kl - plen <= max_extra) {
            struct xime_hit h;

            h.val = val_of(w->val_off);
            h.freq = w->freq;
            h.len = (unsigned char)kl;
            h.partial = 1;
            n = push_hit(out, n, cap, &h);
        }
        i++;
    }
    for (i = 0; i < g_user_n; i++) {
        const char *k = g_user[i].key;
        int kl = (int)strlen(k);

        if (kl > plen && kl - plen <= max_extra && strncmp(k, p, (size_t)plen) == 0) {
            struct xime_hit h;

            h.val = g_user[i].val;
            h.freq = g_user[i].freq;
            h.len = (unsigned char)kl;
            h.partial = 1;
            n = push_hit(out, n, cap, &h);
        }
    }
    return n;
}

int dict_initials(const char *s, int len, struct xime_hit *out, int cap)
{
    int lo = 0, hi = XIME_DICT_INITIALS_COUNT, i, n = 0;

    if (!g_ready || len <= 0 || cap <= 0)
        return 0;
    while (lo < hi) {
        int mid = lo + (hi - lo) / 2;

        if (cmp_prefix(xime_initials_blob + xime_initials[mid].off, s, len) < 0)
            lo = mid + 1;
        else
            hi = mid;
    }
    for (i = lo; i < XIME_DICT_INITIALS_COUNT && n < cap; i++) {
        const char *k = xime_initials_blob + xime_initials[i].off;
        const struct xime_word_ref *w;

        if (cmp_prefix(k, s, len) != 0 || (int)strlen(k) != len)
            break;
        w = &xime_dict_words[xime_initials[i].word];
        out[n].val = val_of(w->val_off);
        out[n].freq = w->freq;
        out[n].len = (unsigned char)len;
        out[n].partial = 0;
        n++;
    }
    return n;
}

void dict_init_syllables(void)
{
    const char *p = xime_syllables;

    while (*p) {
        const char *start;

        while (*p == ' ')
            p++;
        if (!*p)
            break;
        start = p;
        while (*p && *p != ' ')
            p++;
        if ((size_t)(p - start) > g_max_syl)
            g_max_syl = (size_t)(p - start);
    }
}

static int load_user_dict(const char *path)
{
    FILE *f;
    char line[512];
    int loaded = 0;

    f = fopen(path, "r");
    if (!f)
        return 0;
    while (fgets(line, sizeof(line), f)) {
        char *key, *val, *sep, *nl, *save = NULL, *tok;
        char *copy;

        if (line[0] == '#' || line[0] == '\n')
            continue;
        nl = strchr(line, '\n');
        if (nl)
            *nl = 0;
        sep = strchr(line, '\t');
        if (!sep)
            sep = strchr(line, ' ');
        if (!sep)
            continue;
        *sep = 0;
        key = line;
        val = sep + 1;
        while (*val == ' ' || *val == '\t')
            val++;
        if (!*key || !*val)
            continue;
        copy = strdup(val);
        if (!copy)
            continue;
        for (tok = strtok_r(copy, " \t", &save); tok; tok = strtok_r(NULL, " \t", &save)) {
            if (g_user_n == g_user_cap) {
                int cap = g_user_cap ? g_user_cap * 2 : 64;
                struct user_word *p = realloc(g_user, (size_t)cap * sizeof(*p));

                if (!p)
                    break;
                g_user = p;
                g_user_cap = cap;
            }
            g_user[g_user_n].key = strdup(key);
            g_user[g_user_n].val = strdup(tok);
            /* 同一条目里靠前的候选词频略高，保持用户书写的顺序 */
            g_user[g_user_n].freq = USER_FREQ - (unsigned)loaded;
            if (!g_user[g_user_n].key || !g_user[g_user_n].val) {
                free(g_user[g_user_n].key);
                free(g_user[g_user_n].val);
                break;
            }
            g_user_n++;
            loaded++;
        }
        free(copy);
    }
    fclose(f);
    return loaded;
}

int dict_init(const char *extra_path)
{
    if (g_ready)
        return 0;
    dict_init_syllables();
    g_ready = 1;
    return extra_path ? load_user_dict(extra_path) : 0;
}

int dict_is_syllable(const char *s)
{
    const char *p = xime_syllables;
    size_t len = strlen(s);

    if (!g_ready)
        dict_init(NULL);
    if (len == 0 || len > g_max_syl)
        return 0;
    while ((p = strstr(p, s)) != NULL) {
        int left_ok = (p == xime_syllables) || (p[-1] == ' ');
        int right_ok = (p[len] == ' ') || (p[len] == 0);

        if (left_ok && right_ok)
            return 1;
        p += len;
    }
    return 0;
}

size_t dict_max_syllable_len(void)
{
    if (!g_ready)
        dict_init(NULL);
    return g_max_syl;
}

int dict_word_count(void)
{
    return XIME_DICT_WORD_COUNT;
}

void dict_foreach_value(void (*cb)(const char *val, void *ud), void *ud)
{
    int i;

    if (!cb)
        return;
    if (!g_ready)
        dict_init(NULL);
    for (i = 0; i < XIME_DICT_WORD_COUNT; i++)
        cb(val_of(xime_dict_words[i].val_off), ud);
    for (i = 0; i < g_user_n; i++)
        cb(g_user[i].val, ud);
}
