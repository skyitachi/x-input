# xime（Rust 版）

> **分支说明**：`feat/slint-ui` 分支把候选窗 UI 换成了 Slint 软件渲染，**放弃了「零动态依赖」前提**（体积 9MB→16MB）。协议层与引擎不变。详见 `../docs/slint-ui.md`。本 README 描述的是 master 的手写软光栅版本。

Wayland 原生的中文拼音输入法（含英文前缀补全），纯 Rust 实现，**单个静态二进制、零动态依赖、零系统接线**（不注入任何进程、不需要环境变量；输入法自身的配置界面是另一回事，见下）。

同一方案先前有一个 C 版本（`../wayland-ime/`）。Rust 版不是把 C 代码翻译一遍，而是换了两个关键决策，
换来一项 C 版做不到的性质：**安装包没有硬 Depends**（XWayland 覆盖所需的 fcitx5
客户端模块以 `Recommends` 声明，只拉 qt/gtk 模块、不拉 fcitx5 守护进程）。

> C 版自 2026-11 起**冻结**，不再添加新功能，只用于协议/渲染的对照与回归基准。
> 本 README 里「与 C 版一致」说的是**算法同源**（词网格 N-best、代价模型），不是能力等价：
> 新词库、当期语料、模糊音与拼写错容错这些后续能力只在本实现里。

---

## 一、为什么值得用 Rust 重写

C 版能工作，但它依赖 4 个动态库：

```
libwayland-client.so.0   libxkbcommon.so.0   libm.so.6   libc.so.6
```

`libwayland-client` 和 `libxkbcommon` 是绕不过去的——只要用 `wayland-rs` 或 libwayland，
就得链上它们。而 Rust 两条路都能绕开：

| 环节 | C 版做法 | Rust 版做法 | 结果 |
|---|---|---|---|
| Wayland 通信 | libwayland-client 封装 | **自己实现 wire 协议**（Unix socket + 定长头 + SCM_RIGHTS） | 不再链 libwayland |
| keycode→keysym | libxkbcommon | **自己解析 XKB keymap 文本** | 不再链 xkbcommon |
| libm | log10 | 标准库自带 | 不再链 libm |
| libc | glibc | **musl 静态链接** | 不再链 libc |

四条都断掉之后，产物是：

```
$ file xime
xime: ELF 64-bit LSB executable, x86-64, statically linked, stripped

$ ldd xime
        statically linked
```

对一个"装完就能用"的输入法来说，这不是炫技：**没有硬 Depends 意味着不会因为
系统升级 soname 而失效，也不会和别的输入法框架的依赖树打架。**（X11/XWayland
应用需要 fcitx5 客户端模块，由 `Recommends: fcitx5-frontend-all` 声明——只拉
qt/gtk 客户端模块、不拉 fcitx5 守护进程，缺省时 `xime-setup --auto` 会补装。）

顺带还有一个反直觉的结果：早期 Rust 版比 C 版**快**。
37 字整句逐键解码 C 版 9.32ms、Rust 版 5.67ms（同为 14.3 万词库、纯词频打分），原因
见第四节。但词库和模型都升级过之后，结论反过来了：**现在是 17.0 ms 对 9.32 ms**，
多出来的时间全部花在三阶语言模型的上下文查表上（见「六点六」）。这是有意的取舍——
16 ms 一帧的预算里，把 CPU 换给排序质量。

---

## 二、能力边界

中英两种输入走的是同一条 Wayland 输入法链路，切换只改输入法内部状态，
所以下表对两种模式都成立。协议上有**三个前端**，共用同一个引擎与按键状态机：
wlroots 系用 `zwp_input_method_v2`，KWin 用 `zwp_input_method_v1`（`--frontend auto`
自动识别），GNOME 走 IBus（`--ibus`）。

| 合成器 | 支持 | 说明 |
|---|---|---|
| KDE Plasma (KWin) | ✅ | 走 `zwp_input_method_v1`；必须由 KWin 拉起（写 `kwinrc` 的 `InputMethod`，值必须是 `.desktop`）；`xime-setup` 自动接线 |
| sway / Hyprland / labwc | ✅ | 直接作为普通客户端运行（`zwp_input_method_v2`） |
| GNOME (Mutter) | ✅ | 用 `--ibus` 前端。Mutter 没实现 `zwp_input_method_v2`，但它实现了 IBus 这条完全不同的路，见「第三条路」 |
| GTK3 / GTK4 / Qt / Electron 应用 | ✅ | 走 `zwp_text_input_v3`；设了 `QT_IM_MODULE`/`GTK_IM_MODULE` 的 X11/XWayland 应用改由 DBus 前端接（见下） |
| XWayland 应用 / xterm | ❌ | 不在协议覆盖范围内（要用只能走 XIM，那是框架的路子；KDE 上同理，KWin 只为 wl_surface 建上下文） |

### 环境变量：`QT_IM_MODULE=fcitx` 不再需要清掉

早期只有 Wayland 前端时，会话里存在下面任意一个变量都会让 Qt/GTK 应用改走 fcitx
的 DBus 模块、**不建 `zwp_text_input_v3`**，xime 一行日志都收不到：

```sh
QT_IM_MODULE=fcitx        # 或 fcitx5，两者等价（插件按服务名选协议，不看这个值）
GTK_IM_MODULE=fcitx
XMODIFIERS=@im=fcitx
```

现在由 `xime-both` 同时跑两个前端，这条 DBus 通道正好被接下，于是：

| 应用 | 实际走哪条 | 结果 |
|---|---|---|
| Wayland 原生 Qt6 | 内置 `text-input-v3`（Qt6 根本不加载 fcitx5-qt 插件，实测） | ✅ 原生候选窗 |
| Wayland 原生 GTK3/4 | `text-input-v3`，除非设了 `GTK_IM_MODULE` | ✅ 原生候选窗 |
| XWayland / X11 的 Qt（微信、WPS） | fcitx5-qt → DBus 前端 | ✅ 候选窗由客户端自绘 |
| XWayland / X11 的 GTK | fcitx5-gtk → DBus 前端 | ✅ 候选窗由客户端自绘 |

因此**不需要为了 Wayland 应用去注释这些变量**，也不需要给单个应用单独设值。
`xime-setup` 以前会注释 `~/.config/environment.d/99-fcitx5-im.conf`，现在这一步已经反转：
它只会把旧版注释掉的变量**恢复回来**（XWayland 应用靠这些变量加载 fcitx5-qt/gtk 再连
`xime --dbus`），不会再去注释。不设变量时 Wayland 原生应用照样走原生通道，
只是 X11/XWayland 应用没有中文。

完整的 KDE 接入说明、两个会让连接当场断掉的协议签名坑、以及 XWayland 的局限，
见 [`docs/kde-input-method-v1.md`](docs/kde-input-method-v1.md)。

终端能不能用，只看一件事：**它有没有实现 `zwp_text_input_v3`**。实现了就和普通
Wayland 应用一样，不需要额外适配。

| 终端 | 支持 | 说明 |
|---|---|---|
| foot | ✅ | 编译带 `+ime`（1.10+ 默认带），`foot --version` 里能看到 |
| kitty / WezTerm / Ghostty | ✅ | 原生 `text-input-v3` 客户端 |
| Konsole / GNOME Console 等 GTK4/Qt6 终端 | ✅ | 走各自工具包的 text-input 实现 |
| Alacritty | ❌ | 上游明确把 IME 列为 non-goal（PR #7289 被拒） |
| xterm / 跑在 XWayland 下的终端 | ❌ | X11 路径，需要 XIM 前端 |

实测步骤（Hyprland + foot）：

```sh
pkill fcitx5                 # 席位一山不容二虎，必须先腾出来
xime-both                    # 或 systemctl --user enable --now xime.service（崩溃自启 + 日志进 journal）
# 在 foot 里直接打 nihao，候选窗应贴着光标出现
```

**2026-11-26 复测结论（Hyprland 0.56.2 + foot 1.28.0 `+ime` + xime 0.3.0）[实测]**：

| 项 | 结果 |
|---|---|
| 协议 | 走 `zwp_input_method_manager_v2` + `zwp_text_input_manager_v3`（foot 用 v3，日志 `content_type ... purpose=13`） |
| 预编辑 | foot 自己渲染带下划线的 `nihao` |
| 候选窗 | 独立 popup surface，贴在光标下方，6 条 + `‹ ›` 翻页箭头 |
| 上屏 | 空格提交「你好」，落盘字节 `e4 bd a0 e5 a5 bd` |
| 翻页 | `=` 翻到第 2 页（转/赚/专/转股/传/砖），`-` 回上一页 |
| 拼写容错 | `nihap` 的首选仍是「你好」 |
| 按键回注 | `Ctrl+C` 原样进终端，`cat` 被终止 |

完整记录与截图见 [`../docs/xime-app-matrix.md`](../docs/xime-app-matrix.md) 第八节。

自动化测试注意：**不要用 `wtype` 注入**。它会创建 `zwp_virtual_keyboard_v1` 并上传自己生成的
最小 keymap（约 1.6 KB），合成器把这份 keymap 转发给正在抓键盘的 xime，而 `keymap.rs` 的解析器
要求至少 20 个可解析键位（`filled < 20` 即放弃），于是回退到旧 keymap、键码错位，终端里会出现
`^[[1234` 之类的乱码。改用 [`tools/uinput-send.py`](tools/uinput-send.py) 通过 `/dev/uinput`
造临时键盘（本机 `crw-rw-rw-`，不需要 root），按键就与真人敲击等价：

```sh
python3 tools/uinput-send.py text:nihao sleep:1.2 shot:/tmp/preedit.png space enter
```

`--check` 现在会直接告诉你两件最容易踩的事：

```
  input-method 席位        已被占用（另一个输入法持有）
  text-input（应用侧）      可用（zwp_text_input_manager_v3）
  其他输入法进程            fcitx5
      → 先停掉它们（如 pkill fcitx5），否则 xime 拿不到席位
```

终端里没有候选窗但能上屏时，用 `--inline-candidates` 把候选塞进 preedit，
由终端自己的字体渲染——不依赖 popup surface，兼容性最好。


之所以不需要框架、不需要环境变量、不需要用户手工接线，是因为**没有往任何进程里注入东西**：
输入法是 `zwp_input_method_v2` 的一端，应用是 `zwp_text_input_v3` 的一端，合成器负责转接。

**「零配置」指的是零系统接线，不是输入法自身不能有配置。** 候选词条数、翻页键位、
候选窗配色这类输入法自身的设置，完全可以走 fcitx 式的图形设置界面，且技术路线与
候选窗同源：在同一个二进制里再开一个 xdg_toplevel（或 layer-shell）窗口，用同一套
软光栅绘制，鼠标事件照常接收——候选窗是穿透的，设置窗不必穿透。加一份
`~/.config/xime/config.toml` 读写即可。这是产品化方向，不动摇零依赖的架构前提。

### 第二条路：DBus 前端（`xime --dbus`），与第一条路同时跑

上面表格里打 ❌ 的几种情况——X11/XWayland、以及只认 fcitx 的程序
（微信、WPS、Electron）——由 `--dbus` 前端覆盖。它根本不碰 Wayland 协议，
而是抢下 fcitx5 的服务名 `org.fcitx.Fcitx5`，让程序**内置的** fcitx5-qt /
fcitx5-gtk 客户端连上来：

```sh
xime-both                # 推荐：Wayland 前端 + DBus 前端一起起
xime-both --status       # 看两个前端各自在跑没、服务名归谁
xime --dbus --verbose    # 只起 DBus 前端（需先停掉 fcitx5，服务名唯一）
```

`xime-both` 是普通 shell 脚本（`packaging/xime-both`，装成 `/usr/bin/xime-both`）：
Wayland 前端在后台语义上前台运行，DBus 前端作为它的兄弟进程；收到 SIGTERM 一并回收，
Wayland 前端自己崩了也**不**杀 DBus 那半边（X11 应用还要用）。fcitx5 在跑时自动跳过
DBus 前端，避免抢服务名。

| 场景 | Wayland 前端 | `--dbus` 前端 | `--ibus` 前端 |
|---|---|---|---|
| Hyprland / sway / KDE | ✅ | ✅ | ✅（IBus 也能跑） |
| GNOME Wayland | ❌ Mutter 无该协议 | ⚠️ 只在应用带 fcitx5-qt/gtk 模块时可用 | ✅ **唯一可用的路** |
| X11 会话 / XWayland 应用 | ❌ | ✅ | ✅ |
| 微信 / WPS / Electron | ❌ | ✅ **已实测** | ✅ |

KDE 那格的「Wayland 前端 ✅」指的是 `zwp_input_method_v1`：xime 已实现这个前端，
由 KWin 拉起、`--frontend auto` 自动识别，Wayland 应用全功能可用；但 KWin 只为
`wl_surface` 建输入法上下文，**XWayland 应用（微信/WPS）仍然只能用 `--dbus`**。

三条路**不再互斥**：应用在启动时自己选一条，xime 各前端分别接住自己那一半。
唯一不能做的是「同一个应用同时走两条」——那是 Qt/GTK 启动时单选决定的。
GNOME 上通常只跑 `--ibus`（GNOME 的客户端只认 IBus，另外两条去不了那儿）。

这条路有个意外的好处：候选窗**由应用自己画**（Qt 与 GTK 客户端都声明
`ClientSideInputPanel`，bit 39；实测 Qt 的能力位是 `0xe000800072`、
GTK 是 `0xc001000032`），所以 xime 不需要 atlas、render、shm、popup 任何代码——
恰好是 Wayland 前端最费劲的那一半。代价是它和 fcitx5 互斥，且系统里要有
`fcitx5-qt` / `fcitx5-gtk` 模块（只装模块，不需要跑 fcitx5 守护进程）。
协议细节与实测记录见 `docs/fcitx5-dbus-frontend.md`。

按键行为与 Wayland 前端保持一致，另有几处这个前端特有的：

| 按键 | 行为 |
|---|---|
| `1`–`9` | 选当前页的第 n 条（翻页后仍按页内序号算） |
| `-` / `=`、PageUp / PageDown | 上一页 / 下一页（每页 9 条，边界不循环） |
| 空格 / 回车 | 中文提交当前页首候选；英文提交补全词且**不吞空格/回车** |
| `,` `.` `?` `;` `:` 等 | 组字中先上屏候选，再出全角标点（与 Wayland 前端同一张映射表） |
| Shift 单击 | 中英切换（英文态是前缀补全，不是直通）；切换时把未上屏的组字串原样提交，不丢字 |
| Ctrl+Shift+F | 简繁切换 |
| Ctrl+其它 | 一律透传（`Ctrl+C` 不会被当成字母 `c`） |
| 密码框 / PIN | 客户端声明 `Password`/`Sensitive` 时全键透传，且丢掉残留组字 |

注意 `Ctrl+Space`：omarchy 的 Hyprland 默认把它绑成了全局快捷键（modmask 4），
按键在合成器层就被吃掉，应用和输入法都收不到——这种环境下用 Shift 单击切中英。
判断方法见文档里的「按键到底有没有送到」一节。

它还实现了 `/controller` 上的 `org.fcitx.Fcitx.Controller1`（读接口 +
`SetCurrentIM`），所以桌面顶栏的「中／英」指示器、`fcitx5-remote -n`
这类小工具照常工作——不实现的话状态栏会直接抛异常退掉，用户看到的就是
「装了它别的应用输入法坏了」。想让它常驻：

```sh
systemd-run --user --unit=xime-dbus --collect -p Restart=always \
  /path/to/xime --dbus
systemctl --user stop xime-dbus && systemctl --user start omarchy-fcitx5   # 切回
```

### 第三条路：IBus 前端（`xime --ibus`），给 GNOME 用

GNOME 是唯一一个「既有输入法框架、又不肯实现 `zwp_input_method_v2`」的桌面：
Mutter 到 51 仍然没有这个协议（wayland.app 的合成器表里它是 `x`，连 KWin 都实现了
的 v1 它都没有）。而 GTK4 应用、Firefox / Chromium / Electron 的 Wayland 模式
**完全无视 `GTK_IM_MODULE`**，只走 `text-input-v3` → mutter → IBus。所以这两条路
在 GNOME 上都到不了 xime，只能在 IBus 这一层接。

`xime --ibus` 就是让 xime 直接变成一个 IBus 引擎：

```sh
xime --ibus             # 由 ibus-daemon 按需拉起，一般不用手动跑
xime --ibus --verbose   # 排握手问题时用
```

它做四件事，和官方 `ibus-engine-simple` 完全一致：连上 ibus-daemon 的总线、
申请同名总线名、在 `/org/freedesktop/IBus/Factory` 提供 `CreateEngine`、
把每个输入上下文的引擎对象挂在 `/org/freedesktop/IBus/Engine/<n>` 上。
装包时附带 `packaging/xime-ibus.xml`（装到 `/usr/share/ibus/component/xime.xml`），
ibus-daemon 读它之后「设置 → 键盘 → 输入源」里就会出现「xime 拼音」。

> ⚠️ **最容易踩的坑：ibus-daemon 默认不开在会话总线上。**
>
> 它自建一个私有 D-Bus 服务（如 `unix:path=/run/user/1000/ibus/dbus-XXXX`），把地址
> 写进 `~/.config/ibus/bus/<machine-id>-unix-<display>`（文件里是 `IBUS_ADDRESS=...`）。
> 客户端和引擎都必须按 libibus 的规则去解析这个地址。**直接连
> `DBUS_SESSION_BUS_ADDRESS` 会连到另一条总线**，daemon 永远看不到引擎，表现为
> `ibus engine xime` 卡 15 秒后报 `Set global engine failed: Timeout was reached`，
> 而引擎进程自己还活得好好的——这个现象非常容易误判成协议写错。
> `--ibus` 的地址解析顺序与 libibus 一致：`$IBUS_ADDRESS` →
> `$IBUS_ADDRESS_FILE` → `~/.config/ibus/bus/<machine-id>-unix-<display>` →
> 扫该目录取 daemon 还活着且最新的那个。

功能上与另外两个前端的唯一让步是**候选窗交给 gnome-shell 画**：GNOME 不支持
`wlr-layer-shell`，自绘浮窗没有可靠落点，所以候选表通过 `UpdateLookupTable` 信号
交给 IBus 面板，翻页、选词、预编辑都走标准接口。

```sh
ibus engine xime                 # 切到 xime
ibus engine                      # 看当前引擎
ibus list-engine | grep -A2 中文  # 看 xime 有没有被注册上
```

---


## 三、架构

```
src/
├── main.rs     事件循环、按键路由、提交/回注、候选窗生命周期
├── wire.rs     Wayland wire 协议：连接、编解码、fd 传递（SCM_RIGHTS）
├── proto.rs    协议请求构造（每个函数对应 XML 里一个 request，opcode 标注来源）
├── keymap.rs   XKB keymap 文本解析 → keycode/keysym 表（替代 libxkbcommon）
├── dict.rs     词库：include_bytes! 零拷贝 + 二分查找 + O(1) 长度过滤
├── engine.rs   N-best 解码器
├── ngram.rs    三元语言模型：KN 回退链 + 上下文预热（Ctx）
├── fuzzy.rs    模糊音与拼写错：音节表 + 音节级替换 + 编辑距离 1
├── render.rs   软光栅：圆角矩形 + 字形 alpha 叠加 + 彩色 emoji 合成 + BMP 输出
├── font.rs     字形图集解析
├── emoji.rs    彩色 emoji：ARGB 图集 + 拼音/英文前缀检索
├── popup.rs    候选条布局与配色
├── en.rs       英文前缀补全：字母序二分 + 词频排序
├── zh.rs       简繁转换：单字映射 + 词级例外 + 词条可见性
├── session.rs  三个前端共用的按键状态机（中英状态、组字串、翻页、共享状态文件）
├── fcitx_dbus.rs  DBus 前端：冒充 fcitx5 守护进程（`--dbus`）
├── ibus.rs     IBus 前端：Factory/Engine 对象 + IBUS_ADDRESS 解析（`--ibus`）
└── shm.rs      memfd + mmap 提供 wl_shm 像素内存
data/
├── dict.bin    33.3 万词条（9.9MB）
├── val_map.bin 词条 id → 词形 id（1.3MB，供 ngram 查概率）
├── ngram.bin   三元语言模型（25.5MB，KN 平滑，33.2 万形/131 万对/140 万三元）
├── en.bin      1 万英文词（133KB）
├── en-words.txt  英文原始词表（按词频降序，75KB）
├── zh.bin      简繁转换表（275KB）
├── atlas.bin   9033 字形 8bpp 图集（4.4MB）
├── emoji.bin   1375 个彩色 emoji 的 32bpp 图集（3.2MB）
├── emoji.txt   emoji 词表：每行的 emoji + 它的检索键
├── emoji-keys.bin  emoji 键索引（字母序，93KB，供前缀二分）
└── hant-chars.txt  繁体字符集（供 mkfont 烘焙字形）
```

数据在**构建期**烘焙成二进制，运行时用 `include_bytes!` 映射进只读段：
不解析、不分配、不依赖 fontconfig/freetype，也不携带 TTF 文件。

### 3.1 彩色 emoji

候选里能出 emoji，拼音与英文单词键都能命中：`xiaolian` 出 😀😃，中文模式下直接敲
`fire` 出 🚒🔥（这条拼音串没有对应词，emoji 表按字母前缀命中）。
emoji 插在候选第 6、7 格——不抢首位（打 `xiaolian` 首选仍是「笑脸」），
但三种候选窗形态都在第一页看得见。

颜色是自带的，不靠系统字体：

- `tools/mkemoji.c` 在构建期用 FreeType 读 NotoColorEmoji 的 CBDT 位图，
  双线性缩放到 20px 后存成**预乘 ARGB** 图集。正文那套 8bpp alpha 掩码装不下颜色。
- `scripts/gen-emoji.py` 生成词表：英文键来自 Unicode 的 emoji-test.txt 名称，
  拼音键来自 CLDR 中文注解（`😀 -> 笑脸`）再过一遍词库反查单字读音。
- 两条渲染路径都已接上：手写 Canvas 直接按预乘像素 `blend_into`；
  Slint 那条把 emoji 切成独立图片段（Slint 的软件渲染器只认轮廓字体，
  彩色位图交给它只会描出黑色外形）。

只收单字形 emoji（单码点或「码点 + VS16」），ZWJ 合成序列（👨‍👩‍👧）与国旗跳过：
候选窗按码点逐格查图集，画不了合成序列，硬收只会变成几个 emoji 并排。

### 3.2 为什么自己写 wire 协议

`wayland-backend` 0.3 已经移除了纯 Rust 后端，只剩 `client_system`（链 libwayland）
和 `dlopen` 两种，都绕不开那个 `.so`。既然目标是"零动态依赖"，就只能自己实现线协议。
好在协议本身很小：

```
u32 object_id
u32 (总长度 << 16) | opcode
参数区：int/uint 四字节；string = u32(长度含 NUL) + 字节 + NUL + 补齐
fd 不占消息体，走 SCM_RIGHTS 附属数据，按消息流顺序与 fd 参数一一对应
```

三个坑（都用官方 XML 与 Hyprland 生成的接口头核对过）：

1. `zwp_input_method_v2.commit(serial)` **带 serial**，且必须等于已收到的 `done` 事件计数。
   写错会被严格实现的合成器静默丢弃。
2. `grab_keyboard` 之后所有按键都归输入法，**未消费的必须用虚拟键盘原样回注**，
   否则 `Ctrl+C`、方向键、`Tab` 全部失效。代码按 keycode 记账（含抬起），避免按键卡死。
3. fd 不能"每条消息分一批"——一批读进多条消息时，只有真正带 fd 参数的那条才能取走它。

### 3.3 为什么自己解析 keymap

合成器会通过 `grab.keymap` 发来一份 XKB keymap（约 35KB 文本），C 版交给 libxkbcommon 编译。
输入法真正需要的只有一件事：**某个 keycode 在当前 shift/caps 下产生哪个 keysym**。

所以只解析两段：

- `xkb_keycodes`：`<AD01> = 24` 给出名字到 keycode 的映射
- `xkb_symbols`：`key <AD01> { [ 0x71, 0x51 ] }` 给出前两列 keysym

xkbcomp 输出的是**十六进制数值**（`0x71`）而非名字（`q`），这一点踩过一次坑。
解析失败时回退内置 US 布局表，`XIME_KM_DEBUG=1` 可以打印解析统计。

修饰键按 XKB 标准位：Shift=bit0、Lock=bit1、Control=bit2、Mod1(Alt)=bit3、Mod4(Super)=bit6。

---

## 四、解码器：与 C 版一致，但快一倍

算法与 C 版相同：词网格上的 N-best 路径搜索，唯一差别是每个位置保留 9 条路径
（C 版 6 条；多出的几条留给竖排/网格形态，横排仍只显示 6 条，见第六节），
三类边分别是整词、简拼（`nh → 你好`）、尾部音节补全（`nih → 你好`）。

```
cost(词) = 10 - log10(freq)      # 常数项取 10 补偿纯 unigram 无上下文
```

差别在实现细节上。移植时漏掉一个优化，导致性能差 5 倍，定位过程值得记一笔：

**症状**：37 字要 39.7ms（C 版 9.3ms）。单独量每类查询才看出问题：

| 查询 | 优化前 | 优化后 |
|---|---|---|
| `exact("n")` 单字母 | **34.25 μs** | 0.105 μs |
| `exact("ni")` | 7.58 μs | 0.211 μs |
| `exact("nihao")` | 0.16 μs | 0.16 μs |

**原因**：查"n"时按前缀扫描，会线性扫掉所有以 n 开头的词条（几千条）。
拼音是逐键输入的，每个位置都要查 `rem` 次，开销被放大成平方级。

**解决**：词条按拼音字节序排列，同一个 key 的条目是连续的；而 key 长度可以直接
从偏移差算出——blob 里 key 与 value 紧邻存放，`key_len = val_off - key_off - 1`，**O(1)**。
于是二分定位后，长度不匹配就立即结束，不必逐个比较字符串。

```
exact_into: lower_bound(p) → 长度 != len(p) 就返回
            → 长度相同则只可能是 p 本身
```

这个优化同样适用于 C 版（C 版没有做），所以 Rust 版反而更快：

| 场景 | C 版 | Rust 版 |
|---|---|---|
| `nihao`（5 键） | 0.166 ms | 0.297 ms |
| 17 字整句 | 3.046 ms | 2.681 ms |
| 23 字整句 | 4.572 ms | 3.072 ms |
| 37 字整句 | 9.324 ms | **5.674 ms** |

（含逐键重算；`nihao` 那一项因为绝对值太小、受进程噪声影响，不具代表性）

> 上表是 **14.3 万词库 + 纯词频打分** 时代的对比，只用来说明「O(1) 长度过滤」这个
> 优化的价值，不代表当前状态。今天的数字是 33.3 万词库 + 三元模型下的 **17.0 ms**
> （C 版仍是 9.32 ms），差距全在上下文查表，见「六点六」的性能一节。

**词库规模会线性影响这里的数字。** 词条从 14.3 万扩到 33.3 万后，同一台机器上
37 字整句由 9.98 ms 涨到 14.5 ms（+45%）——瓶颈不在边枚举，而在尾部补全的
`prefix_into`：它按前缀扫描同一段连续区间，词条翻倍、扫描长度就翻倍（单字母键
`n` 一次就要 51 μs，是 `exact` 的 400 倍）。要收回这部分开销，需要给前缀查加一个
「首字母 + 长度」分桶的索引，属于独立的优化项。

繁体形态在候选输出环节多一次查表转换，37 字整句 5.54 ms → 6.19 ms（+12%），
仍在 C 版一半左右；简体形态不受影响。

---

## 五、验证

```
$ cargo test --release
running 46 tests ... test result: ok. 46 passed; 0 failed
```

覆盖：常用词、整句解码、简拼、尾部补全、多音字（银行/重庆）、ü 写作 v、
码位直输（`u4f60 → 你`）、退格、候选去重、长度上限、状态重置；
简繁（简体不混繁体、繁体按词转换、切换即时重算）；
英文（前缀补全按词频排序、大小写形态、候选上限、原样可上屏）；
候选窗（三种形态尺寸、列数约束、英文态配色与尺寸不变）；
按键判定（`Ctrl+Shift+F`、Shift keycode、字母大小写保真）；
连接接管（`WAYLAND_SOCKET` 的 fd 校验、`FD_CLOEXEC` 与非阻塞补齐）；
单实例锁（flock 二次加锁被拒、fd 关闭后锁释放、目录不可写时降级可用）。

**协议级端到端测试**用的是 C 版那套假合成器（`../wayland-ime/test/mock_compositor.c`，
用 libwayland-server 实现 compositor/shm/seat/input-method/virtual-keyboard）。
它支持用环境变量替换按键序列，所以同一条链路能分别验证中文与英文：

| 变量 | 作用 |
|---|---|
| `XIME_CLIENT` | 被测二进制（默认 `./build/xime`） |
| `XIME_ARGS` | 传给它的参数，例如 `--en` |
| `MOCK_KEYS` | 按键序列：小写字母＝该键，大写＝Shift+该键，另有 `space`/`enter`/`esc`/`backspace`/`shift` |
| `MOCK_EXPECT` | 期望的 `commit_string`，默认「你好」 |
| `MOCK_INHERIT` | 设为 1 时不用命名 socket，改用 socketpair + `WAYLAND_SOCKET` 传 fd，复现 KWin 拉起输入法的方式 |

默认场景（中文）：

```
$ XIME_CLIENT=./xime ../wayland-ime/build/mock_compositor
xime: keymap 已解析（483 键）
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

英文链路。`--en` 默认**透传**（不出候选窗），补全要显式开 `--en-completion`：

```
$ MOCK_PURPOSE=0 MOCK_KEYS="hel enter" XIME_ARGS=--en \
  XIME_CLIENT=./xime ../wayland-ime/build/mock_compositor
ok   密码框无组字（preedit 1 次）
ok   密码框无 commit_string
ok   4 键全部原样回注（含数字）
全部通过

$ MOCK_KEYS="hel space" MOCK_EXPECT="help" XIME_ARGS="--en --en-completion" ...
ok   提交内容 "help" 符合预期
全部通过

$ MOCK_KEYS="Hel space" MOCK_EXPECT="Help" ...       # Shift+首字母 -> 首字母大写
$ MOCK_KEYS="shift hel space" MOCK_EXPECT="help" ... # 单按 Shift 切英文
```

密码框与终端是特例，靠 `content_type` 事件自动识别：

- `content_purpose=password/pin`（GUI 密码框）→ **全键透传**：不组字、不弹候选、Shift 切换一并禁用，敲什么进什么；
- `content_purpose=terminal`（终端）→ 英文模式自动退化为纯透传（终端里英文补全只有坏处），中文模式照常组字；
- 英文模式**默认就不出候选窗**（字母直接进应用，与 fcitx5 一致）；想要前缀补全得显式加 `--en-completion`；
- 英文模式的回车/空格会在上屏后**原样转发按键本身**——旧实现把回车吃掉，sudo 输完密码连提交都收不到。

假合成器用 `MOCK_PURPOSE=8|14` 模拟这两类焦点，断言「无组字、无提交、按键全部原样回注」。

最后一条同时验证了 Shift 的延迟切换：`Hel` 那次 Shift 一直按着，所以没有触发中英切换，
否则打出来的会是中文候选而不是 `Help`。

KDE 那条「由合成器拉起」的路径单独跑一遍——合成器不给命名 socket，而是把一条已连接的
fd 通过 `WAYLAND_SOCKET` 塞过来，xime 必须直接接管它：

```
$ MOCK_INHERIT=1 XIME_CLIENT=./xime ../wayland-ime/build/mock_compositor
模式：MOCK_INHERIT（WAYLAND_SOCKET 传递连接）
ok   候选窗 surface 已创建 / 键盘 grab 已建立 / 空格选中首选并提交 "你好"
全部通过
```

**渲染一致性**：`--render-test` 产出的候选条与 C 版**逐字节相同**
（`cmp` 无差异），说明软光栅、字形取样、混合、圆角与布局完全对齐。

**KDE 真机（Plasma 6.6.6 / KWin 6.6.6 / Wayland）**：`zwp_input_method_v1` 前端逐项
实测通过——候选窗贴光标、内联 preedit、空格上屏、数字选词、全角标点、退格、
Esc 取消、不消费的键经 `ctx.key` 回注、`ctx.modifiers` 回传修饰键状态。
清单与关键日志见 [`docs/kde-input-method-v1.md`](docs/kde-input-method-v1.md) 第六节。

---

## 六、构建与安装

构建期依赖：`cargo`、`python3`、系统 `fontconfig` 开发库（Slint 软件渲染用系统字体，
需 pkg-config 可查到 freetype/fontconfig）。主线走宿主 gnu target，产物动态链接
fontconfig 等库；不再需要 musl target。重新生成数据（`scripts/gen-data.sh`）还需要
`python3-opencc`（生成简繁转换表）：Arch `pacman -S python-opencc`，其他平台 `pip install opencc`。
`data/*.bin` 已生成好，日常构建不需要装它。

```sh
./build.sh
#  1/4 数据（复用 data/*.bin，缺失时调用 ../wayland-ime/tools 生成；
#      简繁表由 scripts/gen-zh.py 用 OpenCC 预生成，并顺带导出繁体字符集给字体烘焙）
#  2/4 宿主编译（gnu 动态链接；Slint 候选窗需 fontconfig）
#  3/4 单元测试
#  4/4 打包 → dist/xime_0.3.0_amd64.deb + dist/xime-0.3.0-1-x86_64.pkg.tar.zst

# Debian/Ubuntu 系：
sudo apt install ./dist/xime_0.3.0_amd64.deb

# Arch 系（Arch/CachyOS/Omarchy 等）：
sudo pacman -U dist/xime-0.3.0-1-x86_64.pkg.tar.zst

# 两种包文件布局一致、零依赖；安装/卸载钩子等价。包里带了三样接线材料：
#   /usr/share/applications/xime.desktop   KDE 系统设置里的「虚拟键盘」条目
#   /etc/xdg/autostart/xime.desktop        fcitx5 式开机自启（KDE/GNOME 除外）
#   /usr/lib/systemd/user/xime.service     与 fcitx5 同款生命周期（Omarchy 上 fcitx5 就是这么跑的）
# 安装钩子自动执行 xime-setup --auto：KDE 写 kwinrc；有 systemd 用户会话就
# enable xime.service 并停用 omarchy-fcitx5.service；都没有才写会话启动项。
xime-setup --status
```

### 源码直装（开发/自用，不打 deb 包）

改了代码只想在本机试，用不带 sudo 的这一条：

```sh
packaging/xime-install            # cargo build --release + 装到 ~/.local/bin + 校验哈希
packaging/xime-install --no-build # 只装现有的 target/release/xime
packaging/xime-install --fix-desktop  # .desktop 的 Exec 与安装路径错位时顺手改写
```

**为什么必须有这一步**：`xime-switch` / `xime-setup` 都**不装二进制**。`xime-switch`
拉起的只是 `~/.local/bin/xime` 这份拷贝，KDE 下更绕——kwinrc 的 `InputMethod`
指向 `xime.desktop`，而它的 `Exec` 是写死的绝对路径。所以「`cargo build` 出来的
`target/release/xime`」和「实际在跑的那份」是两条互不相干的线：漏装一次就会一直跑
旧版，而且没有任何报错。`xime-install` 把两步合成一条，装完打印两份 md5 与
`--version`，顺带检查 `.desktop` 的 `Exec` 是否指向本次安装路径。

装的时候若 xime 正在跑，它会提示：进程持有的是已加载的旧映射，替换文件不会换代码，
需要 `xime-switch fcitx5` 再 `xime-switch xime` 让 KWin 重新拉起。

### 让桌面「选到」xime

Wayland 没有输入法注册表，所以不存在一个跨桌面通用的「输入法列表」。能不能选，
取决于合成器怎么管理输入法：

| 桌面 | 系统设置里能选吗 | xime 怎么被拉起 |
|---|---|---|
| KDE Plasma (KWin) | 能：**系统设置 → 键盘 → 虚拟键盘** | 包内桌面条目（`X-KDE-Wayland-VirtualKeyboard=true`）+ kwinrc 的 `[Wayland] InputMethod` |
| Hyprland / sway / labwc | 没有这个概念 | 写进会话启动项；谁绑上 `zwp_input_method_v2` 席位谁是输入法 |
| GNOME (Mutter) | 能：**设置 → 键盘 → 输入源 → 中文**，列表里是「xime 拼音」 | 装 `packaging/xime-ibus.xml` 到 `/usr/share/ibus/component/xime.xml`，ibus-daemon 按需 exec `xime --ibus` |

GNOME 的列表是**按 IBus component XML 发现**的：`/usr/share/ibus/component/*.xml` 里
每个 `<engine>` 就是「输入源」里的一个条目，`<component><name>` 同时是引擎进程要申请的
总线名（必须和 `src/ibus.rs` 的 `BUS_NAME` 逐字一致）。

KDE 的列表是**按桌面条目发现**的：`/usr/share/applications` 下带
`X-KDE-Wayland-VirtualKeyboard=true` 的条目才会出现（fcitx5、plasma-keyboard 都靠这一行
被发现）。选中之后 KWin 把条目路径写进 `~/.config/kwinrc`：

```ini
[Wayland]
InputMethod=/usr/share/applications/xime.desktop
VirtualKeyboardMode=2
```

两个值都有讲究：

- `InputMethod` 直接写可执行文件路径（`/usr/bin/xime`）也能跑起来，但**列表里不会显示为已选中**，
  因为列表只认桌面条目。这就是「装好了却选不到」的典型原因。
- `VirtualKeyboardMode=0` 表示禁用，KWin 根本不会拉起输入法。`xime-setup` 只在它缺失或为 0 时
  改成 2（触摸/数位板/鼠标都启用），其余值一概不动。

还有更隐蔽的一层：KDE 不让输入法自己去连 socket，而是**由 KWin 拉起进程，把一条已经连好的
fd 通过 `WAYLAND_SOCKET` 交过去**，只有这条连接才被允许绑定 `zwp_input_method_v2`。自己按
`WAYLAND_DISPLAY` 另开一条，拿到的是普通客户端身份，绑定时会被合成器拒掉。xime 启动时先看
`WAYLAND_SOCKET`，有就接管，没有才自己连；`xime --check` 第一行会写明当前是哪一种。

```sh
xime-setup --status              # 会话类型 / 桌面条目 / kwinrc 当前值 + 协议自检
sudo xime-setup --kde-default    # 只写 KDE 那部分（装包时已自动执行）
sudo xime-setup --auto           # 按当前桌面自动接线：KDE 写 kwinrc，wlroots 写启动项
```

### 与 fcitx5 共存与切换（xime-switch）

xime 和 fcitx5 是对等的输入法，可以随时互相切换，两边都装也没关系——xime 内置
会话级单实例锁（`$XDG_RUNTIME_DIR/xime.lock` 上的 flock）：systemd、KWin、XDG
autostart、手工启动不管哪条路把第二个实例拉起来，后来者都会安静退出（退出码 0，
不会把 systemd 的 `Restart=always` 逼进重启循环）。没有明确拒绝语义的合成器上
（Hyprland 就是静默换人），两个输入法同时跑的表现是「候选窗时有时无」，这把锁
把整类问题挡掉。

```sh
xime-switch              # 谁在跑就切到另一个
xime-switch xime         # 明确切到 xime
xime-switch fcitx5       # 明确切到 fcitx5
xime-setup --bind        # 给切换绑个快捷键（默认 CTRL + ALT + SPACE，可用 --bind 覆盖）
```

切换的原则是**怎么启动的就怎么停**：

- **systemd 用户单元**——Omarchy 上的 fcitx5 就是 `omarchy-fcitx5.service`（带
  `Restart=always`，光 `pkill fcitx5` 两秒后就被拉回来）：disable + stop 对方的
  单元，enable + start 己方的单元。重启后依然保持选择，这正是 fcitx5 的管理方式。
- 非 systemd 的会话：pkill 掉在跑的，直接拉起新的。
- **KDE**：两边都归 KWin 管，切换就是改 kwinrc 的 `[Wayland] InputMethod` 并通知
  KWin 重载（`kwriteconfig6 --notify`），候选窗当场换人，不用注销。
  KWin 6.6 对单次变更常常不重载，`xime-switch` 会自动走「先切对端再切回目标」的
  两段式兜底确保生效；切完还会轮询进程，确认目标输入法真的起来了。
  两边的 desktop 条目都**不带** `--dbus`——xime 已有 `zwp_input_method_v1` 前端，
  KDE 下由 `--frontend auto` 自动选中（见 `docs/kde-input-method-v1.md`）。

`xime-setup --auto` 的接线优先级：KDE 写 kwinrc → 有 systemd 用户会话且装了
`xime.service` 就走 systemd（同时停用 fcitx5 的单元，防止登录时两者赛跑）→
都没有才落到 exec-once / `o.launch_on_start("xime")` 这类会话启动项。任何时候
`xime-setup --status` 都能看到当前跑着谁、哪些单元处于 enabled。

### deepin / X11 会话（xime-compat）

`xime.service` 带 `ConditionEnvironment=WAYLAND_DISPLAY`。deepin 虚拟机、
lightdm + startdde 这类 X11 会话没有这个变量，systemd 会直接跳过单元；而
`xime-switch` 只看 `systemctl start` 的返回码，于是打印「xime 已启动」，
`pgrep -x xime` 却为空，末尾显示「当前: none」。

`xime-compat` 专门补这一层（幂等，可重复执行）：

```sh
xime-compat                  # 启动：写 systemd drop-in、停 fcitx5、拉起 xime
xime-compat status           # 看会话类型、进程、org.fcitx.Fcitx5 归属、IM 变量
xime-compat env              # 只写 environment.d + ~/.xprofile
xime-compat stop             # 停止 xime（不自动恢复 fcitx5）
```

X11 下它会清掉 `WAYLAND_DISPLAY` 条件，让 `xime-both` 按会话只起 DBus 前端；
同时把 `QT_IM_MODULE` / `GTK_IM_MODULE` / `XMODIFIERS` 写进
`~/.config/environment.d/99-xime-im.conf` 和 `~/.xprofile`，覆盖 systemd 与
DDE/lightdm 两条继承链。环境变量登录时读取，改完注销重登最稳；临时验证可用
`QT_IM_MODULE=fcitx5 应用名`。X11 应用还依赖 `fcitx5-frontend-all`
（包的 `Recommends`），缺了运行 `sudo apt install fcitx5-frontend-all`。

### 调试与自检

```sh
xime --check                      # 协议可用性 + 词库/字形/简繁数据统计
xime-switch                       # xime ⇄ fcitx5 互相切换（运行时生效）
xime-setup --bind "SUPER+M"       # 给 xime-switch 绑个快捷键
xime --query jintiantianqihenhao  # 打印候选与解码代价
xime --query toufa --zh hant      # 繁体形态下看候选（头发 -> 頭髮）
xime --query hel --en             # 英文前缀补全（hel -> help/held/hello…）
xime --bench                      # 解码性能 + 词库查询微基准
xime --keymap-test /tmp/keymap.txt # 抽查 keymap 解析结果
xime --render-test out.bmp nihao  # 候选条渲染成 BMP
xime-rs/tools/deepin-probe.sh     # deepin/X11：DBus + uinput 模拟按键分层诊断
xime --verbose                    # 打印协议绑定与每次 preedit/commit
```

### 候选窗形态

与 fcitx5 的候选面板一样支持多种形态，全部由同一套软光栅绘制，
不需要额外依赖或主题文件：

| 形态 | 参数 | 一次显示 | 说明 |
|---|---|---|---|
| 横排 | `--popup horizontal`（默认） | 6 条 | 单行横条，贴着光标 |
| 竖排 | `--popup vertical` | 9 条 | 单列列表，选中项整行高亮 |
| 网格 | `--popup grid --popup-cols N` | N×3，上限 9 | 多行多列，N 默认 3 |

配色两套内置：`--theme light`（默认）/ `--theme dark`，更多见下一节。

```sh
xime --popup vertical              # 竖排候选
xime --popup grid --popup-cols 4   # 4 列网格
xime --theme dark                  # 深色候选窗
xime --theme fcitx:omarchy         # 直接用 fcitx5 的主题
xime --render-test out.bmp nihao --popup grid   # 任意形态预览成图
```

![横排](dist/popup-horizontal.png) ![竖排](dist/popup-vertical.png) ![网格](dist/popup-grid.png)

形态只影响"画多大、怎么排"，与解码逻辑解耦：`popup.rs` 里一个 `Style`，
`measure` 和 `paint` 各自按形态分支，主循环只把 `Style` 传下去。

### 外观配置：直接吃 fcitx5 的 theme.conf

`Style` 里的颜色和所有边距都能配，落到 `config.ini`：

```sh
# 三级，后者覆盖前者
/etc/xime/config.ini
~/.config/xime/config.ini
XIME_THEME=fcitx:omarchy xime     # 环境变量只覆盖主题
```

```ini
[xime]
layout = horizontal     ; horizontal | vertical | grid
cols = 3
theme = fcitx:omarchy   ; light | dark | fcitx:名字 | 主题文件路径

[colors]
bg = #f7fbfc            ; 支持 #RGB / #RRGGBB / #AARRGGBB
text = #17181c
accent = #2f6bff
selected_text = #ffffff
border = #24000000

[metrics]
pad_x = 11
pad_y = 6
item_pad_x = 9
item_gap = 4
row_gap = 2
radius = 11
hl_radius = 7
hl_pad_y = 1
border_width = 1
```

**不用重启**：主循环每 400ms 检查一次配置文件指纹（mtime + 大小），变了就重算
`Style`，接着的 `refresh` 会用新度量重新 `measure`，尺寸变了自动重建 shm 缓冲。

#### 复用 fcitx5 主题

`--theme fcitx:omarchy` 会去 `~/.local/share/fcitx5/themes/<名字>/theme.conf`
和 `/usr/share/fcitx5/themes/<名字>/theme.conf` 找主题并直接映射。
`xime --list-themes` 列出本机能找到的全部主题，`xime --dump-theme --theme fcitx:omarchy`
打印映射结果（排查"为什么颜色不对"用这个）。

字段对应关系（fcitx5 的 KConfig 是 INI，解析器只实现需要的子集）：

| fcitx5 theme.conf | xime | 说明 |
|---|---|---|
| `[InputPanel] NormalColor` | `text` | 候选文字色 |
| `[InputPanel] HighlightCandidateColor` | `selected_text` | **选中项文字色** |
| `[InputPanel] HighlightBackgroundColor` | `accent` | 选中块底色 |
| `[InputPanel/Highlight] Color` | `accent`（回退） | 上面那个键缺失时用 |
| `[InputPanel/Background] Color` | `bg` | 面板底色 |
| `[InputPanel/Background] BorderColor` / `BorderWidth` | `border` / `border_width` | 描边 |
| `[InputPanel/ContentMargin]` + `[InputPanel/Background/Margin]` | `pad_x` / `pad_y` | fcitx 是两层相加 |
| `[InputPanel/TextMargin] Left` | `item_pad_x` | 文字左右留白 |
| `[InputPanel] Spacing` | `item_gap` / `row_gap` | 候选间距，缺省 0 |
| `[InputPanel/Highlight/Margin] Top` | `hl_pad_y` | 选中块上下外扩 |
| 背景亮度 | 浅色/深色判定 | fcitx 没这个键，按 BT.601 luma 推断 |
| `Image=`（九宫格 PNG/SVG） | **不支持** | xime 没有图片解码器，见「已知限制」 |
| `PageButtonAlignment` / `[InputPanel/PrevPage]` | **不支持** | 还没做分页 |

fcitx 的纯色主题（Omarchy 生成的就是这种）能 100% 还原：颜色、两层边距、
2px 描边、直角高亮块都对得上。带背景图的主题只会退化成纯色底。

![fcitx 主题](dist/themes/omarchy-h.png)

### 英文（前缀补全）

Shift 单按切中英。英文模式不是"关掉输入法让按键直通"，而是一个**带补全的独立模式**：
敲 `hel` 弹候选，空格上屏 `help`。

| 操作 | 行为 |
|---|---|
| 字母 | 进缓冲，候选窗按词频补全（1 万高频词） |
| 空格 | 上屏首选 |
| 回车 | 上屏你敲的字母本身（不要补全） |
| 数字 | 选第 N 个候选 |
| 退格 | 删一个字母，候选即时重算 |
| Esc | 清空缓冲 |
| Shift + 首字母 | 首字母大写（`Hel` → `Help`） |
| Caps Lock | 整词大写（`HELP`） |

**Shift 的切换判定放在松开时**：按下 Shift 再按字母是输入大写，只有单独按一下就松开才切模式。
这不是细节——fcitx5 同样是这么做的，否则 `Shift+A` 想打大写 A 会先被切成中文。

英文态候选窗的选中块是青色、中文态是蓝色，不靠文字标记，一眼分得出。

```sh
xime --en                          # 启动即英文（透传，不出候选窗，与 fcitx5 一致）
xime --en --en-completion          # 英文 + 前缀补全候选窗
xime --query hel --en              # 看某个前缀的补全结果
xime --render-test e.bmp hel --en  # 渲染英文候选窗
```

![英文横排](dist/popup-en-horizontal.png) ![英文竖排](dist/popup-en-vertical.png)

为什么英文单独一层、不复用拼音引擎：

- **两件事本质不同**。拼音要在词网格上搜 N-best 路径（同一串字母能被切成不同音节组合），
  英文只是"查出以它开头的词、按常用度排序"，一次二分就够。为共用而合并，两边都会变差。
- **排名依据不同**：中文只看词频；英文还要处理大小写形态（句首、Caps Lock），这是拼音没有的维度。
- **数据独立**：`data/en-words.txt`（按词频降序的一万词）由 `gen-en.py` 重排成字母序 + 词频索引，
  运行时只查表，依旧零依赖。

英文模式**刻意不做**拼写纠错、下一词预测、自动加空格或大写句首。这些在英文写作里都是替用户做主，
输入法先做到"不添乱"就够了。

### 简体 / 繁体

简繁是**输出形态**，不是两套词库：同一套词频、同一条解码路径，只在候选产出后按形态渲染。

| 形态 | 参数 | 行为 |
|---|---|---|
| 简体 | `--zh hans`（默认） | 词库里 `變成`/`变成` 这类重复条目只留简体写法，候选里不混繁体 |
| 繁体 | `--zh hant` | 按 OpenCC `s2t` 口径整词转换：`头发 → 頭髮`、`出发 → 出發`、`干净 → 乾淨` |

运行中 **`Ctrl+Shift+F`** 切换（与 fcitx5 同组按键），已输入的候选立即按新形态重画，
不必重新输入，也不会打乱候选顺序。

![简体候选](dist/popup-hans.png) ![繁体候选](dist/popup-hant.png)

为什么不是"装两套词库"：

- **词库源数据本来就混着繁体写法**（3867 条，且词频常常更高）。不处理的话，`biancheng`
  的第一候选会是 `變成` 而不是 `变成`——这不是"功能"，是候选简繁混排。
- **逐字转换会出错**：`头发` 逐字转成 `頭發`（错，应为 `頭髮`），`干净` 成 `幹淨`（错，应为 `乾淨`），
  `乾隆` 成 `幹隆`（错）。一个字对应多个繁体时只能整词判定。
- **两套词库会分裂排名**：同一句拼音在两种形态下候选顺序不同，用户切换形态时看到列表"跳位"。
  这里只在输出环节转换，解码与词频完全共用。

实现分三层，数据在构建期预生成（`data/zh.bin`，97KB），运行期只查表、不依赖 OpenCC：

| 表 | 内容 | 作用 |
|---|---|---|
| 单字映射（1727） | 简 → 繁 | 任意词的逐字兜底 |
| 词级例外（2518） | 整词 → 整词 | 逐字会错的词走这张表，转换时按**最长匹配**命中 |
| 词条位图（2×18KB） | 每词条一位 | 标记"繁体形"与"非规范繁体写法"，决定该条目在哪种形态下可见 |

字形也要跟上：繁体用字有 1036 个不在词库字符集里，构建期由 `scripts/gen-zh.py`
导出 `data/hant-chars.txt` 交给 `mkfont` 一起烘焙，否则候选窗里那些字会是空白格。

```sh
xime --zh hant                      # 启动即繁体
xime --query biancheng --zh hant    # 變成 / 編成 / 編程 …
xime --render-test t.bmp toufa --zh hant   # 繁体候选窗预览
```


导出当前 keymap 用于测试：

```sh
cat > /tmp/km.c <<'EOF'
#include <stdio.h>
#include <xkbcommon/xkbcommon.h>
int main(void){struct xkb_context*c=xkb_context_new(0);
struct xkb_rule_names n={.layout="us"};struct xkb_keymap*k=xkb_keymap_new_from_names(c,&n,0);
FILE*f=fopen("/tmp/keymap.txt","w");fputs(xkb_keymap_get_as_string(k,XKB_KEYMAP_FORMAT_TEXT_V1),f);
fclose(f);return 0;}
EOF
cc -o /tmp/km /tmp/km.c $(pkg-config --cflags --libs xkbcommon) && /tmp/km
```

---

## 六点五、扩充词库

词库是**构建期烘焙**的（`data/dict.bin` 用 `include_bytes!` 嵌进二进制），所以加词
不是改配置，而是改源数据再重新构建。源数据有三层，加词的入口是第三层：

| 层 | 位置 | 内容 |
|---|---|---|
| 读音 | `wayland-ime/.cache/dict/large_pinyin.txt` | 词 → 逐字读音（多音字在词中读音正确） |
| 词频（存量） | `jieba_dict.txt` + `zh_freq_50k.txt` | 2014 年前后的书面语 + 字幕口语，决定基础排序 |
| 词频（当期） | `wayland-ime/.cache/dict/modern_freq.txt` | 当月维基百科 dump 统计，只用来**提频**（见下节） |
| **人工词表** | `wayland-ime/tools/extra-words.txt` | 前几层都没有、或词频被噪声压住的词 |

```sh
# 1. 先查：到底是没收录，还是排位太后？工具会直接给出该补的那一行
cd wayland-ime
python3 tools/audit-dict.py 内卷 抑制 蓝牙耳机 --file my-words.txt

# 2. 把建议行追加到 tools/extra-words.txt
#    内卷	nei juan	40000     # 词<TAB>拼音<TAB>词频，拼音可省

# 3. 重新生成数据并编译
cd ../xime-rs && sh scripts/gen-data.sh && cargo build --release
```

几条容易踩的：

- **词频只能提、不能降**（与已有词条取大），所以给常见词随手填个数不会把排名弄坏。
- **多音字词必须手写拼音**：自动推导走的是「单字首选音」，`空调` 会被拼成
  `kongdiao`（调首选 diào），`调休` 成 `diaoxiu`。工具会标 `(读音按单字首选音拼)`。
- **人工词表里的词，自动推导不再收**：`mkdict.py` 会把 `extra-words.txt` 的词排除在
  「单字首音兜底」之外。不这么做，同一个词会挂到两个键上——`重写` 既按单字音进
  `zhongxie`（重=zhòng，错）又按人工读音进 `chongxie`（对），用户打 `chongxie` 反而
  出不来。所以「给词补读音」这一步同时也在「把错误读音踢掉」。
- **已有分领域词表**：`extra-words.txt` 按 职场/数码/生活/医疗/金融/网络/家居 分组，
  其中金融与计算机两段覆盖行情与财务、交易动作、宏观指标，以及算法与数据结构、
  操作系统、数据库、分布式与运维、机器学习/大模型的常用术语。加词前先用
  `audit-dict.py` 查一遍，多半已经收录，只是排位问题。
- **同音窗口是 6 条**（`MAX_SAME_KEY`，与 `src/dict.rs` 的 `HITS` 必须一致）。
  排到第 7 位以后的词等于不存在——要么给它提频进前 6，要么同步调大这两个常量
  （词库体积会线性增长）。
- **改了 `dict.bin` 就要管 `val_map.bin` / `ngram.bin`**：两者的 `val_id` 是
  「按词形词频排的名次」，词库一变就整体平移。`gen-data.sh` 在有语料时会自动重跑
  `tools/mkngram` 重建两张表；**没有就地下策**——旧表的 id 平移没有正确翻译方法
  （`remap-bigram.py` 只认二元表格式，对三元表无用）。跳过这步不会报错，只会让
  上下文消歧静默错位，所以 `gen-data.sh` 发现 `dict.bin` 比 `ngram.bin` 新时会打印警告。
- **新字要重烘字形**：`gen-data.sh` 会顺带跑 `mkfont-ft`（读 `dict_gen.h` 的字符集 +
  `hant-chars.txt`）。只换 `dict.bin` 不重烘图集，新词里没见过的字会显示成空白格。

---

## 六点六、当期语料与三元语言模型（维基百科 dump）

存量语料停在 2014 年，这不是「有噪声」而是**结构性缺口**：`内卷/躺平/破防/充电宝/
大模型/直播间` 在 jieba 与字幕源里计数为 0，任何排序算法都救不回来，只能一个个往
`extra-words.txt` 里手填。补一份当期语料，这批词就能自己进来。

```sh
cd wayland-ime
sh scripts/fetch-corpus.sh          # 下载 zhwiki 最新 dump（约 3.3GB）+ THUOCL 领域词表
python3 tools/wiki2txt.py --jobs 8 --stride 6   # 抽正文 -> .cache/corpus/wiki.txt
python3 tools/mkmodernfreq.py       # 统计词频 -> .cache/dict/modern_freq.txt
cd ../xime-rs && sh scripts/gen-data.sh
```

实测规模：3.3GB 压缩 → 4.98M 页 → 每 6 页取 1 页 → 5.42M 句 / 161.8M 字 / 456MB 文本，
8 进程清洗耗时 19.6 分钟；词频统计单进程 3.5 分钟、峰值内存约 3GB。

### 为什么是「提频」而不是「混合」

一开始按概率线性混合（0.28/0.32/0.40），结果 `年假`（字幕源 1487 次）输给了同音的
`年甲`（维基 751 次），用户打 `nianjia` 首选变成生僻词。原因是**维基是百科体，它的
分布本来就不是「人会输入什么」的模型**——权重给多大都不对。

现在改成单调提频：

```
prob = max(prob_存量, alpha * prob_当期)     仅当 当期 > 2 × 存量 时覆盖
```

- `alpha` 不是拍出来的常量：取两套语料共有的中频词（5.5 万个）的比值中位数
  （本次 0.239）。两套语料的总量口径不同，不校准会让当代语料整体虚高 4 倍。
- `MODERN_MARGIN = 2.0` 卡掉「差一点点」的情况——那多半是题材差异而不是词真的变常用。
  存量语料完全没有计数的新词（0 → 有）不受门槛限制，照进不误。

效果：135 个常用拼音键的首选变化 **0 个**；130 词抽测里只有 `死锁` 由第 2 提到第 1；
词形 331,291 → 332,289（换入 1.2 万、换出 1.1 万，变动集中在同键尾部：生僻专名如
`阿比西尼亚人`/`阿尔金山脉` 被维基里更常用的同音词挤掉，换上来的是 `逝世/歧义/姓氏`
这类本来就该有、但 2014 年语料里没计数的常用词）。

### 三个踩过的坑

- **不能 `subprocess` 起 `bzip2` 再读管道。** worker 是 fork 出来的，会连管道写端一起
  继承；`bzip2` 退出后写端仍被 worker 握着，父进程的 `read()` 永远等不到 EOF。表现是
  「跑到 75% 就不动了，CPU 还在转」。解压必须在父进程里用 `bz2` 模块做。
- **惰性正则 + 不配对的标记会退化成 O(k·n)。** `\{\|.*?\|\}` 遇到一个含 3000 个 `{|`
  却没有 `|}` 的模板页，单次 `clean()` 能跑十几分钟（两次全量抽取都卡在同一页）。
  现在每个多行正则前都有便宜的 `in` 判断，`RE_FILE` 也去掉了嵌套量词。
- **必须用 `--stride` 而不是 `--max-pages`。** dump 按页面 ID 排序，前 20% 基本是
  2003~2008 年创建的基础条目；截断等于把近十年的条目全丢掉，正好丢掉最需要的部分。

### 语言模型：三阶 modified Kneser-Ney（对齐 kenlm）

原来的 `bigram.bin` 是「PMI 折算的二阶表」，`TRANS_WEIGHT=0.4` 只让它在词频排序上做
微调。这种表有两个天花板：一是看不到更早的上下文（`及其/机器 + 学习` 在二阶里长得
一模一样），二是 PMI 不是概率，无法与词频口径统一，权重只能拍脑袋。

现在换成三阶模型，语义与 kenlm / ARPA 完全一致：

```text
P(w|a,b) = P3(w|a,b)              三元命中
         = bow(a,b) · P(w|b)      三元未命中（bow 挂在 2-gram 行上）
P(w|b)   = P2(w|b)                二元命中
         = bow(b) · P1(w)         二元未命中（bow 挂在 1-gram 行上）
```

| 环节 | kenlm / lmplz | `tools/mkngram.rs` |
|---|---|---|
| 平滑 | modified Kneser-Ney（Chen-Goodman） | 同：每阶独立拟合 `D1/D2/D3` |
| 回退 | ARPA 链，`log10` 域相加 | 同 |
| 低阶计数 | 接续计数（不同左上下文个数） | 同 |
| 剪枝 | `--prune` 按相对熵 | 计数阈值 + 全局配额（简单、可复现） |
| 二进制 | trie + q8 量化 | 分组有序表 + i16 定点（1e-3 log10） |

**引擎里的口径**（`engine.rs`）：

```text
cost = 词频代价 + 词形代价 + CTX_WEIGHT · (log10 P1(w) − log10 P(w|ctx))
```

`CTX_WEIGHT = 1.0`（原来 `TRANS_WEIGHT = 0.4`）。注意用的是**相对一元**的差值而不是
概率绝对值：这样词库词频（含人工提频、领域词表）继续主导排序，语言模型只在「两个
候选词频接近」时出手。绝对值口径会让模型压过词库，人工提频立刻失效。

规模（当期维基语料 5.42M 行 / 161.8M 字）：

| | 数量 | 说明 |
|---|---|---|
| 词形 | 332,289 | 与词库一一对应（`val_map.bin` 定序） |
| 二元 | 1,309,428 | 剪枝后（`--min-bigram 3`） |
| 三元 | 1,397,567 | 剪枝后（`--min-trigram 3`） |
| 体积 | 25.5MB | 旧的 `bigram.bin` 是 7.3MB |
| 构建 | 34.6 s | 单文件零依赖，`rustc --edition 2021 -O` |

自检：`ΣP1 = 1.0000`，二元与三元的每个上下文归一化偏差均 `0.0000`；导出的 ARPA 与
`ngram.bin` 打分一致到量化误差（±0.0005）。

```sh
cd ../xime-rs && sh scripts/gen-data.sh    # 内部调 tools/mkngram（自动编译）
# 或者只重建模型：
rustc --edition 2021 -O -o tools/mkngram tools/mkngram.rs
tools/mkngram --dict data/dict.bin --corpus ../wayland-ime/.cache/corpus/wiki.txt \
    --out-map data/val_map.bin --out-ngram data/ngram.bin \
    --top-bigram 1200000 --min-bigram 3 --min-trigram 3
# 加 --arpa /tmp/zh.arpa 可额外导出 ARPA，用来和 kenlm 互验
```

语料 `wiki.txt` 直接可用：`mkngram` 自己按词库做贪心最长匹配切分，**不需要 jieba**。

#### 四个踩过的坑

- **回退权重挂错行**（最隐蔽的一个）。ARPA 里 1-gram 行的 `bow` 是「二元未命中退回
  一元」用的，2-gram 行的 `bow` 才是「三元未命中退回二元」用的。最初把三元残差写进
  `uni_bow`，自检三元上下文偏差 0.6573——每个上下文都差一大截，但候选看起来还算正常，
  极难从输入结果上发现。修正后偏差归零。
- **整词切分会饿死内部字对**。语料里「我想」作为整词被切走 2471 次，`(我,想)` 这个
  二元对几乎不存在，KN 于是认为「我像」更常见，`woxiangchuquwan` 出「我像出去玩」。
  修法：切分时对 2 字以上的词，同时补记其**内部相邻字对**（`intra` 数组），整词路径与
  单字路径都拿得到统计。
- **HashMap 随机访问太慢**。一亿 token × 三元查表，`HashMap<u64, u32>` 是 2.4 μs/token，
  三亿次查表 245 s。改成 `Vec<u64>` 顺序 append + `sort_unstable` + 游程压缩，
  19.8 s 完成收集与排序，全套降到 34.6 s。
- **标点必须断链**。断句哨兵 `SEP = u32::MAX` 把「…模型，带来…」断成两条链，否则
  「模型→带」这类跨标点伪搭配会把「带模型」顶到「大模型」前面。

#### 性能：14.3 → 17.0 ms

三元查表是随机访存，直接把 37 键整句从 14.3 ms 推到 22.9 ms。逐步隔离测量：纯结构
开销 14.4 ms、`Ctx::prepare` 占 8 ms、`ctx.cost()` 只占 0.8 ms——瓶颈在「准备上下文
时跳着读」而不是「算概率」。

最终加了两个直映缓存（`ctx_cache` 1<<13 存预热好的 `Ctx`，`cost_cache` 1<<16 存
`(prev2, prev1, w) → cost`），并把 `Ctx` 的后继列表压进栈上 `[(u32, f32); 4]` 内联数组，
降到 **17.0 ms**（37 键，候选 21 条），23 键 9.9 ms。

这已经贴着 16.6 ms 帧预算（60Hz）的边。要继续压，方向是「同一前词的后继列表在内存里
连续」——现在是分组有序表，天然满足，但缓存槽位命中率还不够高。

#### 效果

- 全量用例里**唯一变了的首选**是 `womenyihui`：「我们一会」→「我们也会」。语料里
  「我们一会」0 次、「我们也会」15 次（书面语写「一会儿」），这是模型对数据的正确
  反应，「我们一会」仍在第二候选。
- `woxiangchuquwan` 恢复「我想出去玩」——这是三元引入后自己踩的坑（整词切分饿死内部
  字对），不是收益，见上面第二个坑。
- 真正的收益体现在 `trigram_context_sharpens_bigram` 断言的那类：「到达终点 的」
  三元代价比二元低 0.2 以上，而**没见过的上下文精确退回二元**（不掉分）。
- 其余 30 余条整句的首选不变——符合预期：`CTX_WEIGHT=1.0` 只在词频接近时说话。

### 语言模型还有一个回归基准：mkbigram

`tools/mkbigram.rs` 与 `wayland-ime/tools/mkbigram.py` 已不在主路径上（`gen-data.sh`
不再调用，`data/bigram.bin` 已从仓库移除）。它们输出的是上一代二元表，保留下来是为了
两件事：

1. **二元基线**。要说清「三元到底带来多少收益」，得能重建一版二元模型跑同一批抽样。
   重建路径：Rust 27.4 s / Python 77.6 s（同一份 5.42M 行语料）。
2. **互验**。不限 `--top` 时两版输出逐字节相同，这是那份 Python 参考实现的回归第一步。

```sh
rustc --edition 2021 -O -o xime-rs/tools/mkbigram xime-rs/tools/mkbigram.rs
wayland-ime/tools/mkbigram.py --corpus wiki.txt --top 9999999 \
    --out-map /tmp/a.map --out-bigram /tmp/a.bg
xime-rs/tools/mkbigram      --corpus wiki.txt --top 9999999 \
    --out-map /tmp/b.map --out-bigram /tmp/b.bg
cmp /tmp/a.bg /tmp/b.bg     # 无输出即一致
```

`--top` 生效时两者会有差异，且**不必对齐**：第 100 万名的边界上必然有一批计数相同的
pair，Python 的 `Counter.most_common` 按哈希迭代顺序取，Rust 按 `(count, prev, next)`
取。两边都是合法的「前 100 万」。

Rust 版比 Python 版快的四个地方，全是「数据搬运」而不是算法：

| | Python 版 | Rust 版 |
|---|---|---|
| 语料 | 整个文件读成 5.42M 个 `str` 常驻 | 一次 `read` 成 `Vec<u8>`，线程切 `&str` |
| 分块 | `Pool.starmap` 把每块的行列表 pickle 进管道 | 分块按字节偏移，线程共享同一份只读切片 |
| 切分 | 每位置切片出 `str` 再查 dict（命中即分配） | `&str` 零拷贝查表 |
| 计数 | `Counter[(p, n)]` 元组 | `HashMap<u64, u32>`，key = `(prev<<32)\|next` |

> 顺带纠正一个测试用例：`daimoxing` 断言「大模型」是错的。「大模型」的拼音是
> `damoxing`；`daimoxing` 能出「大模型」是因为「大」还有 `dài` 读音（大夫），
> 恰好拼出 `大(dài)+模型`。这条通路靠二元表对 `大→模型` 的评分，换表就翻到
> 「带模型」——现在测试改用正确拼音 `damoxing`（整词路径，代价 5.52，领先 5 个点）。

---

## 六点七、模糊音与拼写错

在此之前没有任何容错：打 `sijian` 出来的是「四溅」，`zongguo` 是「宗国」，
`nihap` 是「你哈珀」——三条都是字面上合法的拼音，引擎没有理由报错，可用户要的是
「时间」「中国」「你好」。用户能容忍的错分两类，处理方式不同：

| 类型 | 例子 | 做法 | 惩罚 |
|---|---|---|---|
| 方言模糊音 | `zongguo`→中国、`sijian`→时间、`lanfang`→南方 | 音节级的声母/韵母替换 | `FUZZY_PENALTY = 2.0` |
| 打错一个键 | `nihap`→你好 | 编辑距离 1 的合法音节替换 | `REPAIR_PENALTY = 4.0` |

规则只列真会被混淆的：声母 `zh↔z`、`ch↔c`、`sh↔s`、`n↔l`、`f↔h`、`r↔l`；韵母
`an↔ang`、`en↔eng`、`in↔ing`、`ian↔iang`、`uan↔uang`。替换后拼出来不是合法音节
就丢弃——`zhuang` 的 zh→z 得到 `zuang`，汉语里没这个音节，不生成。

### 三条把成本钉死的前提

1. **只在能完整切分时做模糊**。剩余串能切成合法音节（`sijian` = `si`+`jian`）才逐个
   音节生成变体；打到一半（`zhon`）不做。切不动时，只有坏尾**不是任何音节的前缀**
   （`hap` 不是，`h` 是）才当拼写错去修——否则用户只是还没打完，搜出来的全是垃圾。
2. **残余并回上一音节再判断**。`nihap` 会被贪心切成 `ni|ha|p`，残余 `p` 本身是合法
   前缀（pa/po…），单看残余会误判成「还在打字」。把 `p` 并回 `ha` 得到 `hap`，它
   不是任何音节的前缀，这才确认是真打错。
3. **等长限制 + 更重的惩罚**。修复只做等长替换、且惩罚 4.0。不加这两条时，`nih`
   会被修成 `ni`→「你」（3.6+4），压过简拼本该给的「你好」（4.8+2.0）。实测惩罚
   低于 3.5 就开始出这个问题。

### 效果与代价

| 输入 | 之前 | 现在 |
|---|---|---|
| `zongguo` | 宗国 | **中国** |
| `sijian` | 四溅 | **时间** |
| `lanfang` | 兰芳 | **南方** |
| `nihap` | 你哈珀 | **你好** |

37 键整句 14.30 → 14.29 ms（同机同数据，噪声范围内）；候选只多不停，
`zhonghuarenmingongheguo` 的候选数从 26 涨到 27 就是因为多了一条模糊边。

**为什么不做构建期折叠索引**：折叠表按词条数线性长（33 万条约 4MB，二进制 +18%），
而错拼修正只需要高频词能命中。运行时变体的成本是「每位置几个音节 × 2 次查表」，
实测被 exact 的二分开销淹没，不值得为此付体积。

### 不覆盖的

- **整句漏字母**：`zhongou` 仍是「中欧」。要修得在整串上搜插入，搜索空间按
  位置 × 字母爆炸。打错的音节本身少了字母时由尾音节补全兜（`zhonggu`→中国）。
- **连续两处以上错**：只做 1 处差异。
- **C 版没有**：C 版是参考实现，只对齐解码与协议，不追功能。

---

## 七、已知限制

| 限制 | 原因 | 影响 |
|---|---|---|
| 模糊音只认方言混淆与单键打错 | `fuzzy.rs` 的音节级替换（1 处）+ 编辑距离 1 | 整句漏字母（`zhongou`→中国）、两处以上错误、跨音节的错拼修不出来；见「六点七」 |
| 语言模型只到三阶，且训练语料偏百科 | `ngram.bin` 是词级 trigram（KN 平滑），上下文代价取「相对一元的差值」 | 需要四阶以上才解得开的长距离结构（`…的…的…`）解不了；`CTX_WEIGHT=1.0` 已开到满，再大就会压过词库的人工提频 |
| 模型经过剪枝 | 二元 `--min-bigram 3`、三元 `--min-trigram 3`；1310 万种 pair 全存会让二进制再涨 60MB 以上 | 长尾搭配查不到，退回 KN 的 `bow` 估算（平滑值，不是硬惩罚）。常见搭配都在表内，影响集中在生僻专名 |
| 同音窗口只有 6 条 | `MAX_SAME_KEY`（构建）与 `HITS`（运行）必须一致 | 同一拼音的第 7 个及以后的词等于不存在（如 `yizhi` 取不到「抑制」）；要放宽得同时改两个常量，词库体积线性增长 |
| 词频语料偏百科 | 存量语料是 2014 年的新闻+字幕，当期语料是维基百科 | 口语词靠存量语料排序（提频设计保证不被百科体压下去），但维基对口语的覆盖本身有限：新出现的口语/网络梗仍要 `extra-words.txt` 人工补 |
| 简拼只覆盖 2~4 字词 | 索引按词长筛选以控制体积 | 更长的简拼串不生效 |
| 不记忆用户选择 | 无用户词频学习 | 高频自造词需要写进词表 |
| 非 US 布局依赖 keymap 解析 | 只取每键前两列 keysym | 需要 AltGr 或三层的符号（如某些欧洲布局）可能取不到 |
| 修饰键按标准 XKB 位判断 | 未解析 `modifier_map` 重映射 | 自定义 modifier 映射的布局下 Ctrl/Alt 判断可能失准 |
| 改词库要重新构建 | 词条与字形编译进二进制 | 加词流程见「六点五、扩充词库」；新字要显示仍需重烘图集 |
| 英文只用一万高频词 | 词表取自公开的词频统计，不含专有名词与专业术语 | 罕见词、人名、代码标识符补不出来，按回车照原样上屏即可 |
| 英文不纠错、不预测下一词 | 刻意不做（见第六节） | 打错字不会被改正，也不会替用户补句首大写 |
| Plasma 下必须由 KWin 拉起 | 只有 KWin 交出的那条专用连接上才看得到 `zwp_input_method_v1`（普通客户端去 registry 翻不到） | 手工 `xime &` 在 KDE 下会打印原因并以退出码 1 结束（不会静默占住单实例锁）；走桌面条目或 `xime-setup`。`kwinrc` 的 `InputMethod` 只认 `.desktop`，写裸二进制路径会被静默忽略 |
| KDE 下切换输入法后，要先切一次焦点 | `activate` 由焦点变化驱动，KWin 不会在输入法重载瞬间补发 | 表现为「切过去直接打字没反应」，Alt+Tab 换一下窗口即恢复 |
| 无法在 X11/XWayland 应用里用（Wayland 前端） | KWin 只为 `wl_surface` 建输入法上下文，X11 窗口没有 wl_surface；wlroots 系则是协议本身不覆盖 | 用 `xime-both`：DBus 前端覆盖 X11/XWayland，Wayland 应用仍走原生前端，两边同时可用 |
| Qt/GTK 应用设了 `QT_IM_MODULE=fcitx` 时用不了 xime | 该应用改走 fcitx5-qt / fcitx5-gtk 的 DBus 模块，不再建 `zwp_text_input_v3` | 不用清掉变量：`xime --dbus` 冒充 fcitx5 守护进程接住它即可。值写 `fcitx` 或 `fcitx5` 都可以（插件按服务名 `org.fcitx.Fcitx5` 选协议） |
| 微信里用不了 xime（Wayland 前端） | 它虽是 Qt6 静态链接、确实创建 `zwp_text_input_v3`、xime 的 commit 也确实发出，但聊天框不插入文字；同样动作走 DBus 就正常，说明断在协议中继而非应用 | 交给 `xime-both` 的 DBus 前端，已实测可打出中文（输入框实测为 `n你好`） |
| 简繁只有 OpenCC `s2t` 一种口径 | 未生成 `s2tw`/`s2hk` 变体表（用词差异大，如 软件/軟體） | 台湾/香港用词习惯需另加一列转换表，接口已按 `Variant` 预留 |

---

## 八、与 C 版怎么选

| | C 版 | Rust 版 |
|---|---|---|
| deb Depends | `libc6`、`libwayland-client0`、`libxkbcommon0` | **无**（Recommends `fcitx5-frontend-all` 供 XWayland） |
| 构建期依赖 | gcc、wayland-scanner、libwayland-dev、libxkbcommon-dev | cargo + musl target |
| 体积 | 2.7MB / 安装 6.7MB | 3.1MB / 安装 7.6MB |
| 代码量 | 约 2200 行 C | 约 4600 行 Rust（含约 900 行单元测试） |
| 37 字解码 | 9.32ms | 17.0ms（14.3 万词库时代是 5.67ms，三元模型花掉了差额） |
| 语言模型 | 无（纯词频） | 三阶 modified Kneser-Ney，ARPA 回退链 |
| 移植到新架构 | 交叉编译工具链 | `rustup target add` 即可 |

要**绝对最小的依赖面**（例如不可变镜像、老旧发行版、容器内），选 Rust 版；
要**审计尽可能简单**（C 版代码更短且无 unsafe 之外的抽象），两个都可以。
