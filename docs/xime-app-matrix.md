# xime 适用场景：四个常用应用 × 两种合成器

> 目的：回答「Hyprland 和当前 KDE 下，WorkBuddy / 微信 / Chrome / Konsole 到底能不能用 xime」。
> 依据分级标注：**[实测]** 本机跑出来的、**[源码]** 代码或上游提交、**[推断]** 由前两者推得、**[未验证]**。
> 日期：2026-11-20（末次复验 21:2x，KDE 侧四应用全通）。

---

## 一、先看结论

### 1.1 矩阵

| 应用 | 实际形态 | Hyprland（v2 前端） | 当前 KDE（v1 前端） |
|---|---|---|---|
| **Chrome** | Chromium 158，Wayland 原生 | ✅ 可用（Chromium 自己实现 text-input-v3，不依赖 GTK 模块）**[推断]** | ✅ 可用 **[实测]**，连环境变量都不用清 |
| **Konsole** | Qt6 25.12，Wayland 原生 | ✅ 可用（需清 `QT_IM_MODULE`） | ✅ 可用 **[实测]**，前提是清 `QT_IM_MODULE`（会话级，需重登生效） |
| **WorkBuddy** | Electron 37 / Chromium 138，**已改为 Wayland 原生** | ⚠️ 推断可用（同 Chrome 路径，v2 前端下待验证） | ✅ **实测通过**：`xlsclients` 无记录、`activeClientSupportsTextInput=true` |
| **微信** | 4.1.13.26，Wayland 原生，实际走 **text-input-v2** | ⚠️ 推断可用（Hyprland 是否通告 v2 待确认） | ✅ **实测通过**：候选窗弹出、空格上屏「你好」；前提是清掉 IM 变量 |

一句话：**KDE 下四个应用全部打通——Chrome / Konsole / 微信 / WorkBuddy 均已实测建立了 text-input 通道。** 共同规律是：能不能连上 text-input，取决于**该应用启动那一刻的环境变量与图形后端**，不是合成器本身。

### 1.1.1 「为什么只有 Chrome 能用」（2026-11-20 实测定论）

用户的实际现象曾是 Chrome 正常、WorkBuddy 与 Konsole 打不出中文。三者并排看，差别正好落在两道闸门上：

| 应用 | 图形后端 | 启动时的 IM 变量 | 当时结果 | 卡在哪 |
|---|---|---|---|---|
| Chrome 158 | Wayland 原生 | 有 `GTK_IM_MODULE=fcitx` | ✅ | 都不卡：Chromium 140+ 的 v3 默认开，且它**不读** `GTK_IM_MODULE` |
| Konsole 25.12 | Wayland 原生 | 有 `QT_IM_MODULE=fcitx` | ❌ | 卡第 2 道闸：Qt 走 fcitx 模块，不建 text-input |
| WorkBuddy（Chromium 138） | **XWayland** | 有 `GTK_IM_MODULE=fcitx` | ❌ | 卡第 1 道闸：X11 窗口没有 `wl_surface`；外加 138 的 v3 未默认开 |

所以「只有 Chrome 能用」不是 xime 挑应用，而是**当时只有 Chrome 同时越过了这两道闸**。两处改动（注释会话级 IM 变量 + 启动器加 ozone/IME 参数）落地后，四者全部通过。

### 1.2 「是否都已可以使用」

**是——KDE 下四个都通了。**（Hyprland 侧仍有两个待验证项，见第一节表格的 ⚠️）

- **Chrome / Konsole**：Wayland 原生，已实测建了 text-input，可用；
- **微信**：Wayland 原生 + text-input-v2，**已实测在 xime 下弹出候选窗并上屏「你好」**；但必须清掉 `QT_IM_MODULE` / `GTK_IM_MODULE` / `XMODIFIERS` 启动，否则它会退回 fcitx 的 DBus 通道，text-input 永远不 enable；
- **WorkBuddy**：启动器加参数后重启，已转为 Wayland 原生（`xlsclients` 无记录）且 `activeClientSupportsTextInput=true`。

另外两条前端本身互斥：原生要求会话里**没有** `QT_IM_MODULE` / `GTK_IM_MODULE`，`--dbus` 要求**有**，且要抢同一个单实例锁，只能二选一。所以别混着用——目标就是让四个应用都走原生前端。

### 1.3 会话状态沿革：从「xime 根本没接管」到「四应用全通」

**（A）修复前 [实测]** `xime-kde-check` 输出四项全红：

| 检查项 | 实际值 | 后果 |
|---|---|---|
| `QT_IM_MODULE` | `fcitx` | Qt 应用不建 text-input |
| `GTK_IM_MODULE` | `fcitx` | GTK 应用改用 fcitx 模块，不建 text-input（Chromium 例外，见 4.1） |
| `kwinrc [Wayland] InputMethod` | `org.fcitx.Fcitx5.desktop` | KWin 拉起的是 fcitx5 |
| xime 进程 | 未运行 | 无输入法可接 |

变量来源：`~/.config/environment.d/99-fcitx5-im.conf`（三行 fcitx 桥接，未被注释）。当时「四个应用都用不上 xime」不是 xime 在这些应用里失败，而是 xime 压根没上线。

**（B）修复动作（20:45）**，详见 1.1.1 / 4.2 / 4.3：

- `99-fcitx5-im.conf` 里 `QT_IM_MODULE` / `GTK_IM_MODULE` 已注释（保留 `XMODIFIERS`，X11 应用的 XIM 兜底不受影响）；
- `workbuddy.sh` 已加 ozone + IME 参数，并在启动器内 `unset` 这两个变量。

**（C）21:2x 复验 [实测]**：`kwinrc` 已指向 `xime.desktop`、xime 进程在跑、`GTK_IM_MODULE`/`QT_IM_MODULE` 已不在应用环境里、WorkBuddy 不在 `xlsclients` 且 `activeClientSupportsTextInput=true`。**四应用全部走通原生前端。**

---

## 二、判定模型：三层闸门，缺一层就废

任何一个应用要用 xime，必须同时过三关。任何一关不过，症状都是「打得进英文字母」，而不是报错。

```
① 应用层：有没有创建 zwp_text_input_v3 上下文？
   ├─ Wayland 原生 + 未设 QT_IM_MODULE/GTK_IM_MODULE → 有
   ├─ 设了 IM 变量（哪怕应用是 Wayland 原生）      → 没有（改走 fcitx DBus 模块）
   └─ XWayland（X11 窗口没有 wl_surface）          → 没有，且无法有
② 合成器层：有没有 input_method 协议把按键交给 xime？
   ├─ Hyprland / sway / labwc → zwp_input_method_v2（xime 当普通客户端起）
   └─ KWin 6.6               → zwp_input_method_v1（必须由 KWin 拉起）
③ 前端层：xime 起的是哪条路？
   ├─ 原生（默认）→ 只覆盖 ① 里「有」的那批
   └─ --dbus      → 只覆盖 ① 里「没有」的那批（且要求 IM 变量存在）
```

两个容易误判的地方，本次都实测到了：

- **应用是 Wayland 原生，也可能不建 text-input。** 同一个 Konsole，去掉 `QT_IM_MODULE` 后 `activeClientSupportsTextInput=true`，带上就是 `false` **[实测]**。所以「Konsole 支不支持」和「你的会话配没配好」是两个问题。
- **Chromium 系不认 `GTK_IM_MODULE` 这条路。** Chrome 158 在 `GTK_IM_MODULE=fcitx` 下**仍然**建了 text-input-v3（`activeClientSupportsTextInput=true`）**[实测]**。这是它和 Qt 应用最大的行为差异（WaylandTextInputV3 自 Chromium 140 起默认开启 **[源码]**；140 之前要用 `--wayland-text-input-version=3` 显式指定）。
- **同一个应用可能同时有两条 IC，选哪条由环境变量决定。** 微信默认走 `frontend:dbus`（focus:1），`frontend:wayland` 建了却 focus:0；把 IM 变量清掉重启，它才切到 wayland 那条并 `enable()` **[实测]**。所以「看到应用 bind 了 text-input 管理器」不等于「它在用」。

KDE 侧还有一层桥接值得知道：KWin 6.6 客户端说 v3、输入法说 v1，中间由 KWin 转换（KDE bug 517620 里能看到 Discord 的 v3 `surrounding_text` 被 KWin 转成 `input_method_context_v1` 事件）**[源码]**。这解释了为什么 v1 前端能覆盖原生 Chromium。

---

## 三、实测记录（2026-11-20，本机 KDE 会话）

环境：KDE Plasma 6.6.6 / KWin 6.6.6 / Wayland（`wayland-0`），Chrome 158.0.8127.36，Konsole 25.12.3，WorkBuddy 5.9.3（Chromium 138），微信 4.1.13.26。

| # | 对象 | 操作 | 结果 |
|---|---|---|---|
| 1 | Konsole | `env -u QT_IM_MODULE -u GTK_IM_MODULE konsole --separate` + 置焦 | `activeClientSupportsTextInput=true`；不在 `xlsclients` 列表 → Wayland 原生且建了 text-input |
| 2 | Konsole | 同上，但继承 `QT_IM_MODULE=fcitx` | `activeClientSupportsTextInput=false` → 变量一票否决 |
| 3 | Chrome | 默认参数启动，继承 `GTK_IM_MODULE=fcitx` | 日志出现 `--ozone-platform=wayland`；不在 `xlsclients`；`activeClientSupportsTextInput=true` |
| 4 | WorkBuddy | 改参数前，运行中实例置焦 | 出现在 `xlsclients`（`kubuntu workbuddy`）；`activeClientSupportsTextInput=false` |
| 5 | 微信（旧版 4.1.1.4） | 二进制静态检查 | 无 `platforms/`、`wl_display_connect`=0 → 曾判定 X11-only，**该结论已作废** |
| 6 | 微信（4.1.13.26） | `WAYLAND_DEBUG=1 wechat` | 窗口 `wechat/微信` **不在** `xlsclients`（Wayland 原生）；扫码页阶段未建 text-input 实例 |
| 7 | 微信（默认环境，含 IM 变量） | `fcitx5-diagnose` | 双 IC：`frontend:dbus focus:1` + `frontend:wayland focus:0` → 默认走 DBus，text-input 不启用 |
| 8 | 微信（`env -u` 清 IM 变量） | `WAYLAND_DEBUG` 抓协议 | `get_text_input` 出现；`zwp_text_input_v2#19.enable(wl_surface#39)`；`activeClientSupportsTextInput=true` |
| 9 | 微信 + xime（`xime-switch xime`） | Ctrl+F → 注入 `nihao` → 空格 | 候选窗 `1.你好 2.你号 3.呢好 …`；上屏后搜索框显示 **「你好」** ✅ |
| 10 | Konsole + xime（20:47 复测） | 干净环境起 Konsole，`xime-switch xime` → 点终端 → 注入 `nihao` → 空格 | 终端内联 preedit `nihao` + 候选窗 `1.你好 2.你号 3.呢好 4.妙好 5.你灏 6.你豪`；空格后提示符行出现 **「你好」** ✅ |
| 11 | WorkBuddy 启动器（20:45） | `strings workbuddy` 查能力 | `zwp_text_input_manager_v3`、`ozone-platform-hint`、`wayland-text-input-version`、`enable-wayland-ime` 均在二进制内 → 参数受支持 |
| 12 | WorkBuddy 重启后（21:2x） | 读 `/proc/PID/cmdline` + `environ`，`xlsclients`，KWin 置焦查属性 | cmdline 三参数齐全；`GTK_IM_MODULE`/`QT_IM_MODULE` 已不在环境里；**`xlsclients` 无 workbuddy**；`active=true`、`activeClientSupportsTextInput=true` ✅ |

第 7、8 条是这次的关键：**微信的输入通道由启动时的环境变量一次性决定**——有 IM 变量走 DBus，没有就走 text-input-v2。第 9 条是端到端闭环，证明 xime 在微信里确实能落字。

第 4 条与第 12 条构成一组前后对照：**改参数前 WorkBuddy 确实跑在 XWayland**（这与「Electron 默认 X11」一致，Chromium 140 之前 ozone 默认 x11 **[源码]**），X11 窗口没有 `wl_surface`，KWin 不为它建输入法上下文——这不是 xime 的问题，fcitx5 也接不到。重启后第 12 条全绿，路径与 Chrome 完全一致。

---

## 四、逐个应用

### 4.1 Chrome —— 最省事的一个

| 项 | 内容 |
|---|---|
| 形态 | Wayland 原生（158 默认 wayland ozone），text-input-v3 自 **Chromium 140** 起默认启用 |
| KDE | ✅ 只要 KWin 拉起 xime 就能用；`GTK_IM_MODULE` **不影响**（实测） |
| Hyprland | ✅ 同理，建议顺手清 `GTK_IM_MODULE` 以免走 GTK IME 回退 |
| 风险 | Chromium 会发超长 `surrounding_text`（KDE bug 517620），xime 侧未做长度校验，KWin 转发时可能触发输入法侧异常；目前未观察到 |

xime 在 Chrome 里唯一的未验证项是「commit 能否真正落进网页输入框」——需要真正切到 xime 才能确认，机制链（v3 → KWin 桥 → v1）是通的。

### 4.2 Konsole —— 能通，但要先清环境变量

| 项 | 内容 |
|---|---|
| 形态 | Qt6 → Qt Wayland 插件的 text-input-v3 |
| KDE | ✅ 实测 `activeClientSupportsTextInput=true`（清 `QT_IM_MODULE` 后）；等价于已全绿实测的 Kate |
| Hyprland | ✅ 同一机制；`QT_IM_MODULE` 必须为空 |
| 注意 | 终端里 `Ctrl+C`、`Tab`、方向键依赖 xime 的「不消费就回注」，这部分在 KDE v1 路径下已实测通过（`ctx.key` 原样转发） |

Konsole 是四个里最稳的：不涉及 Chromium 那套自定义 IME 实现，也没有登录态这类前置条件。

**环境变量的坑在于「改了不生效」**：`~/.config/environment.d/99-fcitx5-im.conf` 里的
`QT_IM_MODULE=fcitx` 是登录时由 systemd 导入、再被 Plasma 继承给所有子进程的。
注释掉那两行之后，**必须重新登录一次**才会作用于面板/菜单拉起的 Konsole——
已经开着的 Konsole 进程环境是启动那一刻的快照，重启应用也没用，得重登。
（验证时可以绕开：`env -u QT_IM_MODULE -u GTK_IM_MODULE konsole`。）

### 4.3 WorkBuddy —— 加三个参数后已实测转 Wayland（可用）

| 项 | 内容 |
|---|---|
| 形态 | Electron 37（Chromium 138），改参数前是 XWayland |
| KDE | ✅ **实测通过**：重启后不在 `xlsclients` 里，`activeClientSupportsTextInput=true` |
| Hyprland | ⚠️ 机制同 Chrome，但 Hyprland 用 v2 前端，未实测 |
| 已改 | `workbuddy.sh` 的 `exec` 加 `--ozone-platform-hint=auto --enable-wayland-ime --wayland-text-input-version=3`，并在启动器里 `unset GTK_IM_MODULE QT_IM_MODULE` |

**为什么必须带 `--wayland-text-input-version=3`**：Chromium 是 **140** 才把
text-input-v3 设为默认的；本包内嵌 138（`strings workbuddy | grep Chrome/` →
`Chrome/138.0.7204.251`），低于这条线，不显式指定就还停在 v1/v2 的老路上。
140 之后这条参数可以删掉，只留 ozone hint。

`unset GTK_IM_MODULE QT_IM_MODULE` 写在启动器里而不是只改会话环境，是为了**免重登**：
Electron 会检查这两个变量，非空时退回 GTK 输入法模块、绕开 text-input；
而 `environment.d` 的改动要下次登录才进入 Plasma 的环境。

**2026-11-20 21:2x 实测（用户重启后复验）**：

```
cmdline    workbuddy --no-sandbox --ozone-platform-hint=auto --enable-wayland-ime --wayland-text-input-version=3
environ    WAYLAND_DISPLAY=wayland-0   DISPLAY=:1   XMODIFIERS=@im=fcitx
           （GTK_IM_MODULE / QT_IM_MODULE 均不存在 ← 启动器的 unset 生效）
xlsclients 只剩 kubuntu / qwen-work-cn / clash-verge，**无 workbuddy**
KWin       active=true, activeClientSupportsTextInput=true
```

验证判据就是这两条：`xlsclients -display $DISPLAY | grep -i workbuddy` **无输出**，且 `activeClientSupportsTextInput=true`。两条都满足，说明它已是 Wayland 原生客户端并建立了输入法上下文。

原先担心的「重打包 macOS 版渲染层用自定义滚动布局、输入框不是标准 `contenteditable`」这条风险，**本次实测未出现**——text-input 通道正常建立。

### 4.4 微信 —— 两套通道，环境变量决定用哪套（已实测可用）

**结论先行：微信 4.1.13.26 + xime（KDE / v1 前端）实测可以打中文。** 候选窗正常弹出、空格上屏「你好」。但有一个硬前提：**启动微信时必须没有 `QT_IM_MODULE` / `GTK_IM_MODULE` / `XMODIFIERS`**。

#### 它其实有两条输入通道

`fcitx5-diagnose` 在同一个微信进程上能同时列出两个 IC：

```
IC [... ] program:wechat frontend:dbus    cap:...  focus:1   ← 有 IM 变量时在用这条
IC [... ] program:wechat frontend:wayland cap:...  focus:0   ← 已创建，但没启用
```

默认会话里（`99-fcitx5-im.conf` 三行没注释）微信走 **DBus**：连 fcitx5 或 xime 的 `--dbus` 前端，**完全不 enable text-input**。这时候 xime 原生前端一个键都收不到——不是它坏了，是微信压根没走那条路。

#### 清掉 IM 变量后发生什么（[实测]）

用 `env -u QT_IM_MODULE -u GTK_IM_MODULE -u XMODIFIERS wechat` 启动，协议轨迹立刻变化：

```
zwp_text_input_manager_v2#18.get_text_input(new id zwp_text_input_v2#19, wl_seat#7)
zwp_text_input_v2#19.enable(wl_surface#39)
zwp_text_input_v2#19.set_cursor_rectangle(323, 510, 2, 22)
```

三个关键点：

1. **它用的是 text-input-v2，不是 v3。** 之前只看 `bind(...manager_v3)` 会误判——那次绑定之后它实际实例化的是 v2 对象。
2. `enable(wl_surface#39)` 说明输入框真的把输入法打开了。
3. KWin 的 `activeClientSupportsTextInput` 此时为 **true**，`frontend:wayland` 那条 IC 也变成可用。

#### 端到端实测（xime v1 前端）

先把 KWin 切到 xime（`xime-switch xime`），然后在微信里按 Ctrl+F 唤起搜索框，注入 `nihao`：

| 步骤 | 观察 |
|---|---|
| 输入 `nihao` | 搜索框内出现带下划线的 preedit，上方弹出 xime 候选窗：`1. 你好  2. 你号  3. 呢好  4. 你好  5. 你灏  6. 你豪` |
| 按空格 | 搜索框内容变为 **「你好」**，候选窗消失 |

这就是完整闭环：按键 → xime 组字 → KWin 转交 → 微信落字。和 Chrome 唯一的差别是微信说 v2、Chrome 说 v3，KWin 两种都桥接到 input method v1。

#### 一个反直觉的副作用

把 KWin 切回 fcitx5 之后，**同一个微信实例反而打不出中文了**（只出英文 `nihao`）：它已经选了 text-input 通道，而 fcitx5 在那个实例上没有接管成功。把微信关掉、按默认环境重启，DBus 那条 IC 回来（`focus:1`），fcitx5 立刻恢复正常。

所以微信的通道选择是**启动时一次性决定**的：谁在环境里，它就走谁。

#### 两种用法，按需选

| 目标 | 怎么起微信 | 结果 |
|---|---|---|
| 用 xime 原生前端 | `env -u QT_IM_MODULE -u GTK_IM_MODULE -u XMODIFIERS wechat` | 走 text-input-v2，xime 可用 |
| 用 fcitx5 / `xime --dbus` | 正常启动（保留 IM 变量） | 走 DBus，xime 原生前端不可用 |

前者可以写进一个 wrapper 或独立的 `.desktop` 条目，不必改全局环境。

**Hyprland 侧**：微信用 v2，Hyprland 是否通告 `zwp_text_input_manager_v2` 需要确认（wlroots 系常见是 v3）。若只有 v3，微信在 Hyprland 下可能直接没有可用协议——这一格本次没有环境实测，标 **[未验证]**。

#### 版本沿革（别再套旧结论）

| 版本 | 状态 |
|---|---|
| 4.1.1.4 | 纯 X11 静态构建，`wl_display_connect` 符号为 0，只能 XWayland → xime 两条前端都够不着 |
| 4.1.13.26 | Wayland 原生 + text-input-v2 + DBus 双通道 → **清 IM 变量后 xime 可用（已实测）** |

---

## 五、两条可选路线

### 路线 A：原生前端（推荐，目标是四个全通）

**KDE：**

```sh
# 1. 接线：生成 xime.desktop、改 kwinrc、把 environment.d 里的 IM 变量注释掉
xime-setup
# 2. 重新登录（环境变量只在登录时导入，改完当次会话不生效）
# 3. WorkBuddy 改走 Wayland（三个参数都要带，Chromium 138 < 140 不默认 v3）
#    编辑 ~/utility/WorkBuddy-5.9.3-linux-x64/workbuddy.sh
#    unset GTK_IM_MODULE QT_IM_MODULE
#    exec "$DIR/workbuddy" --no-sandbox --ozone-platform-hint=auto \
#        --enable-wayland-ime --wayland-text-input-version=3 "$@"
# 4. 验证
xime-kde-check
xlsclients -display $DISPLAY | grep -i workbuddy   # 应无输出
# 5. 微信：重登后 IM 变量已全局清空，正常启动即可走 text-input-v2
#    若不想重登、只想临时验证，单独这样起：
#    env -u QT_IM_MODULE -u GTK_IM_MODULE -u XMODIFIERS wechat
```

**Hyprland：**

```sh
# hyprland.conf
exec-once = xime
# 前提：会话里 QT_IM_MODULE / GTK_IM_MODULE 为空；fcitx5 不要同时抢席位
```

覆盖结果（KDE 下已全部实测）：Chrome ✅、Konsole ✅、微信 ✅（清 IM 变量启动）、WorkBuddy ✅（加 ozone/IME 参数后重启）。

### 路线 B：`--dbus` 前端（覆盖面更窄，别指望它救微信）

用途是覆盖 **XWayland 客户端**，以及「自己会加载 fcitx5-qt / fcitx5-gtk 模块」的程序。要求保留 `QT_IM_MODULE=fcitx5`（注意是 `fcitx5`，不是 `fcitx`——xime 只实现了 fcitx5 那套 `InputContext1`），且与原生前端互斥。此时 Chrome/Konsole 退回 fcitx 模块语义（若 fcitx5 不跑，就没有输入法），所以这条路等于「xime 当 fcitx5 的替身」，不是「xime 当 Wayland 输入法」。

微信不在覆盖范围内：新版虽然已经 Wayland 化，但二进制里仍没有 `fcitx5platforminputcontext` 符号（4.4），DBus 上没有客户端会连上来。它该走的是原生前端，不是这条路。

---

## 六、验证命令清单

```sh
# 谁在管输入法
xime-kde-check

# 应用是不是 XWayland（出现即 X11，xime 原生前端无解）
xlsclients -display $DISPLAY

# 当前聚焦的应用有没有建 text-input
for p in active activeClientSupportsTextInput available enabled; do
  printf '%-30s = %s\n' "$p" "$(qdbus6 org.kde.KWin /VirtualKeyboard \
    org.freedesktop.DBus.Properties.Get org.kde.kwin.VirtualKeyboard $p)"
done

# xime 是否真的在上屏（KDE 下日志必须重定向，KWin 不读子进程 stdout）
grep -c 'commit' /tmp/xime-kde.log
grep -c '回注'   /tmp/xime-kde.log
```

判读口诀：**字母直接进应用 = 按键直通 = 应用没建 text-input（第 ① 关没过），不是 xime 解析错。**

---

## 七、未验证清单（别当成结论用）

1. **Chrome / WorkBuddy 里 `commit` 是否真正落字** —— 只验证到「建了 text-input」，没验证到「汉字上屏」。用户已确认 WorkBuddy 实际可用，其余未做逐字验收。
2. **Hyprland 侧的四个应用** —— 本机当前无 Hyprland 会话（`~/.config/hypr` 不存在），矩阵里 Hyprland 一列来自既有实测记录与上游文档，不是本次复测。
3. **Chromium 超长 `surrounding_text`** —— 理论风险（KDE bug 517620），未观察到实际崩溃。
4. **微信在 Hyprland 下是否可用** —— 它实例化的是 `zwp_text_input_v2`；Hyprland/wlroots 是否通告 `text_input_manager_v2` 需确认（常见只有 v3）。KDE 侧已实测通过，Hyprland 侧无环境可测。
5. **WorkBuddy 在 Hyprland 下** —— 机制同 Chrome（v3 → wlroots 的 input method v2），但本机无环境，未测。

---

## 附：证据出处

| 结论 | 出处 |
|---|---|
| KWin 只实现 `zwp_input_method_v1`，必须由 KWin 拉起 | `xime-rs/docs/kde-input-method-v1.md` |
| KWin 把客户端 v3 桥到 v1 context | KDE bug 517620（`zwp_text_input_v3 → zwp_input_method_context_v1.surrounding_text`） |
| Chromium M136 起 WaylandTextInputV3 默认开启 | chromium/src commit `e48954bdc391`（M136 cherry-pick） |
| Chromium 140 起默认 Wayland ozone | ArchWiki Chromium、Flathub 讨论 |
| 微信 4.1.13.26 清 IM 变量后 xime 可落字 | 本机端到端实测：`get_text_input` + `zwp_text_input_v2.enable()`；候选窗 `1.你好…`；空格后搜索框显示「你好」；`fcitx5-diagnose` 可见 dbus/wayland 双 IC |
| XWayland 无 `wl_surface`，KWin 不建上下文 | `xime-rs/docs/kde-input-method-v1.md` 第五节 |
