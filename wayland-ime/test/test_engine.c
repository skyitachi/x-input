/* 引擎单元测试：不依赖 Wayland，直接验证解码质量与边界行为。
 *
 * 这些用例是"输入法手感"的回归基线：每条都对应一个真实可能踩到的坑
 * （多音字、ü 的写法、简拼、未输完的音节、无效输入）。
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../src/dict.h"
#include "../src/engine.h"

static int g_fail;
static int g_pass;

static void check(int cond, const char *what)
{
    if (cond) {
        g_pass++;
    } else {
        g_fail++;
        fprintf(stderr, "FAIL: %s\n", what);
    }
}

static void type(struct xime_engine *e, const char *s)
{
    while (*s)
        engine_push_char(e, *s++);
}

/* 输入 input，断言首选是 expect */
static void case_expect(const char *input, const char *expect)
{
    struct xime_engine e;
    char msg[192];

    engine_reset(&e);
    type(&e, input);
    snprintf(msg, sizeof(msg), "输入 \"%s\" 首选应为 \"%s\"（实际 \"%s\", %d 个候选）",
             input, expect, e.ncand ? e.cands[0] : "(无)", e.ncand);
    check(e.ncand > 0 && strcmp(e.cands[0], expect) == 0, msg);
}

/* 断言某候选集合里包含 expect（用于简拼、补全这类允许并列的场景） */
static void case_contains(const char *input, const char *expect)
{
    struct xime_engine e;
    char msg[192];
    int i, found = 0;

    engine_reset(&e);
    type(&e, input);
    for (i = 0; i < e.ncand; i++)
        if (strcmp(e.cands[i], expect) == 0)
            found = 1;
    snprintf(msg, sizeof(msg), "输入 \"%s\" 的候选应包含 \"%s\"（共 %d 个）", input, expect, e.ncand);
    check(found, msg);
}

int main(void)
{
    struct xime_engine e;
    const char *user_dict = "/tmp/xime-test-dict.txt";
    FILE *f;

    /* 先写一份用户词表，验证它与内置词库的合并优先级 */
    f = fopen(user_dict, "w");
    if (f) {
        fprintf(f, "ceshi\t测十 侧视\n");
        fclose(f);
    }
    check(dict_init(user_dict) > 0, "用户词表被载入");

    /* 词库规模：太小说明词库生成或打包出了问题 */
    check(dict_word_count() > 100000, "内置词条数 > 10 万");

    /* 音节判定 */
    check(dict_is_syllable("ni") == 1, "ni 是合法音节");
    check(dict_is_syllable("zhuang") == 1, "zhuang 是合法音节");
    check(dict_is_syllable("nih") == 0, "nih 不是完整音节");
    check(dict_is_syllable("zzz") == 0, "zzz 不是音节");

    /* 基础词组与长句：N-best 解码的核心能力 */
    case_expect("nihao", "你好");
    case_expect("zhongguo", "中国");
    case_expect("shurufa", "输入法");
    case_expect("woaini", "我爱你");
    case_expect("jintian", "今天");
    case_expect("jintiantianqihenhao", "今天天气很好");
    case_expect("woshiyigexuesheng", "我是一个学生");
    case_expect("nihaoma", "你好吗");

    /* 单字与同音排序 */
    case_expect("ni", "你");
    case_expect("wo", "我");

    /* 多音字：必须靠"整词读音"而不是单字首选读音 */
    case_expect("yinhang", "银行");
    case_expect("xingzou", "行走");
    case_expect("chongqing", "重庆");

    /* ü 写成 v */
    case_expect("lvxing", "旅行");
    case_expect("nvhai", "女孩");

    /* 未输完的尾部音节：靠词条补全 */
    case_expect("nih", "你好");
    case_expect("zhongg", "中国");
    case_contains("m", "吗");

    /* 简拼 */
    case_contains("nh", "你好");
    case_contains("zg", "中国");

    /* 一音多词：候选必须给出多种切分，而不是只换首字 */
    engine_reset(&e);
    type(&e, "nihao");
    check(e.ncand >= 3, "nihao 至少 3 个候选");
    check(e.ncand >= 2 && strcmp(e.cands[0], e.cands[1]) != 0, "候选之间不重复");
    check(e.cost[0] <= e.cost[1], "候选按代价升序");

    /* 反复重算结果稳定（无状态泄漏） */
    {
        struct xime_engine e2;

        engine_reset(&e2);
        type(&e2, "nihao");
        check(e.ncand == e2.ncand && strcmp(e.cands[0], e2.cands[0]) == 0, "重复解码结果一致");
    }

    /* 用户词表优先于内置词条 */
    engine_reset(&e);
    type(&e, "ceshi");
    check(e.ncand > 0 && strcmp(e.cands[0], "测十") == 0, "用户词表优先（ceshi -> 测十）");

    /* 非法输入：没有候选，但 preedit 保留原串 */
    engine_reset(&e);
    type(&e, "vvvv");
    check(e.ncand == 0, "vvvv 没有候选");
    check(strcmp(e.preedit, "vvvv") == 0, "vvvv preedit 保留原串");

    /* 退格 */
    engine_reset(&e);
    type(&e, "nihao");
    engine_backspace(&e);
    check(strcmp(e.input, "niha") == 0, "退格后输入串为 niha");
    check(engine_backspace(&e) == 1, "还有内容可退");
    engine_reset(&e);
    check(engine_backspace(&e) == 0, "空缓冲退格返回 0");

    /* Unicode 直输 */
    engine_reset(&e);
    type(&e, "u4f60");
    check(e.ncand == 1 && strcmp(e.cands[0], "你") == 0, "u4f60 -> 你");
    check(strcmp(e.preedit, "U+4F60") == 0, "unicode 模式 preedit 显示码位");

    /* 内联候选模式 */
    engine_reset(&e);
    type(&e, "ni");
    {
        char buf[512];

        engine_inline_preedit(&e, buf, sizeof(buf));
        check(strstr(buf, "1.你") != NULL, "内联 preedit 含 1.你");
    }

    /* 输入串上限保护 */
    engine_reset(&e);
    {
        int i, rc = 0;

        for (i = 0; i < XIME_MAX_INPUT + 10 && rc == 0; i++)
            rc = engine_push_char(&e, 'n');
        check(rc == -1, "超过上限时 push 返回 -1");
        check(e.len == XIME_MAX_INPUT, "输入串被截断到上限");
    }

    printf("%d passed, %d failed\n", g_pass, g_fail);
    return g_fail ? 1 : 0;
}
