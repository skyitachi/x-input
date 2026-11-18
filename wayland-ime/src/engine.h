#ifndef XIME_ENGINE_H
#define XIME_ENGINE_H

#include <stddef.h>

#define XIME_MAX_INPUT 40
#define XIME_MAX_CANDS 9
#define XIME_CAND_MAX 128

struct xime_engine {
    char input[XIME_MAX_INPUT + 1]; /* 原始输入串（小写字母 / 十六进制） */
    int len;
    char preedit[160];
    char cands[XIME_MAX_CANDS][XIME_CAND_MAX];
    float cost[XIME_MAX_CANDS]; /* 各候选的解码代价，越小越可信（调试用） */
    int ncand;
};

void engine_reset(struct xime_engine *e);
void engine_recompute(struct xime_engine *e);

/* 追加一个可组字字符（a-z 0-9）。返回 0 成功，-1 输入串已满。 */
int engine_push_char(struct xime_engine *e, char c);

/* 退格。返回 1 表示有内容被删除，0 表示已经空了。 */
int engine_backspace(struct xime_engine *e);

/* 输入串是否非空（决定按键是否由输入法消费） */
int engine_active(const struct xime_engine *e);

/* 无弹窗降级：把候选内联进 preedit 文本，由应用自身字体渲染 */
const char *engine_inline_preedit(struct xime_engine *e, char *buf, size_t cap);

#endif
