# 调研：给 xime 加一个 fcitx5 DBus 前端

> 目标读者：决定要不要做这件事的人。
> 结论先行：**技术上可行，但有一步必须先用 100 行代码验证，验证不过就不要投入。**
> 文档日期：2026-11-19。协议依据：`fcitx/fcitx5-qt` master、`fcitx/fcitx5` master、微信 4.x Linux 二进制。

---

## 一、为什么需要考虑这条路

xime 现在是**合成器级**输入法：只实现 `zwp_input_method_v2`，靠合成器把按键转给它，再通过 `zwp_text_input_v3` 把结果送回应用。这条链有三个硬边界：

| 边界 | 原因 | 影响 |
|---|---|---|
| GNOME | mutter 至今未实现 `zwp_input_method_v2`（`libmutter-18` 里该符号计数为 0） | xime 在 GNOME 上完全不可用 |
| X11 / XWayland | X 客户端走 XIM，没有 input-method-v2 | 微信、WPS 等 XWayland 应用用不了 |
| 自带输入法客户端的应用 | 见下 | 微信里不上屏 |

而 fcitx5 在**同一个微信**里能用。原因是微信把 fcitx 的客户端编译进了自己的二进制，走的是会话总线，跟合成器无关：

```
$ strings -a /opt/wechat-universal/wechat | grep -i fcitx
fcitx::QFcitxPlatformInputContext
fcitx::FcitxCandidateWindow
fcitx::FcitxQtWatcher / FcitxQtInputMethodProxy
org.fcitx.Fcitx.InputContext / org.fcitx.Fcitx.InputContext1
org.fcitx.Fcitx5
FCITX_DBUS_ADDRESS
```

所以「让 xime 在微信里能用」和「让 xime 在 GNOME 上能用」其实是**同一件事**：给 xime 加一个 DBus 前端，让它假装成 fcitx5 守护进程。所有自带 fcitx5-qt / fcitx5-gtk 的应用都会连上来。

覆盖面变化：

| 环境 | 现在（Wayland 前端） | 加上 DBus 前端 |
|---|---|---|
| Hyprland / sway / KDE | ✅ | ✅（两种前端并存） |
| GNOME Wayland | ❌ 无协议 | ✅ GTK / Qt 应用 |
| X11 会话 | ❌ | ✅ |
| XWayland 应用（微信、WPS） | ❌ | ✅ |
| Electron（走 Qt 模块） | ❌ | ✅ |
| 只认 IBus 的应用（Firefox 等） | ❌ | ❌ 仍需第三个前端 |

---

## 二、协议的事实标准在哪

没有 RFC。事实标准是 fcitx5-qt 的接口 XML 加上 fcitx5 守护端的 vtable 注册：

| 文件 | 作用 |
|---|---|
| `fcitx5-qt/qt5/dbusaddons/interfaces/org.fcitx.Fcitx.InputMethod1.xml` | 创建输入上下文（12 行） |
| `fcitx5-qt/qt5/dbusaddons/interfaces/org.fcitx.Fcitx.InputContext1.xml` | 输入上下文的全部方法与信号（117 行） |
| `fcitx5-qt/qt5/dbusaddons/interfaces/org.fcitx.Fcitx.Controller1.xml` | 配置界面用（**不需要实现**） |
| `fcitx5-qt/qt5/dbusaddons/fcitxqtinputcontextproxy_p.h` | 客户端**实际怎么连、传什么参数** |
| `fcitx5-qt/qt5/platforminputcontext/qfcitxplatforminputcontext.cpp` | 客户端拿到信号后怎么落到应用 |
| `fcitx5/src/frontend/dbusfrontend/dbusfrontend.cpp` | 守护端的行为参考实现（含 batched 变体） |

> qt6 目录下的多数文件是指向 qt5 的符号链接，读一份即可。

---

## 三、拓扑与握手

```
应用进程（微信 / WPS / 任意 Qt 程序）
├── QDBusServiceWatcher 监听 "org.fcitx.Fcitx5"
├── 连 service owner 的 /org/freedesktop/portal/inputmethod
│   └── org.fcitx.Fcitx.InputMethod1.CreateInputContext(a(ss)) → (o path, ay uuid)
└── 在返回的 path 上
    └── org.fcitx.Fcitx.InputContext1
        ├── 方法：FocusIn / FocusOut / Reset / SetCursorRect(V2) / SetCapability /
        │        SetSupportedCapability / SetSurroundingText(Position) / ProcessKeyEvent /
        │        SelectCandidate / PrevPage / NextPage / InvokeAction / DestroyIC
        └── 信号：CommitString / UpdateClientSideUI / UpdateFormattedPreedit /
                 DeleteSurroundingText / ForwardKey / CurrentIM / NotifyFocusOut
```

关键常量（来自 `fcitxqtwatcher_p.h`）：

| 项 | 值 |
|---|---|
| 服务名 | `org.fcitx.Fcitx5`（portal 名 `org.freedesktop.portal.Fcitx`，Qt 默认 `setWatchPortal(true)` 两边都监听） |
| InputMethod1 路径（主总线） | `/org/freedesktop/portal/inputmethod` |
| InputMethod1 路径（portal 总线，兼容） | `/inputmethod` |
| IC 对象路径 | **由守护端自选**，通过 CreateInputContext 的返回值告知 |
| IC uuid | 16 字节；X11 下 Qt 用它做 focus group，Wayland 下不用 |

`CreateInputContext` 的参数（客户端固定发这些键）：

| key | value |
|---|---|
| `program` | 应用可执行文件名 |
| `display` | `wayland:` 或 `x11:` |
| `clientControlVirtualkeyboardShow` | `true` |
| `clientControlVirtualkeyboardHide` | `false` |

---

## 四、接口清单

### 4.1 InputMethod1（只需两个方法）

| 方法 | 入参 | 出参 | 说明 |
|---|---|---|---|
| `CreateInputContext` | `a(ss)` | `o` 路径 + `ay` uuid | 每次新窗口一个 |
| `Version` | – | `u` | 返回 `1` |

### 4.2 InputContext1 方法

| 方法 | 签名 | MVP | 说明 |
|---|---|---|---|
| `FocusIn` | `()` | ✅ | 进入输入框 |
| `FocusOut` | `()` | ✅ | 离开输入框，需清 preedit |
| `Reset` | `()` | ✅ | 清空组字状态 |
| `SetCursorRect` | `iiii` | ✅ | 候选窗定位（Wayland 下若声明 `RelativeRect` 则为窗口相对坐标） |
| `SetCursorRectV2` | `iiiid` | 可选 | 多一个缩放因子 |
| `SetSupportedCapability` | `t` | ✅ | 客户端声明它认识哪些位，Qt 固定发 `0x1ffffffffff` |
| `SetCapability` | `t` | ✅ | **必须解析**：密码框透传、ClientSideInputPanel 门控都靠它 |
| `SetSurroundingText` | `suu` | 可选 | 上下文文本 + 光标/锚点 |
| `SetSurroundingTextPosition` | `uu` | 可选 | 仅更新位置 |
| `ProcessKeyEvent` | `uuub u` → `b` | ✅ | **核心**：`keyval, keycode, state, isRelease, time` → 是否消费 |
| `ProcessKeyEventBatch` | 同名 + 额外出参 | ❌ | fcitx5 新版扩展，Qt 默认不用 |
| `SelectCandidate` | `i` | ✅ | 点候选 / 数字键选词 |
| `PrevPage` / `NextPage` | `()` | ✅ | 翻页 |
| `InvokeAction` | `ui` | ❌ | 动作按钮，可忽略 |
| `DestroyIC` | `()` | ✅ | 窗口销毁，释放状态 |
| `Show/HideVirtualKeyboard`、`IsVirtualKeyboardVisible` | – | ❌ | 移动端才有意义 |

### 4.3 InputContext1 信号

| 信号 | 签名 | MVP | 说明 |
|---|---|---|---|
| `CommitString` | `s` | ✅ | 上屏文本 |
| `UpdateClientSideUI` | `a(si)ia(si)a(si)a(ss)iibb` | ✅ | **推荐主用**：preedit + 上下辅助行 + 候选 + 页码标志，一次发完，由应用自己画 |
| `UpdateFormattedPreedit` | `a(si)i` | 可选 | 只有 preedit，需外部 UI 画候选（kimpanel / classic ui） |
| `DeleteSurroundingText` | `iu` | 后期 | 替换选区 |
| `ForwardKey` | `uub` | 可选 | 让应用重新注入一个按键（也可以直接让 ProcessKeyEvent 返回 false） |
| `CurrentIM` | `sss` | 可选 | 当前输入法名/唯一名/语言，用于状态提示 |
| `NotifyFocusOut` | `()` | 不用 | 服务端要求失焦 |
| `VirtualKeyboardVisibilityChanged` | `b` | 不用 | |

### 4.4 数据结构

```
FcitxQtFormattedPreedit = (s: 文本, i: 格式位)
  格式位：Underline=1<<3、HighLight=1<<4、DontCommit=1<<5、Bold=1<<6、Strike=1<<7、Italic=1<<8
FcitxQtStringKeyValue  = (s: 标签, s: 文本)      // 候选：label + 词
```

D-Bus 类型签名分别为 `a(si)` 与 `a(ss)`。Qt 侧靠 `registerFcitxQtDBusTypes()` 注册这两个自定义类型；守护端只要结构对齐即可。

### 4.5 能力位（`common/fcitxflags.h`）

Qt 客户端固定声明：

```
Preedit(1<<1) | FormattedPreedit(1<<4) | ClientUnfocusCommit(1<<5) |
GetIMInfoOnFocus(1<<23) | KeyEventOrderFix(1<<37) | ReportKeyRepeat(1<<38) |
ClientSideInputPanel(1<<39)   [+ RelativeRect(1<<24) 仅在 Wayland 平台]
```

xime 必须关心的三位：

| 位 | 含义 | xime 的处理 |
|---|---|---|
| `Password(1<<3)` / `Sensitive(1<<36)` | 密码框 | 全键透传，不组字 |
| `ClientSideInputPanel(1<<39)` | 应用自己画候选窗 | 发 `UpdateClientSideUI`，不接自己的 popup |
| `RelativeRect(1<<24)` | 光标矩形是窗口相对坐标 | 只影响候选窗位置，DBus 前端不需要 |

---

## 五、微信实际会调用什么

从二进制里能直接验到的字符串（每个都是独立符号）：

| 符号 | 命中 | 结论 |
|---|---|---|
| `ProcessKeyEvent` | 4 | 走按键通道 |
| `CreateInputContext` | 4 | 走标准建上下文流程 |
| `SetSupportedCapability` | 1 | 会做能力协商 |
| `SetCursorRectV2` | 1 | 用 V2 版本 |
| `UpdateClientSideUI` | 1 | **候选窗由微信自己画** |
| `clientControlVirtualkeyboardShow` | 1 | 与 fcitx5-qt 源码逐字一致 |

也就是说微信内置的就是**未经改动的 fcitx5-qt 客户端逻辑**（至少这部分没改）。它对 xime 的要求就是：按第四章的清单应答。

**一个有利之处**：因为 `ClientSideInputPanel` 恒置位，候选窗（含 preedit 显示）由微信进程内的 `FcitxCandidateWindow` 绘制。xime 在 DBus 前端里**完全不需要处理窗口、定位、字体、主题**——那是 Wayland 前端才操心的事。这也顺带消除了现在那个 `(0,-16) 0×16` 无效光标矩形的问题。

---

## 六、最大的未知 —— 已实测通过

这是整份文档里唯一一条「不验证就别动手」的项。

Qt 模块把上屏落到应用的方式是：

```cpp
void QFcitxPlatformInputContext::commitString(const QString &str) {
    QObject *input = qGuiApp->focusObject();
    if (!input) return;                    // ← 同一个依赖点
    QInputMethodEvent event;
    event.setCommitString(str);
    QCoreApplication::sendEvent(input, &event);
}
```

而 Wayland 路径里，`QWaylandTextInputv3::zwp_text_input_v3_done()` 最后也是 `sendEvent(focusObject, &event)`。**两条路径在最后一步是同一个动作。**

但我们观察到：Wayland 路径下 xime 已经发出 `commit "了" (serial=68)`，微信界面毫无反应。这留下两种可能：

| 假设 | 推论 |
|---|---|
| **A. 微信 patch 了 fcitx 客户端**，把 `commitString` 改成直接喂自己的编辑器 | DBus 前端可行，Wayland 路径注定不通 |
| **B. Wayland 路径失败在更早的位置**（enter 未送达、surface 不匹配等），微信的 focusObject 其实正常 | DBus 前端同样可行 |

两种假设下**结论都是「值得做」**，但前提是 A/B 至少一个成立，而不是第三种情况（「微信编辑器根本不接受 QInputMethodEvent」）——若是第三种，DBus 前端也一样不上屏，整个方向作废。

### Phase 0：用 100 行代码判定（半天）

写一个 **假 fcitx5 守护**（Python + `dbus-next` 即可，不需要 Rust）：

1. 抢 `org.fcitx.Fcitx5` 服务名（先 `systemctl --user stop omarchy-fcitx5`）
2. 在 `/org/freedesktop/portal/inputmethod` 提供 `InputMethod1.CreateInputContext` → 返回固定路径 + 16 字节 uuid
3. 在该路径提供 `InputContext1`，所有方法都**立即返回成功**（`ProcessKeyEvent` 必须回 `false`，否则按键被吞）
4. 收到任意 `ProcessKeyEvent` 时，发一次 `CommitString("被xime接管了")`
5. 保持微信的 `QT_IM_MODULE=fcitx`（默认值，**不用改启动项**），重启微信，点进聊天框敲一个键

判定：

| 结果 | 含义 | 下一步 |
|---|---|---|
| 聊天框出现「被xime接管了」 | 微信确实通过 DBus 上屏 | ✅ 立项 |
| 什么都没有 | 落在第三种情况 | ❌ 放弃这条路，维持 fcitx5 |

这个实验还有一个额外价值：它顺便验证了「服务名独占 = 与 fcitx5 互斥」这个约束的真实体感，以及微信对 `ProcessKeyEvent` 回复速度的容忍度。

### Phase 0 实测结果：**通过**（2026-11-19）

用 `dbus-python`（约 250 行，`/tmp/fake_fcitx5.py`）实现了假守护，实测结论如下。

**成立的部分**

| 观察 | 说明 |
|---|---|
| 微信**自动重连**，不用重启 | 停掉 fcitx5 → 起假守护，微信的 `FcitxQtWatcher` 立刻重建上下文，日志里直接看到 `CreateInputContext` 的 `program='wechat'` |
| 上下文入参与预期完全一致 | `[('program','wechat'), ('display','wayland:'), ('clientControlVirtualkeyboardShow','true'), ('clientControlVirtualkeyboardHide','false')]` |
| 能力位正常协商 | `SetSupportedCapability 0x1ffffffffff` → `SetCapability 0xe801800032`，`bit39 ClientSideInputPanel` 恒开 |
| 光标矩形**有效** | `SetCursorRectV2 277,1253 2x22 @1.0`，与 Wayland 路径下那个 `(0,-16) 0×16` 形成鲜明对比 |
| `CommitString` 一路上屏 | 敲键后微信回传 `SetSurroundingText 'XIME接管成功' cursor=8`，敲第二次变成 `cursor=16` —— **输入框真的收到了** |

**据此可以确认**

1. 假设 **A 或 B 成立**，第三种情况（微信编辑器不接受 `QInputMethodEvent`）被排除。DBus 前端这条路可行，**立项**。
2. Wayland 路径失败的原因不在「微信不消费事件」，而在更早的协议环节（enter 未送达 / surface 不匹配 / 时序）。这条推断留待日后单独查，但已不影响 DBus 方向。
3. `ProcessKeyEvent` 返回 `true`（表示已消费）时，微信确实不再插入原始字符，说明按键「吞/放」的控制权在守护这一侧，行为正确。

**Implementation notes（踩到的）**

- 服务名要**同时**占 `org.fcitx.Fcitx5` 和 `org.fcitx.Fcitx-0`（fcitx4 兼容名，微信两套代理都带）。
- `dbus-python` 里获取调用者必须用 `sender_keyword="sender"`，不能自己反查。
- `gdbus call` 会把整数推断成 int32，与 `u` 签名不匹配而报 `UnknownMethod`；验证签名要用 Python 客户端。
- 停 fcitx5 后系统即无输入法（服务名独占的必然结果），实验期间需接受这个空窗。

### Phase 0.5：候选窗确实由客户端自绘（2026-11-19）

同一个脚本的 `candidate` 模式：`a-z` 累积组字串、每次按键推一次
`UpdateClientSideUI`、空格提交首候选。截图见
`tools/phase0-candidate-window.png`。

| 观察 | 结果 |
|---|---|
| 候选窗出现位置 | 输入框右下角，跟随光标 |
| 渲染内容 | 一行组字串 + 5 条候选横排，首候选蓝色高亮 |
| 用到的数据结构 | `UpdateClientSideUI(a(si) i, a(si), a(si), a(ss), i, i, b, b)` 原样可用 |
| 上屏 | 空格 → `CommitString("你")`，输入框直接出现「你」 |

**结论：DBus 前端不需要一行窗口代码。** 这与 Wayland 前端形成鲜明对比——那边要
自己维护 surface、算光标、处理缩放，还踩过微信给出 `(0,-16) 0×16` 无效矩形的坑。
在 DBus 这条路上，坐标系、缩放、主题、字体全部由应用负责。

**额外发现**

- `quickshell`（Omarchy 的状态栏/启动器外壳）也主动 `CreateInputContext` 并 FocusIn。
  说明这个前端的受益者不止微信，还有 shell 自身那些带输入框的组件。
- 客户端在输入上下文重建之后**不会自动补发 `FocusIn`**。守护进程中途重启（哪怕
  微信立刻重连、`SetCapability` 也正常）时，按键不会转发，必须让输入框重新获得焦点
  才恢复。实现时要么接受这个前提，要么在 `CreateInputContext` 返回后主动引导客户端聚焦。

**新增的坑**

- `ProcessKeyEvent` 的按下与松开都会到达。只有 `type=true`（按下）可以累积字符，
  否则组字串会出现 `ppppnnii` 这样的重复。松开事件按「已消费」处理即可。
- 不要无条件返回 `true`：那会把应用的快捷键（Ctrl+C 之类）一并吞掉。真实实现要
  按 fcitx5 的规则放行——通常只在有组字或有按键映射时才吞。

---

## 七、最小可行子集（MVP）

**做：**

- 服务 `org.fcitx.Fcitx5`，路径 `/org/freedesktop/portal/inputmethod`
- `InputMethod1.CreateInputContext` + `Version`
- IC 方法：`FocusIn`、`FocusOut`、`Reset`、`SetCapability`、`SetSupportedCapability`、`SetCursorRect`、`SetCursorRectV2`、`ProcessKeyEvent`、`SelectCandidate`、`PrevPage`、`NextPage`、`DestroyIC`
- IC 信号：`CommitString`、`UpdateClientSideUI`、`CurrentIM`
- 能力位解析：`Password` / `Sensitive` → 透传，`ClientSideInputPanel` → 客户端画窗
- 候选翻页、英文前缀补全、简繁切换（第 13 章）：第一版未做，第二轮补齐

**不做（第一版刻意排除）：**

- fcitx4 兼容接口 `org.fcitx.Fcitx.InputContext`（老应用才会用）
- `InvokeAction`、虚拟键盘、`DeleteSurroundingText`（先返回错误，Qt 会忽略）
- `SetSurroundingText`（可选，词库联想用；第一版按空处理）
- GTK 模块（`fcitx5-gtk`）的内部逻辑与 IBus 前端

**`Controller1` 已从「不做」改成「必须做」**（2026-11-19 实测修正）：
omarchy 的状态栏（`quickshell` 里跑 `indicator.py`）每 500ms 读一次
`CurrentInputMethodGroup` / `InputMethodGroupInfo` / `CurrentInputMethodInfo`。
以前这三个方法全部 `Unknown object '/controller'`，指示器直接抛异常退出，
顶栏的「中／英」就没了——用户感知是「装了 xime 之后别的应用输入法坏了」。
只需实现读接口 + `SetCurrentIM`，不必实现配置类方法。

---

## 八、实现要点与坑

1. **按键必须及时回复**。Qt 的 `filterEvent` 是异步的：先 `return true` 吞掉按键，再按 `ProcessKeyEvent` 的返回值决定要不要重新投递。守护端卡顿 300ms，打字就会一顿一顿；返回错误同样会触发重投，导致乱序。调试期可设 `FCITX_QT_USE_SYNC=1` 强制同步、便于观察。
2. **`state` 的 bit31 表示按键自动重复**（`KeyState::Repeat`），不是修饰键，要屏蔽掉再判断。
3. **必须先 `FocusIn` 再处理按键**，否则候选窗不出现；`FocusOut` 时要把 preedit 清空并提交或丢弃，否则残字会粘到下一个输入框。
4. **`SetCapability` 可能同时含 `Password` 与 `ClientSideInputPanel`**，桌面终端还会带 `SurroundingText`；xime 现有的 `field_passthrough` 逻辑可以直接复用。
5. **`UpdateClientSideUI` 的 9 个参数一个都不能少**，尤其 `candidateIndex`、`layoutHint`、`hasPrev/hasNext`，否则微信的候选窗会错位或空白。
6. **服务名独占**：`org.fcitx.Fcitx5` 只能有一个占用者。xime 的 DBus 前端与真 fcitx5 天然互斥，需要 `xime-switch` 增加一个「fcitx-dbus 模式」，并在启动时检测对方是否在跑。
7. **静态构建友好**：`zbus` 是纯 Rust 的 D-Bus 实现，不依赖 `libdbus`，不会破坏 xime「零动态依赖」的目标。这一点要在选库时守住（不要用 `dbus-rs`）。
8. **多实例/多应用**：每个窗口一个 IC，路径要唯一（例如 `/xime/ic/<n>`），`DestroyIC` 要真的回收，否则状态会串。
9. **`program` 字段可以白名单**：拿到应用名后，特定程序（如终端、密码框）直接进透传模式，比靠 capability 更可靠。
10. **`ProcessKeyEvent` 第四个参数是 `isRelease`，不是 `isPress`**（2026-11-19 踩坑）。
    协议里这个参数叫 `type`、类型是 `b`，很容易想当然当成「是否按下」。写反的后果很隐蔽：
    靠「松开跟随上次消费结果」还能把字组出来，但**每次新组字的第一个键必定漏给应用**，
    表现就是在微信里打出 `n你好`、在 GTK 输入框里多出一个 `n`。判断办法是抓一次总线：
    真实客户端对同一个键先发 `boolean false` 再发 `boolean true`，前者才是按下。
11. **`FocusIn` 里绝对不能清组字或推空 UI**。微信的 fcitx5-qt 客户端**每敲一个键都会发一次
    `FocusIn`**，在 `FocusIn` 里推空的 `UpdateClientSideUI` 等于每个键都抹掉拼音，
    结果是一个字都打不出来。`FocusIn` 只该记账，清组字交给 `FocusOut` / `Reset`。
12. **上屏顺序：先清组字串，再 `CommitString`**。客户端是「预编辑文本画在光标处」的模型，
    先上屏再清会让提交内容与残留预编辑叠在一起。
13. **`CurrentInputMethodInfo` 是 6 个独立字符串出参**，不是单个 `(ssssssb)` 结构体。
    状态栏脚本直接按 `info[4]` 取标签、`info[5]` 取语言，多包一层结构体就会
    `IndexError` 把指示器打挂（zbus 里裸元组就是「多个出参」，别画蛇添足再套一层）。
14. **改完必须重新构建再用**。本次事故的第一层原因是 `target/release/xime` 比源码旧，
    跑的还是「Password 判成 bit0/bit1」的版本，于是所有键都进了透传分支，
    微信和 GTK 应用同时打不出字——看着像协议问题，其实是二进制没更新。

---

## 九、与现有代码的接入方式

现在 `src/main.rs` 把 Wayland 事件循环、输入状态机、候选窗逻辑混在一起。要挂第二个前端，至少需要两层拆分：

```
engine   （已有：组字、候选、词库、简繁）      ← 完全不改
popup    （已有：Wayland surface 绘制）        ← 仅 Wayland 前端使用

frontend trait
├── WaylandFrontend   （现有 main.rs 的逻辑，改造成 trait 实现）
└── FcitxDbusFrontend （新增）
```

trait 大致只需四个方法：

```rust
trait Frontend {
    fn on_key(&mut self, key: KeyEvent) -> bool;   // 是否消费
    fn commit(&mut self, text: &str);
    fn update_preedit(&mut self, preedit: &str, cursor: usize, cands: &[(String, String)], page: (bool, bool));
    fn reset(&mut self);
}
```

DBus 前端的 `on_key` 把 `(keyval, keycode, state)` 还原成 xime 的 `KeyEvent`；`update_preedit` 转成 `UpdateClientSideUI`；`commit` 转成 `CommitString`。**不需要候选窗**，这块代码量因此小很多。

---

## 十、工作量估计

| 阶段 | 内容 | 估计 |
|---|---|---|
| Phase 0 | 假守护验证微信是否上屏 | 0.5 人日 |
| 1 | 前端 trait 抽象 + 现有 Wayland 逻辑改造 | 2 人日 |
| 2 | `zbus` 服务层：3 个接口、20 个方法/信号、类型与错误处理 | 3 人日 |
| 3 | 引擎状态映射：按键还原、capability、preedit/候选打包、页码 | 2 人日 |
| 4 | 联调：QLineEdit 探针 → 微信 → WPS → GNOME GTK 应用 | 1.5 人日 |
| 5 | 打包：`xime-switch` 增加模式、服务名冲突检测、文档 | 1 人日 |

**合计约 10 人日**（不含 GTK/IBus 前端）。若只求「微信能用」，Phase 2/3 可再砍到 6~7 人日。

风险主要在 Phase 4：微信的焦点切换、候选窗位置、密码框识别都只能在真机上看。

---

## 十一、结论

- **方向成立，且已实测**：Phase 0 证明了上屏可用（`SetSurroundingText` 回显出我们发的字符串），
  Phase 0.5 证明了候选窗由客户端自绘（`UpdateClientSideUI` 一推，微信就画出了 5 条候选）。
  剩下的是工程量问题，不是可行性问题。
- **收益超出「微信」**：同一套前端同时解决 GNOME、X11、XWayland、Electron 四类场景，把 xime
  从「Hyprland 的输入法」变成「跨桌面的输入法」。实测中 `quickshell` 也自动连了上来，说明
  覆盖面还包括 shell 自己的搜索框、启动器。
- **成本比预想低**：候选窗、主题、字体、光标定位全归应用管，xime 只发结构化数据。
  这与 Wayland 前端最费劲的那一半正好互补。
- **真正的代价是双前端维护**：两套前端共用引擎，但生命周期、焦点语义、按键吞放规则都不一样。
  若不打算长期维护第二个前端，**维持 fcitx5 仍是理性选择**；xime 保留 Wayland 前端，作为
  Hyprland/KDE 下的轻量替代。

---

## 十二、真实输入验证方法（2026-11-19）

「能出字」不等于「输入正确」。第一版原型在微信里能打出「你好」，但缓冲区里其实是
`n你好`；同时「别的应用输入法像是坏了」。为了把这类问题查干净，改为**三层取证**：

| 层 | 工具 | 看什么 |
|---|---|---|
| 协议层 | `tools/dbus-trace.py` | 同一个按键序列喂给 fcitx5 与 xime，逐条 diff 返回值、preedit、候选、`CommitString` |
| 真实客户端 | `tools/gtk3-probe.py` | GTK3 输入框（走 `im-fcitx5.so`，与 GIMP、gnome 系应用同一条路径），把落进缓冲区的文本实时打出来 |
| 真实按键 | `tools/uinput-inject` | `/dev/uinput` 虚拟键盘，走合成器的完整 XKB，避免 wtype 精简 keymap 导致 `nihao` 变 `12345` |

配合 `dbus-monitor --session`，可以直接看到「真实客户端发出了什么、xime 回了什么」。

### 验证清单（当前全部通过）

| 场景 | 期望 | 结果 |
|---|---|---|
| 首键 `n` 是否漏给应用 | 缓冲区只出现 `你好` | ✅（修 `isRelease` 前是 `n你好`） |
| 数字键选词 | `nihao` + `3` → `呢好` | ✅ |
| 标点 | `nihao` + `.` → `你好.` | ✅ |
| 退格 | 组字中删拼音；无组字时删文本 | ✅ |
| 长词 | `zhongguo` → 中国；`renmin` → 人民 | ✅ |
| 微信候选窗 | 客户端自绘，9 条候选 | ✅ 截图留档 `tools/phase0-candidate-window.png` |
| 顶栏指示器 | 每 500ms 轮询成功，显示「中」 | ✅（此前 `IndexError` 直接挂掉） |

### 三个必须记住的结论

1. **先确认观察手段可靠，再怀疑代码**。这次一半时间花在测试工具自身的 bug 上：
   `uinput-inject` 的数字表整体偏移一位（`3` 发成 `KEY_4`），`dbus-trace.py` 的
   press 标志也写反了，一度把「工具问题」误判成「引擎选词错位」。
2. **`FocusIn` 是每个键都会来的**，微信尤其如此（见坑 11）。任何在 `FocusIn` 里
   「重置一下更干净」的想法都会让输入法彻底不工作。
3. **状态栏是硬需求**。`Controller1` 缺一个方法，用户看到的就是「输入法坏了」，
   哪怕打字本身是通的。

---

## 十三、第二轮补齐：翻页、英文、简繁、密码框（2026-11-19）

第一版只做到「能出字」，这一轮把 Wayland 前端已有的四件事补上，并顺手查明
了一个环境级陷阱。

### 13.1 候选翻页

协议侧本来就有两条路，都要实现：

- `ProcessKeyEvent` 收 `-` / `=` / `PageUp` / `PageDown`；
- 客户端点候选窗箭头时直接调 `PrevPage` / `NextPage`。

两者共用同一份 `page` 状态（`CtxInner.page`，单位是页）。信号只能从方法里发，
所以 `PrevPage` / `NextPage` / `SelectCandidate` 必须注入
`#[zbus(signal_emitter)]`；空实现的话，客户端点了箭头什么也不会发生。

**页内序号**是这一节的坑：`SelectCandidate(i)` 与数字键 `1`-`9` 给的 `i` 都是
「当前页内第几条」，不是全局序号，取候选要写成 `page * PAGE_SIZE + i`。
写错的话第 2 页按 `1` 会拿到第 1 页的首条，看起来像「翻页没生效」。

`UpdateClientSideUI` 的 `hasPrev` / `hasNext` 要如实反映边界，客户端据此显示
箭头；到边界时按键仍要吞掉（返回 true），否则 `-` 会被应用当成减号打出来。

**另一个发现：引擎原本就只产 9 条候选。** `engine.rs` 的 `BEST`（每个位置保留
几条路径）原本是 9，最终候选表因此最多 9 条，`CAND_MAX = 128` 只是上限摆设。
翻页代码写对了也翻不到第 2 页。把 `BEST` 提到 27、`HITS` 提到 6 之后，`nihao`
有 27 条（3 页），全串 `zhonghuarenmingongheguo` 的单次解码耗时增加约 1ms
（30 次进程内 `--query` 从 147ms 到 186ms，含进程启动），可以接受。

### 13.2 英文模式（前缀补全）

第一版英文态是「一律透传」，等于没有英文输入支持。现在复用 `src/en.rs` 的
`EnEngine`（与 Wayland 前端同一套词表与大小写逻辑）：

| 键 | 行为 |
|---|---|
| 字母 | 进缓冲，实时出补全候选 |
| 空格 | 提交首候选，**返回 false** 让空格照常进应用 |
| 回车 | 提交「用户敲的原始字母」，回车同样透传 |
| `1`-`9` | 选补全词 |
| Esc | 放弃 |

关键在空格：`ProcessKeyEvent` 返回 `true` 时客户端不再处理这个键，所以
「提交 + 吞键」会让 `hel␣` 变成 `hello`（空格丢了）。正确做法是
`commit=Some(词), consumed=false`，让应用自己把空格插进去。

### 13.3 简繁切换 Ctrl+Shift+F

`Global.variant` 是全局的，切换后所有上下文在下次按键时经 `sync_variant()`
同步；当前上下文立刻 `engine.set_variant()` 重算，所以候选窗当场变繁体。
必须在「其它 Ctrl 组合一律透传」之前处理，否则 `Ctrl+Shift+F` 会被当成普通
`Ctrl+F`。

### 13.4 密码框

`SetCapability` 里只要带 `Password`(bit3) 或 `Sensitive`(bit36) 就进透传：
字母、数字、空格全部返回 false，且**立即丢弃残留组字**（否则上一个普通输入框
的拼音会跟着进密码框）。实测：`nihao` 五键全部 false、无 CommitString、
无候选信号；从普通框切到密码框时 preedit 也被清空。

### 13.5 环境陷阱：`Ctrl+Space` 可能根本到不了应用

真实按键测试里 `Ctrl+Space` 切不了中英，但 DBus 层自测是通的。原因不在协议：

```sh
$ hyprctl binds -j | jq '.[] | select(.key=="SPACE")'
{ "modmask": 4, "dispatcher": "__lua", ... }   # 4 = Ctrl，omarchy 的全局绑定
```

按键在合成器层被当成全局快捷键吃掉了，应用和输入法都收不到。判断方法：
`hyprctl` 查绑定 + 直接读 `Controller1.CurrentInputMethod` 有没有变。

这类环境下用 **Shift 单击**（`shift_pending` + 松开判定）切中英，它是唯一
不经过合成器绑定的路径。真 fcitx5 在同一台机器上同样切不动，不是 xime 的问题。

### 13.6 无人值守回归

`tools/dbus-selftest.py` 把上述行为固化成 40 项断言，不需要人敲键、不需要看屏幕：

```sh
python3 tools/dbus-selftest.py       # 40/40 通过
python3 tools/dbus-selftest.py -v    # 打印每步 UI 快照
```

覆盖：翻页内容不重叠、页内序号选词、末页不越界、`PrevPage`/`NextPage` 方法、
英文补全与空格透传、`Ctrl+Letter` 透传、简繁切换、密码框全透传。

真实按键侧另外验证（`uinput-inject` + `gtk3-probe.py` + 微信截图）：

| 场景 | 结果 |
|---|---|
| GTK：`nihao` → `=` → `1` | 缓冲区 `尼好`（第 2 页首条）✅ |
| GTK：Shift 单击 → `hel␣` | 缓冲区 `help `（空格保留）✅ |
| 微信：`nihao` 第 1 页 | 9 条候选，`你好` 打头 ✅ |
| 微信：`=` 第 2 页 | `尼好 你耗 妮好 妳号 泥号 尼号 妮号 呢灏 呢豪` ✅ |
| 微信：第 2 页按 `1` | 输入框得到干净的 `尼好`，候选窗收起 ✅ |
| 微信：Shift 切英文后 `ni` | 客户端自绘英文补全候选 ✅ |

`uinput-inject` 这轮也补了两件事：`combo <mods> <key>`（组合键）与
`tap <key>`（单击命名键，用来测 Shift 切换）。注意 `click dx dy` 是**相对位移**，
不是绝对坐标——想点某个位置先 `hyprctl cursorpos` 读出当前位置再算差值，
否则会像这轮一样把鼠标甩到屏幕角落、点到别的窗口上去。

---

## 参考

- `fcitx/fcitx5-qt` · `qt5/dbusaddons/interfaces/*.xml`、`qt5/dbusaddons/fcitxqtinputcontextproxy_p.h`、`qt5/platforminputcontext/qfcitxplatforminputcontext.cpp`
- `fcitx/fcitx5` · `src/frontend/dbusfrontend/dbusfrontend.cpp`
- `qt/qtwayland` · `src/client/qwaylandtextinputv3.cpp`（对照 Wayland 路径最后一步）
- 本机证据：`/opt/wechat-universal/wechat` 的符号表；`journalctl --user -u xime-test` 的 commit 记录
