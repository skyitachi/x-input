/* 拼音解码器：在「词」构成的网格上做 N-best 路径搜索。
 *
 * 为什么不能贪心切分：拼音串没有分隔符，"xian" 可能是「先」也可能是「西安」；
 * 更麻烦的是 "nihaom" 能被完整解析成「你/哈/欧盟」，也可能该切成「你好/吗」。
 * 只在首选路径的首词上做变体，永远看不到后者——所以必须保留每个位置上的
 * 多条候选路径，最后统一按代价排序。
 *
 * 代价：每个词 = log10(总词频) - log10(词频)。一个词只付一次常数项，
 * 因此长词天然优于逐字拼，符合汉语构词习惯。
 */
#include "engine.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

#include "dict.h"
#include "utf8.h"

#define XIME_MAX_KEY 32 /* 词条拼音键最长字节数（数据里最长 24） */
#define XIME_HITS 4     /* 同一拼音取几个同音候选参与搜索 */
#define XIME_BEST 6     /* 每个位置保留几条路径 */
#define XIME_EDGES 96   /* 每个位置最多几条边 */
#define COST_INF 1e9f
/* 内部拼接缓冲要比单个候选大，避免长句拼接时被编译器判为可能截断 */
#define XIME_TEXT_MAX (XIME_CAND_MAX * 2)

/* 词频总和的对数，相当于"每多切一个词就多付一次"的常数项。
 * 词频是概率乘以 1e9，所以理论上取 9；再大一点是为了补偿纯 unigram 模型
 * 缺少上下文约束——否则「你/号」这种高频单字拼接会压过「你好」这种整词。 */
#define LOG_TOTAL 10.0f
/* 补全未输完的尾部音节：按"缺一个音节"计固定惩罚，再按缺字数量轻微递增，
 * 否则输入 "n" 时「中国」会因为词频高而排在「你」前面。 */
#define PARTIAL_BASE 2.0f
#define PARTIAL_SLOPE 0.7f
/* 简拼（nh -> 你好）的惩罚，保证全拼结果优先 */
#define INITIALS_PENALTY 2.0f

struct path {
    float cost;
    char text[XIME_TEXT_MAX];
};

struct edge {
    int to;
    float cost;
    const char *val;
};

static struct path g_path[XIME_MAX_INPUT + 1][XIME_BEST];
static int g_pn[XIME_MAX_INPUT + 1];
static struct edge g_edge[XIME_EDGES];

static int hex_val(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    return -1;
}

/* 有界拼接：既要防止长句溢出，也要让编译器看得懂边界 */
static void join_text(char *dst, size_t cap, const char *a, const char *b)
{
    size_t n = 0;

    while (a && *a && n + 1 < cap)
        dst[n++] = *a++;
    while (b && *b && n + 1 < cap)
        dst[n++] = *b++;
    dst[n] = 0;
}

static float word_cost(unsigned int freq)
{
    return LOG_TOTAL - log10f((float)freq + 1.0f);
}

/* 往某个位置的候选路径表里插入一条，保持按代价升序、文本去重 */
static void push_path(int pos, const char *text, float cost)
{
    int n = g_pn[pos], i;

    for (i = 0; i < n; i++)
        if (strcmp(g_path[pos][i].text, text) == 0)
            return;
    if (n < XIME_BEST)
        g_pn[pos] = ++n;
    else if (cost >= g_path[pos][n - 1].cost)
        return;
    i = n - 1;
    while (i > 0 && g_path[pos][i - 1].cost > cost) {
        g_path[pos][i] = g_path[pos][i - 1];
        i--;
    }
    g_path[pos][i].cost = cost;
    snprintf(g_path[pos][i].text, XIME_TEXT_MAX, "%s", text);
}

static void add_edge(int *n, int to, float cost, const char *val)
{
    if (*n >= XIME_EDGES)
        return;
    g_edge[*n].to = to;
    g_edge[*n].cost = cost;
    g_edge[*n].val = val;
    (*n)++;
}

/* 枚举从位置 i 出发的所有边：整词、简拼、尾部音节补全。 */
static void collect_edges(const struct xime_engine *e, int i, int *out_n)
{
    int len = e->len;
    int rem = len - i;
    int n = 0, L;

    for (L = 1; L <= rem && L <= XIME_MAX_KEY; L++) {
        struct xime_hit hits[XIME_HITS];
        int nh = dict_exact(e->input + i, L, hits, XIME_HITS), k;

        for (k = 0; k < nh; k++)
            add_edge(&n, i + L, word_cost(hits[k].freq), hits[k].val);
    }

    /* 简拼：每个音节只打首字母，"nh" -> 你好/南海 */
    for (L = 2; L <= rem && L <= 6; L++) {
        struct xime_hit hits[XIME_HITS];
        int nh = dict_initials(e->input + i, L, hits, XIME_HITS), k;

        for (k = 0; k < nh; k++)
            add_edge(&n, i + L, word_cost(hits[k].freq) + INITIALS_PENALTY, hits[k].val);
    }

    /* 尾部音节没输完：用词条补全，代价随缺字数量增加 */
    if (rem > 0) {
        struct xime_hit hits[XIME_HITS];
        int nh = dict_prefix(e->input + i, rem, 3, hits, XIME_HITS), k;

        for (k = 0; k < nh; k++) {
            float extra = (float)((int)hits[k].len - rem);

            add_edge(&n, len,
                     word_cost(hits[k].freq) + PARTIAL_BASE + PARTIAL_SLOPE * (extra - 1.0f),
                     hits[k].val);
        }
    }
    *out_n = n;
}

void engine_reset(struct xime_engine *e)
{
    memset(e, 0, sizeof(*e));
    e->preedit[0] = 0;
}

void engine_recompute(struct xime_engine *e)
{
    int i, k, p;

    e->ncand = 0;
    e->preedit[0] = 0;
    if (e->len <= 0)
        return;

    /* u4f60 -> 你 */
    if (e->input[0] == 'u' && e->len >= 5) {
        uint32_t cp = 0;
        int ok = 1;

        for (k = 1; k < e->len; k++) {
            int h = hex_val(e->input[k]);

            if (h < 0) {
                ok = 0;
                break;
            }
            cp = cp * 16u + (uint32_t)h;
        }
        if (ok && cp > 0 && cp <= 0x10FFFF && !(cp >= 0xD800 && cp <= 0xDFFF)) {
            char enc[8];
            size_t el = utf8_encode(cp, enc);

            enc[el] = 0;
            snprintf(e->preedit, sizeof(e->preedit), "U+%04X", (unsigned)cp);
            snprintf(e->cands[0], XIME_CAND_MAX, "%s", enc);
            e->cost[0] = 0.0f;
            e->ncand = 1;
            return;
        }
    }

    snprintf(e->preedit, sizeof(e->preedit), "%s", e->input);
    if (!dict_max_syllable_len())
        return;

    for (i = 0; i <= e->len; i++)
        g_pn[i] = 0;
    g_pn[0] = 1;
    g_path[0][0].cost = 0.0f;
    g_path[0][0].text[0] = 0;

    /* 前向 N-best：所有边都从已到达的位置向右延伸，因此按 i 递增扫描即拓扑序 */
    for (i = 0; i < e->len; i++) {
        int ne = 0;

        if (g_pn[i] == 0)
            continue;
        collect_edges(e, i, &ne);
        for (p = 0; p < g_pn[i]; p++) {
            for (k = 0; k < ne; k++) {
                char text[XIME_TEXT_MAX];

                join_text(text, sizeof(text), g_path[i][p].text, g_edge[k].val);
                push_path(g_edge[k].to, text, g_path[i][p].cost + g_edge[k].cost);
            }
        }
    }

    for (i = 0; i < g_pn[e->len] && e->ncand < XIME_MAX_CANDS; i++) {
        snprintf(e->cands[e->ncand], XIME_CAND_MAX, "%s", g_path[e->len][i].text);
        e->cost[e->ncand] = g_path[e->len][i].cost;
        e->ncand++;
    }
}

int engine_push_char(struct xime_engine *e, char c)
{
    if (e->len >= XIME_MAX_INPUT)
        return -1;
    e->input[e->len++] = c;
    e->input[e->len] = 0;
    engine_recompute(e);
    return 0;
}

int engine_backspace(struct xime_engine *e)
{
    if (e->len <= 0)
        return 0;
    e->input[--e->len] = 0;
    engine_recompute(e);
    return 1;
}

int engine_active(const struct xime_engine *e)
{
    return e->len > 0;
}

const char *engine_inline_preedit(struct xime_engine *e, char *buf, size_t cap)
{
    size_t len = 0;
    int i;

    buf[0] = 0;
    len = utf8_append(buf, cap, len, e->preedit);
    if (e->ncand > 0) {
        len = utf8_append(buf, cap, len, "  ");
        for (i = 0; i < e->ncand; i++) {
            char tmp[192];

            snprintf(tmp, sizeof(tmp), "%d.%s ", i + 1, e->cands[i]);
            len = utf8_append(buf, cap, len, tmp);
        }
    }
    return buf;
}
