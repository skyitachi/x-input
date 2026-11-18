# xime — 单包零配置的 Wayland 原生输入法

一个直接讲 `zwp_input_method_v2` + `zwp_text_input_v3` 协议的中文输入法。不依赖 fcitx/ibus 框架，
不注入 `GTK_IM_MODULE`/`QT_IM_MODULE`，不需要打开系统设置，装完一个 deb 就能打字。

* 二进制 585KB，deb ≈ 210KB（字形在构建期烘焙进二进制，运行时零字体依赖）
* 运行期依赖只有 Wayland 会话本就必备的 `libc` / `libwayland-client` / `libxkbcommon`
* 不附带字体、不附带外部词库、不需要 DBus、不需要 xdg-desktop-portal

---

## 一、先看能力边界

输入法在 Wayland 上没有"全局钩子"可用，能不能工作**完全取决于合成器是否实现了 `zwp_input_method_v2`**。
这一点无法用代码绕过，先把边界摊开：

### 合成器侧

| 合成器 | 支持 | 说明 |
|---|---|---|
| KWin（Plasma 5.26+ / 6.x） | ✅ | 但**必须由 KWin 拉起**：它给输入法单独开一个 Wayland display，输入法全局对象只对这个子连接可见。`xime-setup` 会把 `[Wayland] InputMethod=/usr/bin/xime` 写进 `/etc/xdg/kwinrc`（系统默认层，不动用户私有配置），下次登录生效 |
| sway / Hyprland / labwc / river（wlroots） | ✅ | 全局对象对普通客户端可见，`exec xime` 即可，无需任何特殊启动方式 |
| Mutter（GNOME） | ❌ | Mutter 不实现 `zwp_input_method_v2`。GNOME 走的是 ibus + GTK/Qt 输入法模块这条完全不同的路。**本方案在 GNOME 上无法工作**，`xime --check` 会明确报出来 |

### 应用侧

| 应用 | 支持 | 依据 |
|---|---|---|
| GTK4 / GTK3（Wayland 后端） | ✅ | GTK 原生实现 `zwp_text_input_v3` |
| Qt5 / Qt6（wayland 平台插件） | ✅ | qtwayland 实现 `zwp_text_input_v3` |
| Chromium / Electron（Ozone Wayland） | ✅ | 旧版需要 `--enable-wayland-ime` |
| XWayland 里的 X11 程序 | ❌ | X11 走 XIM，本方案不提供 XIM 前端 |
| 部分终端（alacritty/kitty 等） | ❌ | 它们自己处理按键，不接 `text_input_v3` |

> X11 应用和终端这两条"不支持"，正是 fcitx5 必须同时提供 wayland 前端 + GTK 模块 + Qt 模块 + XIM 四个前端的原因。
> 单包零依赖的代价，就是只覆盖协议栈内那部分应用。

---

## 二、为什么"单 deb + 零配置"在 Wayland 上是可行的

| 路线 | 机制 | 运行期依赖 | 兼容面 |
|---|---|---|---|
| fcitx5 / ibus | 输入法框架 + 守护进程 + `GTK_IM_MODULE`/`QT_IM_MODULE`/XIM 三套模块 + DBus | 重（框架、模块、配置） | 最广 |
| 只做 toolkit 模块 | 注入环境变量，让 GTK/Qt 加载 .so | 中（需改环境/设置） | 仅该 toolkit |
| **Wayland 协议原生（本方案）** | 输入法直接是协议的一端，应用是另一端，合成器负责转接 | 只要 Wayland 会话自带的基础库 | KWin + wlroots，覆盖 GTK/Qt/Electron |

协议本身已经把"应用"和"输入法"解耦了：合成器把光标/文本输入状态转给输入法，输入法把
preedit/commit 结果转回应用。**不需要往任何进程里注入东西**，所以才能做到单包、零配置、零环境变量。

---

## 三、架构

```
┌──────────────┐   zwp_text_input_v3    ┌──────────────┐   zwp_input_method_v2   ┌──────────────┐
│ 应用 GTK/Qt  │ <───────────────────> │ 合成器 KWin/ │ <─────────────────────> │    xime      │
│ (wl_surface) │                       │   wlroots    │                         │              │
└──────────────┘                       └──────────────┘                         └──────┬───────┘
                                              ▲                                       │
                                              │ zwp_virtual_keyboard_v1（回注非消费按键）│
                                              └───────────────────────────────────────┘
```

### 源码职责

| 文件 | 职责 |
|---|---|
| `src/xime.c` | 主程序：Wayland 生命周期、registry 绑定、按键路由决策、虚拟键盘回注、CLI |
| `src/engine.c` | 解码引擎：N-best 路径搜索（整词 / 简拼 / 尾部音节补全）、`u4f60` 直输码位 |
| `src/dict.c`, `src/dict_gen.h` | 内置词库：14.4 万词条 + 3.2 万简拼索引，二分查找；支持外部词表合并 |
| `tools/mkdict.py` | **构建期**词库生成：读公开拼音/词频数据 → 生成 `src/dict_gen.h` |
| `src/popup.c` | 候选窗：`zwp_input_popup_surface_v2` + `wl_shm`，自绘圆角底/描边/选中高亮 |
| `src/render.c` | 画布与文字绘制（8bpp alpha 图集叠加），BMP 导出用于离线校验 |
| `src/utf8.c` | UTF-8 编解码 |
| `tools/mkfont.c` | **构建期**字形烘焙：读系统 CJK 字体 → 光栅化成图集 C 数组 |
| `test/test_engine.c` | 引擎单元测试（41 项，含多音字/简拼/补全/词库优先级） |
| `test/mock_compositor.c` | 假合成器：协议级集成测试，不需要真实桌面 |
| `proto/vendor/` | vendored 的 `input-method-unstable-v2.xml`、`wlr-virtual-keyboard-unstable-v1.xml` |
| `packaging/` | control、维护者脚本、`xime-setup` 自动接线、deb 组装脚本 |

---

## 四、协议时序（这一段是全部难点所在）

```
合成器                                    xime
  │  activate ──────────────────────────> │  文本输入获得焦点
  │                                       │  get_input_popup_surface(surface)  → 候选窗角色
  │ <──────────────────── grab_keyboard ─ │  必须抢键盘，否则一个按键都收不到
  │  keymap(format=XKB_V1, fd, size) ───> │  xkb_keymap_new_from_string
  │                                       │  同一份 keymap 转交 virtual_keyboard
  │  repeat_info(rate, delay) ──────────> │
  │  key(serial, time, keycode, state) ─> │  ┌─ 消费：set_preedit_string / commit_string
  │  modifiers(...) ────────────────────> │  └─ 不消费：zwp_virtual_keyboard_v1.key 原样回注
  │ <──────────────── commit(serial) ──── │  一次状态修改的原子提交
```

三个必须踩准的点：

1. **必须 `grab_keyboard`**。输入法拿不到键盘设备，收不到任何按键。
2. **grab 之后所有键都归输入法**，所以"不消费的键"必须用 `zwp_virtual_keyboard_v1` 手动回注，
   否则 `Ctrl+C`、方向键、`Tab` 全部失效——这是最容易漏的一步。
3. **`commit(serial)` 的 serial 必须等于已收到的 `done` 事件数**（协议原文如此）。代码里用
   `done_serial` 计数，每次 commit 都用它。传 0 或不传在严格实现的合成器上会被丢弃状态。

候选窗还有一个隐含约束：**不能用 `xdg_popup`**——输入法拿不到光标在屏幕上的坐标，无法算锚点。
必须用协议自带的 `zwp_input_popup_surface_v2`，由合成器负责贴到光标附近。

---

## 五、关键设计决策

**1. 按键消费/回注按 keycode 记账**
按下时决定这个键是"我消费"还是"回注给应用"，记在 `consumed[keycode]` 里；抬起时查同一张表。
这样不会出现"按下被吃掉、抬起却送给应用"导致的按键卡死。

**2. 用 N-best 路径搜索，不做贪心切分**
拼音串没有分隔符，`xian` 可能是「先」也可能是「西安」，而 `nihaom` 能被完整解析成
「你/哈/欧盟」，也应当能解析成「你好/吗」——只保留首选路径的话，后者永远看不到。
所以先按词频把每种切分算成代价，在每个位置保留 6 条最优路径，最后统一排序。
代价模型与两套词频的取舍见下一节。

**3. 字形构建期烘焙，而不是运行时排版**
运行时排版要么带 19MB 的 Noto CJK，要么依赖 freetype/fontconfig（都是新依赖）。
`tools/mkfont` 在构建期把"词库里出现的全部汉字 + ASCII"光栅化成 8bpp 图集（6406 个字形，
约 2.2MB），运行时直接做 alpha 叠加。代价是换词库需要重新构建（`make font`）。

**4. 无字体降级路径**
`--inline-candidates` 把候选直接拼进 preedit 字符串，由应用自己的字体渲染。
零图集也能用，只是排版不如自绘候选窗。

**5. 诊断先行**
`xime --check` 逐项打印合成器是否提供了所需全局对象，缺哪个、该怎么办，直接给结论——因为
"没反应"是输入法调试里最贵的状态。

---

## 六、拼音引擎：词库与解码

### 6.1 词库从哪来

全部来自可再分发的 MIT 数据，构建期由 `tools/mkdict.py` 编译进二进制：

| 数据 | 用途 | 许可证 |
|---|---|---|
| `mozillazg/pinyin-data` | 单字全部读音（含多音字） | MIT |
| `mozillazg/phrase-pinyin-data` | 词组逐字读音（词内多音字才读得对） | MIT |
| `fxsjy/jieba` 词频 | 书面语词频 | MIT |
| `hermitdave/FrequencyWords`（zh_cn，来自 OpenSubtitles） | 口语词频 | MIT |

产出：**144,745 条词条**（单字 4459 / 双字 95891 / 三字 33459 / 四字以上 10936）、
**32,000 条简拼索引**、**6406 个字形**。

### 6.2 为什么必须混合两套词频

只用 jieba 时，「你好」在 6000 万词语料里只有 **725** 次，而「号码」有 8000 次——
于是 `nihaoma` 会被解成「你/号码」。加上口语语料后「你好」有 5 万次，结果才变成「你好吗」。
两套词频各自归一化成概率后按 0.45 / 0.55 加权（输入法偏口语）：

```
P(w) = 0.45 · count_jieba(w)/Σjieba + 0.55 · count_sub(w)/Σsub
```

语料里都没有的词（新词、专业词）用「最冷那个字的字频 × 0.02」做先验，
否则「中国人」这种没进语料的词会输给「中国/任」这种偶然切分。

### 6.3 代价函数

```
cost(词) = log10(1e9) - log10(freq)     # freq 是概率 × 1e9，故常数项取 9
```

常数项实际取 **10**：纯 unigram 模型没有上下文约束，稍微加大常数项可以让
「整词」稳定压过「高频单字拼接」——`nihao` 才不会给出「你/号」。三类边：

| 边 | 适用 | 额外代价 |
|---|---|---|
| 整词 | 剩余输入恰好等于某词拼音 | 0 |
| 简拼 | 剩余输入恰好等于某词的声母串（`nh` → 你好） | +2.0 |
| 补全 | 剩余输入是某词拼音的前缀（`nih` → 你好） | +2.0 + 0.7×(缺字数-1) |

补全按「缺一个音节」计固定代价，而不是按缺几个字母线性计——否则
`wom` 会因为「米」只缺 1 个字母而压过「我们」（缺 2 个字母）。

### 6.4 性能

| 场景 | 耗时 |
|---|---|
| 启动载入词库 | 0.01 ms（词库是静态数组，不做拷贝/排序） |
| `nihao`（5 键） | 0.17 ms（含逐键重算） |
| 37 字长句 | 0.25 ms/键 |

### 6.5 重建词库

```sh
make dict     # 下载/复用 .cache/dict 数据源 → 生成 src/dict_gen.h
make font     # 词库变了，字形集要跟着重烘
```

## 七、构建与安装

```sh
# 构建期依赖（Debian/Ubuntu）：
#   build-essential pkg-config wayland-protocols libwayland-dev libxkbcommon-dev
# 可选：libwayland-dev 里的 wayland-server（跑协议集成测试用）
./build.sh

sudo apt install ./dist/xime_0.2.0_amd64.deb
```

安装后：

* **KDE**：`/etc/xdg/kwinrc` 里已写好 `[Wayland] InputMethod=/usr/bin/xime`，**重新登录**即生效
* **sway / Hyprland / labwc**：`postinst` 已把启动项追加进对应配置文件，重载配置即生效
* 任何时候想手工确认：`xime-setup --status`

构建机不需要 `dpkg-deb`：`packaging/build-deb.sh` 在没有 dpkg-deb 时会用 `ar`+`tar`
手工组装出结构完全一致的包（本项目的 deb 就是在 Arch 上用这条路径产出的）。

---

## 八、验证

三层验证，都不需要真实桌面环境：

```sh
make test          # 引擎单测：41 passed, 0 failed
make test-proto    # 协议级集成测试：起假合成器，跑完整激活→取键→提交链路
build/xime --render-test /tmp/popup.bmp nihao   # 把候选条渲染成 BMP 肉眼检查
build/xime --query jintiantianqihenhao          # 打印某串的候选与解码代价（调引擎用）
```

`test/mock_compositor.c` 用 `libwayland-server` 实现了 `wl_compositor`/`wl_shm`/`wl_seat`/
`input-method-v2`/`virtual-keyboard`，按真实合成器时序驱动被测进程。实际输出：

```
-> activate + done
  [im] get_input_popup_surface
  [im] grab_keyboard
-> 键入 n i h a o space
  [im] set_preedit_string("n") ... set_preedit_string("nihao")
  [im] commit_string("你好")
-> 发送 Esc（应被回注给应用）
  [vk] key(code=1, state=1)
=== 断言 ===
ok   候选窗 surface 已创建
ok   键盘 grab 已建立
ok   preedit 持续更新（7 次）
ok   空格选中首选并提交 "你好"
ok   commit() 调用了 7 次，最后一次 serial=1
ok   虚拟键盘已同步 keymap
ok   非消费按键（Esc）已通过虚拟键盘回注
全部通过
```

候选窗实际渲染效果（`docs/candidate-bar.bmp`，由 `--render-test` 直接产出）：

![候选窗](docs/candidate-bar.bmp)

---

## 九、已知限制

| 限制 | 原因 | 能否解决 |
|---|---|---|
| GNOME 不可用 | Mutter 不实现 `zwp_input_method_v2` | 只能改走 GTK/Qt 输入法模块路线，那样就不再是"单包零依赖" |
| XWayland / 部分终端不支持 | 不在协议栈覆盖范围 | 需要额外做 XIM 前端 |
| 没有上下文语言模型 | 纯 unigram 代价 + 常数项，不做 bigram/神经网络重排 | 需要打包模型文件与推理代码，已超出"单包零依赖"的边界 |
| 简拼只支持 2~4 字词 | 简拼索引按词长筛选，控制体积 | 放宽 `MAX_INI_PER_KEY` / 词长即可，代价是索引变大 |
| 不记忆用户选择 | 无用户词频学习 | 可加"选中后提权并落盘"，需要处理多进程写冲突 |
| 词库需自造时重新构建 | 词条编译进二进制，字形按词库裁剪 | 外部词表可热加载，但新字要能显示仍需 `make font` |
| KDE 需重新登录一次 | KWin 只在启动会话时拉起输入法 | 无法规避，这是 KWin 的设计 |
| 部分合成器不给 `text_input_rectangle` | 协议里它是"提示"而非强制 | 候选窗位置由合成器决定，输入法无法自行微调 |

---

## 十、目录结构

```
wayland-ime/
├── build.sh                   一键构建
├── Makefile
├── proto/
│   ├── gen.sh                 生成 client/server 协议绑定
│   └── vendor/                vendored 协议 XML（构建期不再依赖 wayland-protocols 包）
├── src/                       见"源码职责"表
│   └── dict_gen.h             生成的词库（约 7.4MB 源码，勿手改）
├── tools/
│   ├── mkdict.py              构建期词库生成（公开拼音 + 双词频数据）
│   └── mkfont.c               构建期字形烘焙
├── scripts/
│   ├── fetch-dict.sh          下载词库数据源（均可再分发，MIT）
│   └── find-font.sh           定位系统 CJK 字体
├── test/                      单测 + 假合成器集成测试
├── packaging/                 control / 维护者脚本 / xime-setup / deb 组装
└── third_party/stb_truetype.h public domain 的 TTF 光栅化器（仅构建期使用）
```
