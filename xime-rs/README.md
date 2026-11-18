# xime（Rust 版）

Wayland 原生的中文拼音输入法（含英文前缀补全），纯 Rust 实现，**单个静态二进制、零动态依赖、零系统接线**（不注入任何进程、不需要环境变量；输入法自身的配置界面是另一回事，见下）。

同一方案先前有一个 C 版本。Rust 版不是把 C 代码翻译一遍，而是换了两个关键决策，
换来一项 C 版做不到的性质：**安装包没有 Depends**。

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

对一个"装完就能用"的输入法来说，这不是炫技：**没有 Depends 意味着不会因为
系统升级 soname 而失效，也不会和别的输入法框架的依赖树打架。**

顺带还有一个反直觉的结果：Rust 版比 C 版**快**。
37 字整句逐键解码 C 版 9.32ms、Rust 版 5.67ms，原因见第四节。

---

## 二、能力边界

中英两种输入走的是同一条 `zwp_input_method_v2` 链路，切换只改输入法内部状态，
所以下表对两种模式都成立。

| 合成器 | 支持 | 说明 |
|---|---|---|
| KDE Plasma (KWin) | ✅ | 需由 KWin 拉起；`xime-setup` 自动写 `/etc/xdg/kwinrc` |
| sway / Hyprland / labwc | ✅ | 直接作为普通客户端运行 |
| GTK3 / GTK4 / Qt / Electron 应用 | ✅ | 走 `zwp_text_input_v3`，不需要注入 `GTK_IM_MODULE` |
| GNOME (Mutter) | ❌ | Mutter 没实现 `zwp_input_method_v2`，任何非框架输入法都不可用 |
| XWayland 应用 / xterm | ❌ | 不在协议覆盖范围内（要用只能走 XIM，那是框架的路子） |

终端能不能用，只看一件事：**它有没有实现 `zwp_text_input_v3`**。实现了就和普通
Wayland 应用一样，不需要额外适配。

| 终端 | 支持 | 说明 |
|---|---|---|
| foot | ✅ | 编译带 `+ime`（1.10+ 默认带），`foot --version` 里能看到 |
| kitty / WezTerm / Ghostty | ✅ | 原生 `text-input-v3` 客户端 |
| Konsole / GNOME Console 等 GTK4/Qt6 终端 | ✅ | 走各自工具包的 text-input 实现 |
| Alacritty | ❌ | 上游明确把 IME 列为 non-goal（PR #7289 被拒） |
| xterm / 跑在 XWayland 下的终端 | ❌ | X11 路径，需要 XIM 前端 |

实测步骤（Hyprland + foot 1.28）：

```sh
pkill fcitx5                 # 席位一山不容二虎，必须先腾出来
xime --verbose &             # 看日志确认 "activate" 与 "keymap 已解析"
# 在 foot 里直接打 nihao，候选窗应贴着光标出现
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
├── render.rs   软光栅：圆角矩形 + 字形 alpha 叠加 + BMP 输出
├── font.rs     字形图集解析
├── popup.rs    候选条布局与配色
├── en.rs       英文前缀补全：字母序二分 + 词频排序
├── zh.rs       简繁转换：单字映射 + 词级例外 + 词条可见性
└── shm.rs      memfd + mmap 提供 wl_shm 像素内存
data/
├── dict.bin    14.4 万词条（4.3MB）
├── en.bin      1 万英文词（133KB）
├── en-words.txt  英文原始词表（按词频降序，75KB）
├── zh.bin      简繁转换表（97KB）
├── atlas.bin   7536 字形 8bpp 图集（2.4MB）
└── hant-chars.txt  繁体字符集（供 mkfont 烘焙字形）
```

数据在**构建期**烘焙成二进制，运行时用 `include_bytes!` 映射进只读段：
不解析、不分配、不依赖 fontconfig/freetype，也不携带 TTF 文件。

### 3.1 为什么自己写 wire 协议

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

### 3.2 为什么自己解析 keymap

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

英文链路（前缀补全 + 空格上屏 + 大小写）：

```
$ MOCK_KEYS="hel space" MOCK_EXPECT="help" XIME_ARGS=--en \
  XIME_CLIENT=./xime ../wayland-ime/build/mock_compositor
ok   空格选中首选并提交 "help"
全部通过

$ MOCK_KEYS="Hel space" MOCK_EXPECT="Help" ...      # Shift+首字母 -> 首字母大写
$ MOCK_KEYS="shift hel space" MOCK_EXPECT="help" ... # 单按 Shift 切英文
```

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

---

## 六、构建与安装

构建期依赖：`cargo`、`python3`、`rustup target add x86_64-unknown-linux-musl`、
以及一个系统 CJK 字体（用于烘焙字形）。重新生成数据（`scripts/gen-data.sh`）还需要
`python3-opencc`（生成简繁转换表）：Arch `pacman -S python-opencc`，其他平台 `pip install opencc`。
`data/*.bin` 已生成好，日常构建不需要装它。

```sh
./build.sh
#  1/4 数据（复用 data/*.bin，缺失时调用 ../wayland-ime/tools 生成；
#      简繁表由 scripts/gen-zh.py 用 OpenCC 预生成，并顺带导出繁体字符集给字体烘焙）
#  2/4 静态编译
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

### 让桌面「选到」xime

Wayland 没有输入法注册表，所以不存在一个跨桌面通用的「输入法列表」。能不能选，
取决于合成器怎么管理输入法：

| 桌面 | 系统设置里能选吗 | xime 怎么被拉起 |
|---|---|---|
| KDE Plasma (KWin) | 能：**系统设置 → 键盘 → 虚拟键盘** | 包内桌面条目（`X-KDE-Wayland-VirtualKeyboard=true`）+ kwinrc 的 `[Wayland] InputMethod` |
| Hyprland / sway / labwc | 没有这个概念 | 写进会话启动项；谁绑上 `zwp_input_method_v2` 席位谁是输入法 |
| GNOME (Mutter) | 有列表，但只列 IBus 引擎 | 用不了：Mutter 没实现该协议 |

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

`xime-setup --auto` 的接线优先级：KDE 写 kwinrc → 有 systemd 用户会话且装了
`xime.service` 就走 systemd（同时停用 fcitx5 的单元，防止登录时两者赛跑）→
都没有才落到 exec-once / `o.launch_on_start("xime")` 这类会话启动项。任何时候
`xime-setup --status` 都能看到当前跑着谁、哪些单元处于 enabled。

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

配色两套：`--theme light`（默认）/ `--theme dark`。

```sh
xime --popup vertical              # 竖排候选
xime --popup grid --popup-cols 4   # 4 列网格
xime --theme dark                  # 深色候选窗
xime --render-test out.bmp nihao --popup grid   # 任意形态预览成图
```

![横排](dist/popup-horizontal.png) ![竖排](dist/popup-vertical.png) ![网格](dist/popup-grid.png)

形态只影响"画多大、怎么排"，与解码逻辑解耦：`popup.rs` 里一个 `Style`
（形态 + 列数 + 明暗），`measure` 和 `paint` 各自按形态分支，主循环只把
`Style` 传下去。后续接配置界面（`~/.config/xime/config.toml`）时，
这份 `Style` 就是唯一要落盘的东西。

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
xime --en                          # 启动即英文
xime --en-passthrough              # 英文但不补全，按键直接进应用
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

## 七、已知限制

| 限制 | 原因 | 影响 |
|---|---|---|
| 没有上下文语言模型 | 纯 unigram + 常数项 | 「及其学习」会排在「机器学习」前面这类词频歧义仍存在 |
| 简拼只覆盖 2~4 字词 | 索引按词长筛选以控制体积 | 更长的简拼串不生效 |
| 不记忆用户选择 | 无用户词频学习 | 高频自造词需要写进词表 |
| 非 US 布局依赖 keymap 解析 | 只取每键前两列 keysym | 需要 AltGr 或三层的符号（如某些欧洲布局）可能取不到 |
| 修饰键按标准 XKB 位判断 | 未解析 `modifier_map` 重映射 | 自定义 modifier 映射的布局下 Ctrl/Alt 判断可能失准 |
| 改词库要重新构建 | 词条与字形编译进二进制 | 外部词表可热加载，但新字要显示仍需重烘图集 |
| 英文只用一万高频词 | 词表取自公开的词频统计，不含专有名词与专业术语 | 罕见词、人名、代码标识符补不出来，按回车照原样上屏即可 |
| 英文不纠错、不预测下一词 | 刻意不做（见第六节） | 打错字不会被改正，也不会替用户补句首大写 |
| Plasma 下必须由 KWin 拉起 | 只有 KWin 交出的那条连接才被放行绑定 `zwp_input_method_v2` | 手工 `xime &` 通常绑不上；走桌面条目或 `xime-setup` |
| 简繁只有 OpenCC `s2t` 一种口径 | 未生成 `s2tw`/`s2hk` 变体表（用词差异大，如 软件/軟體） | 台湾/香港用词习惯需另加一列转换表，接口已按 `Variant` 预留 |

---

## 八、与 C 版怎么选

| | C 版 | Rust 版 |
|---|---|---|
| deb Depends | `libc6`、`libwayland-client0`、`libxkbcommon0` | **无** |
| 构建期依赖 | gcc、wayland-scanner、libwayland-dev、libxkbcommon-dev | cargo + musl target |
| 体积 | 2.7MB / 安装 6.7MB | 3.1MB / 安装 7.6MB |
| 代码量 | 约 2200 行 C | 约 4600 行 Rust（含约 900 行单元测试） |
| 37 字解码 | 9.32ms | 5.67ms |
| 移植到新架构 | 交叉编译工具链 | `rustup target add` 即可 |

要**绝对最小的依赖面**（例如不可变镜像、老旧发行版、容器内），选 Rust 版；
要**审计尽可能简单**（C 版代码更短且无 unsafe 之外的抽象），两个都可以。
