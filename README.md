# xime — Wayland 原生中文输入法

一个直接讲 Wayland 输入法协议的拼音输入法。**不依赖 fcitx/ibus 框架**，不注入
`GTK_IM_MODULE` / `QT_IM_MODULE`，不连 DBus（除 `--dbus` 前端外）、不需要图形设置界面，
装完一个包就能在 KDE / sway / Hyprland 等 Wayland 桌面上打中文。

## 一句话原理

输入法是 `zwp_input_method_v2` 协议的**一端**，应用是 `zwp_text_input_v3` 的**另一端**，
两者从不直接对话，全部由合成器（KWin / wlroots）转接。所以不需要往任何进程里注入东西，
单包零配置、零环境变量就能工作。覆盖面的边界：合成器不实现输入法协议（GNOME/Mutter）时
原生前端不可用，GNOME 改走内置的 IBus 前端（`xime --ibus`，Mutter 只认 IBus）；
应用在 XWayland 上时走内置的 fcitx5-DBus 前端（`xime-both`），
需搭配推荐安装的 fcitx5 客户端模块（qt/gtk）。

候选窗自带**彩色 emoji**：图集在构建期从彩色 emoji 字体烘焙进二进制，运行时同样不加载
任何字体。拼音（`xiaolian`）与英文单词（中文模式下敲 `fire`）都能命中，插在词候选之后的
第一页位置。

详细机制见 [`docs/xime-key-flow.md`](docs/xime-key-flow.md)。

## 目录结构

| 路径 | 说明 |
|---|---|
| [`xime-rs/`](xime-rs/README.md) | **Rust 版（主攻）**。纯 Rust 手写 wire 协议 + 手解析 XKB keymap + musl 静态链接，产物 `ldd` 显示 `statically linked`，deb 无硬 `Depends`（Recommends fcitx5 客户端模块供 XWayland）。当前分支 `feat/slint-ui` 候选窗改用 Slint 软件渲染（见 [`docs/slint-ui.md`](docs/slint-ui.md)） |
| [`wayland-ime/`](wayland-ime/README.md) | **C 版（冻结对照）**。同一方案的首版实现，依赖 libwayland-client / libxkbcommon / libm / libc 四个动态库，代码更短、便于审计。**自 2026-11 起冻结：不再添加新功能**，只修 bug 与跟随数据格式对齐 |
| [`docs/`](docs/) | 跨实现文档：全链路原理、应用×合成器兼容矩阵、Slint 迁移、fcitx5 已知问题 |
| [`fcitx5-preedit-fix/`](fcitx5-preedit-fix/README.md) | fcitx5 空预编辑丢更新的补丁（与 xime 无关，修的是 fcitx5 自身 bug） |
| [`probe-rs/`](probe-rs/) | Wayland 协议探针（实验性质，用纯 Rust `wayland-backend`，目前为空壳） |
| [`.workbuddy/`](.workbuddy/) | WorkBuddy 工具的本地记忆目录，与项目代码无关 |

## 两个实现怎么选

| | C 版（`wayland-ime/`） | Rust 版（`xime-rs/`） |
|---|---|---|
| 运行时依赖 | libwayland-client0、libxkbcommon0 等 4 个 `.so` | **无**（静态链接，deb 无硬 Depends） |
| 构建期依赖 | gcc、wayland-scanner、libwayland-dev、libxkbcommon-dev | cargo + musl target + 系统 CJK 字体 |
| 二进制体积 | 2.7MB（装 6.7MB） | 3.1MB（装 7.6MB，Slint 分支 15.9MB） |
| 37 字整句解码 | 9.32ms（14.3 万词库，纯词频） | 17.0ms（33.3 万词库 + 三阶语言模型） |
| 代码量 | 约 2200 行 C | 约 4600 行 Rust |
| 后续新功能 | **已冻结，不再添加** | 全部新功能在此推进（新词库、模糊音容错、候选窗形态） |

要**绝对最小的依赖面**（不可变镜像、老旧发行版、容器内）选 Rust 版；要**审计尽可能简单**
（代码更短）选 C 版。两者的协议时序与解码算法同源，但 C 版自 2026-11 起冻结，**能力已不再
等价**——词库扩充、当期语料、模糊音与拼写错容错都只在 Rust 版里。C 版现在的角色是最小可
审计的参照实现与回归基准，不是可选的功能对等版本。

## 快速开始

**Rust 版（推荐）**：

```sh
cd xime-rs
./build.sh                                  # 生成数据 → 静态编译 → 单测 → 打包 deb + Arch pkg
sudo apt install ./dist/xime_0.3.0_amd64.deb   # 或 sudo pacman -U dist/xime-0.3.0-1-x86_64.pkg.tar.zst
xime-setup --status                         # 看接线结果
```

**C 版**：

```sh
cd wayland-ime
./build.sh
sudo apt install ./dist/xime_0.2.0_amd64.deb
```

安装后如何被桌面「选到」、与 fcitx5 共存/切换、调试自检等，见各自 README 的「构建与安装」节。

## 能力边界（摘要）

**合成器**：KWin（Plasma 5.26+ / 6.x，必须由 KWin 拉起）、wlroots 系（sway / Hyprland /
labwc / river，`exec xime` 即可）✅；**GNOME/Mutter 走 `--ibus` 前端**（Mutter 不实现
`zwp_input_method_v2`，但它实现了 IBus，装好 component XML 后「设置 → 键盘 → 输入源」
里就能选到「xime 拼音」）。

**应用**：GTK3 / GTK4、Qt5 / Qt6（Wayland 后端）、Chromium / Electron（Ozone Wayland）✅；
XWayland / X11 程序 ❌；部分终端（alacritty 等自处理按键的）❌。

`--dbus` 前端可补上 X11/XWayland、微信 / WPS / Electron 这几块（代价：与 fcitx5
互斥，且要求系统里有 `fcitx5-qt` / `fcitx5-gtk` 模块）；`--ibus` 前端补上 GNOME / X11
（代价：候选窗由 gnome-shell 画，不是自绘）。

本机四个常用应用（Chrome / Konsole / 微信 / WorkBuddy）× KDE 的实测结论见
[`docs/xime-app-matrix.md`](docs/xime-app-matrix.md)。

## 文档索引

| 文档 | 内容 |
|---|---|
| [`docs/xime-key-flow.md`](docs/xime-key-flow.md) | 从按键到汉字的全链路：启动期四步拿到键盘、一个按键的六步、引擎 N-best 解码、与 fcitx5 逐项对照 |
| [`docs/xime-app-matrix.md`](docs/xime-app-matrix.md) | Chrome / Konsole / 微信 / WorkBuddy 在 KDE 与 Hyprland 下能否用 xime（实测 + 判定模型） |
| [`docs/slint-ui.md`](docs/slint-ui.md) | `feat/slint-ui` 分支：候选窗从手写软光栅迁移到 Slint 的约束、尺寸/缩放、验证 |
| [`docs/fcitx5-known-issues.md`](docs/fcitx5-known-issues.md) | fcitx5 自身的问题清单（与 xime 无关），含空预编辑丢更新的根因与补丁 |
| [`xime-rs/README.md`](xime-rs/README.md) | Rust 版完整说明：零依赖的来龙去脉、三种候选窗形态、主题、英文补全、简繁 |
| [`wayland-ime/README.md`](wayland-ime/README.md) | C 版完整说明（已冻结）：词库来源、代价模型、协议时序、构建验证 |

## 代码约定

- 函数名、测试函数名一律用**英文 snake_case**，不允许中文命名的函数（中文注释、断言消息、
  字符串字面量不受限）。
- **C 版（`wayland-ime/`）冻结**：不再添加新功能，只接受 bug 修复与数据格式对齐（改
  `dict.bin` 布局时同步重建 `src/dict_gen.h`）。新能力一律在 `xime-rs/` 实现，不要为
  「两边对齐」而往 C 版加代码。

## 协议与数据来源

- 协议 XML vendored 在 `xime-rs/` 与 `wayland-ime/proto/vendor/`，构建期不再依赖
  `wayland-protocols` 包。
- 词库来自可再分发的 MIT 数据（`mozillazg/pinyin-data`、`phrase-pinyin-data`、
  `fxsjy/jieba` 词频、`hermitdave/FrequencyWords`），外加**清华开放词表**
  （`thunlp/THUOCL`，MIT，提供「这确实是个词」的证据）和**中文维基百科 dump**
  （CC BY-SA 4.0，只统计词频、不分发原文）。
- 还有一份**自带的人工补充词表** `wayland-ime/tools/extra-words.txt`（新词与词频修正），
  构建期由 `tools/mkdict.py` 编译进二进制。加词流程见
  [`xime-rs/README.md`](xime-rs/README.md) 的「六点五、扩充词库」。
- 当期语料是**可选**的：`modern_freq.txt` 不存在时自动退回双源（2014 语料），
  词库仍能构建，只是新词要人工补。流程见「六点六、当期语料」。
- Rust 版的语言模型（`xime-rs/data/ngram.bin`）也用同一份维基语料统计：三阶
  modified Kneser-Ney 平滑 + ARPA 回退链，语义与 kenlm 一致，只存概率与回退权重，
  不分发原文。C 版（冻结）只有词频，没有上下文模型。
