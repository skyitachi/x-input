/*
 * qt_probe —— 最小 Qt6 Widgets 客户端，用来验证「输入法发出去的 commit 到没到 Qt」
 *
 * 微信是静态链接的 Qt6 原生 Wayland 程序，输入走 zwp_text_input_v3。
 * 这个探针用同一个 Qt 代码路径（QLineEdit + QWaylandTextInputv3），
 * 把收到的 QInputMethodEvent（preedit / commit）和按键原样打印，
 * 用来区分「输入法没发」「合成器没转」「客户端没处理」。
 *
 * 用法：
 *     ./qt_probe                       # 窗口出现，会自动把焦点放在输入框
 *     hyprctl dispatch focuswindow class:qtprobe
 *     wtype nihao; wtype -k space      # 或者你直接手动敲
 *
 * 判读：
 *     [qt] IME commit="你好"      → Qt 侧收到了，链路是通的
 *     只有 keyPress 没有 IME      → 客户端把按键自己吞了，没走输入法
 *     连 keyPress 都没有          → 键盘焦点/注入路径有问题
 *
 * 编译（见同目录 Makefile 的 qt_probe 目标）：
 *     g++ -O2 -fPIC $(pkg-config --cflags Qt6Widgets) qt_probe.c \
 *         $(pkg-config --libs Qt6Widgets) -o qt_probe
 */
#include <QApplication>
#include <QLineEdit>
#include <QLabel>
#include <QVBoxLayout>
#include <QWidget>
#include <QKeyEvent>
#include <QInputMethodEvent>
#include <QInputMethod>
#include <QTimer>
#include <QGuiApplication>
#include <cstdio>
#include <cstdarg>
#include <cstdlib>

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stdout, fmt, ap);
    va_end(ap);
    fputc('\n', stdout);
    fflush(stdout);
}

/* 把 QInputMethodEvent 里的属性压缩成一行，方便看光标/下划线是否正常 */
static const char *attrSummary(const QInputMethodEvent &e) {
    static char buf[256];
    int n = snprintf(buf, sizeof buf, "[");
    for (const auto &a : e.attributes()) {
        const char *kind = a.type == QInputMethodEvent::Cursor     ? "cursor"
                         : a.type == QInputMethodEvent::TextFormat ? "fmt"
                         : a.type == QInputMethodEvent::Selection  ? "sel"
                                                                   : "?";
        n += snprintf(buf + n, sizeof buf - (size_t)n, " %s@%d+%d", kind, a.start, a.length);
        if (n >= (int)sizeof buf - 24)
            break;
    }
    snprintf(buf + n, sizeof buf - (size_t)n, " ]");
    return buf;
}

class ProbeEdit : public QLineEdit {
public:
    using QLineEdit::QLineEdit;

protected:
    void inputMethodEvent(QInputMethodEvent *e) override {
        say("[qt] IME preedit=\"%s\" commit=\"%s\" repl=%d+%d attrs=%s",
            qPrintable(e->preeditString()), qPrintable(e->commitString()),
            e->replacementStart(), e->replacementLength(),
            attrSummary(*e));
        QLineEdit::inputMethodEvent(e);
        say("[qt]    → 控件里的文本现在是 \"%s\"", qPrintable(text()));
    }

    void keyPressEvent(QKeyEvent *e) override {
        say("[qt] keyPress key=0x%x text=\"%s\"", e->key(), qPrintable(e->text()));
        QLineEdit::keyPressEvent(e);
    }

    void focusInEvent(QFocusEvent *e) override {
        say("[qt] focusIn (%d)", e->reason());
        QLineEdit::focusInEvent(e);
    }

    void focusOutEvent(QFocusEvent *e) override {
        say("[qt] focusOut (%d)", e->reason());
        QLineEdit::focusOutEvent(e);
    }

    /* Qt 查询给合成器的光标矩形就来自这里，顺手打出来对账 */
    QVariant inputMethodQuery(Qt::InputMethodQuery q) const override {
        QVariant v = QLineEdit::inputMethodQuery(q);
        if (q == Qt::ImCursorRectangle) {
            static QRect last;
            QRect r = v.toRect();
            if (r != last) {
                last = r;
                say("[qt] ImCursorRectangle = %d,%d %dx%d", r.x(), r.y(), r.width(), r.height());
            }
        }
        return v;
    }
};

int main(int argc, char **argv) {
    setvbuf(stdout, nullptr, _IOLBF, 0);
    QApplication app(argc, argv);
    QGuiApplication::setApplicationName(QStringLiteral("qtprobe"));
    QGuiApplication::setDesktopFileName(QStringLiteral("qtprobe"));

    QWidget w;
    w.setWindowTitle(QStringLiteral("qt_probe —— 点这里，然后打字"));
    w.resize(640, 160);

    auto *layout = new QVBoxLayout(&w);
    auto *tip = new QLabel(QStringLiteral("用输入法在这里打字，本窗口会把收到的 preedit / commit 打到终端"));
    tip->setWordWrap(true);
    auto *edit = new ProbeEdit(&w);
    edit->setPlaceholderText(QStringLiteral("在这里输入…"));
    edit->setMinimumHeight(48);
    auto *mirror = new QLabel(&w);

    layout->addWidget(tip);
    layout->addWidget(edit);
    layout->addWidget(mirror);

    w.show();
    w.raise();
    w.activateWindow();
    edit->setFocus(Qt::OtherFocusReason);

    /* 每 500ms 把控件里的文本抄一份，肉眼对着窗口看也能确认 */
    QTimer timer;
    QObject::connect(&timer, &QTimer::timeout, [&] {
        mirror->setText(QStringLiteral("控件文本：") + edit->text());
    });
    timer.start(500);

    /* 把 Qt 侧看到的输入法能力打出来：谁在给 Qt 供事件 */
    QObject::connect(qApp->inputMethod(), &QInputMethod::cursorRectangleChanged, [] {
        say("[qt] inputMethod cursorRectangleChanged");
    });

    say("[qt] 已启动，app_id=qtprobe，qt=%s", qVersion());
    return app.exec();
}
