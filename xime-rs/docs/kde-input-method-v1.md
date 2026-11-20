# KDE（KWin 6.6）：zwp_input_method_v1 接入说明

xime 原本只有 `zwp_input_method_v2` 前端。KWin 6.6 **只实现 v1**，所以 KDE 下
必须有第二个前端。本文记录 v1 前端的实现要点、KWin 侧的协议差异、以及实测踩到的
几处「看起来像输入法的 bug，其实是环境或工具」的坑。

代码入口：`src/proto.rs` 末尾的 v1 请求编码，`src/main.rs` 的 `on_im1` / `on_ctx1` /
`on_kb1` 与 `Frontend` 枚举。运行时可强制：`xime --frontend auto|v1|v2`
（默认 auto：registry 里出现 v1 就切 v1，出现 v2 就用 v2）。

---

## 一、两套协议不是「版本号不同」，是模型不同

| | v2（Hyprland / sway / labwc） | v1（KWin 6.6） |
|---|---|---|
| 怎么拿到输入法 | `zwp_input_method_manager_v2.get_input_method(seat)` | registry 里**直接就是** `zwp_input_method_v1`，只在合成器拉起的连接上出现 |
| 激活 | `activate` 事件 | `activate(id)` 事件，id 由合成器分配（`0xff000000` 起） |
| 键盘 | `grab_keyboard(new_id)` → 抓取对象自定义事件（keymap/key/modifiers，opcode 0/1/2） | `ctx.grab_keyboard(new_id wl_keyboard)` → 拿到**标准 wl_keyboard**（opcode 0 keymap / 3 key / 4 modifiers） |
| 不消费的键 | `zwp_virtual_keyboard_v1` 回注，且要先把 keymap 发给虚拟键盘 | `ctx.key(serial,time,key,state)` 原样转发，**不需要虚拟键盘、不需要同步 keymap** |
| 候选窗 | `get_input_popup_surface`，位置自己算 | `zwp_input_panel_v1.get_input_panel_surface` + `set_overlay_panel`，**合成器贴着光标摆** |
| serial | `done` 事件累加 | `commit_state(serial)`；`commit_string` / `preedit_string` / `key` / `modifiers` 都要带 |

一句话：v2 是「输入法自己当键盘和浮层的主人」，v1 是「输入法借用合成器的键盘和浮层」。
所以 v1 路径下 `forward_key` 走 `ctx.key`，候选窗坐标不用算。

---

## 二、KWin 的启动链：三个条件缺一不可

1. **桌面条目必须带 `X-KDE-Wayland-VirtualKeyboard=true`**
   KDE 的「系统设置 → 键盘 → 虚拟键盘」列表就是扫这个字段。

2. **`kwinrc` 的 `[Wayland] InputMethod` 必须是 `.desktop` 的路径，不能是可执行文件**
   实测（KWin 6.6.6）：

   ```sh
   # 写成桌面条目：立刻生效
   kwriteconfig6 --file kwinrc --group Wayland --key InputMethod \
       /usr/share/applications/org.fcitx.Fcitx5.desktop --notify
   # → fcitx5 被拉起（pgrep 有进程）

   # 写成裸二进制：静默什么都不发生，日志里连报错都没有
   kwriteconfig6 --file kwinrc --group Wayland --key InputMethod \
       /home/skyitachi/lab/x-input/xime-rs/target/release/xime --notify
   # → 没有任何新进程
   ```

   因此 `xime-setup` 在找不到 `/usr/share/applications/xime.desktop` 时，会在
   `~/.local/share/applications/xime.desktop` 生成一份等价条目（`Exec` 直接写二进制，
   **不要带 `--dbus`**）。

3. **`VirtualKeyboardMode` 不能是 0**
   `0` 表示完全禁用，KWin 根本不会拉起输入法。`xime-setup` 只在它是 `0` 或空时
   改成 `2`，其余值尊重用户设置。

满足这三条后，KWin 会 fork 出 xime，并把一条**专用连接**交给它（环境里能看到
`WAYLAND_SOCKET=<fd>`，父进程是 `kwin_wayland`）。只有这条连接上才有
`zwp_input_method_v1`（registry name 35）和 `zwp_input_panel_v1`（name 59）——
普通客户端去 registry 里翻是翻不到的，这就是「为什么非得让 KWin 拉起」。

```
$ xime --verbose          # KWin 拉起后
activate（v1, ctx=0xff000000）
已绑定 zwp_input_panel_v1
候选窗 surface 已创建
已抓取键盘（v1）
keymap 已解析（454 键）
```

### 协议前端是自动选的，但「自动」只覆盖协议这一层

`--frontend auto`（默认）在注册表扫描时按**谁出现**来定，两套协议不同时存在：

| 合成器 | 连接上看到的 | 自动选成 | 还需要什么 |
|---|---|---|---|
| Hyprland / sway / labwc | `zwp_input_method_manager_v2` | v2 | 无（普通客户端，config 里 `exec-once` 即可） |
| KDE Plasma（KWin 6.6） | `zwp_input_method_v1` + `zwp_input_panel_v1` | v1 | **必须由 KWin 拉起** + 撤掉 `QT_IM_MODULE` |

也就是说：**协议这一层是自动的，启动方式不是**。KWin 只在它自己 fork 出来的那条
`WAYLAND_SOCKET` 连接上放 v1，手工跑 `xime &` 在 KDE 上永远是普通客户端（拿不到席位，
现在会直接报错退出，见第七节第 9 条）。

扫描结束的判定点用 `wl_callback` 兜住（registry 的 global 流没有结束信号），确定自己
既没拿到 v2 manager、也没有 v1 对象时才认输退出——所以「被 KWin 拉起但真的没席位」
和「自己乱跑」两种失败都能被区分出来。

想把选择钉死时用 `--frontend v2` / `--frontend v1`（`kwin` 也认）。日常不需要。

---

## 三、两个会让连接当场断掉的协议签名坑

这两处 KWin 编译进去的协议和 `wayland-protocols` 里的 XML **不一致**，且错法一样：
参数不匹配时 KWin 直接判 `invalid arguments` 或 `NULL string`，客户端随后被踢下线，
表现是「输入法启动一下就没了」。

### 1. `grab_keyboard` 是 v1 里唯一不带 serial 的请求

```
zwp_input_method_context_v1.grab_keyboard(new_id keyboard)     # 只有一个参数
```

多塞一个 serial 会变成：

```
kwin_wayland: invalid arguments for zwp_input_method_context_v1#4278190080.grab_keyboard
kwin_wayland: not a valid new object id (93), message grab_keyboard(n)
```

因为 v1 其他请求（`commit_string` / `preedit_string` / `key` / `modifiers` /
`commit_state`）全都带 serial，很容易顺手也加一个。别加。

### 2. `preedit_string` 的第三个参数是 `commit`（字符串），不是 `cursor`（int）

```xml
<!-- 标准 XML -->          <request name="preedit_string">
                                <arg name="serial" type="uint"/>
                                <arg name="text" type="string"/>
                                <arg name="cursor" type="int"/>       ← 标准版
<!-- KWin 编译进去的 -->        <arg name="commit" type="string"/>    ← 实际签名 uss
```

按标准发 `int 0` 时，KWin 把它当字符串读到空指针：

```
kwin_wayland: NULL string received on non-nullable type, message preedit_string(uss)
```

（在 `libkwin.so.6.6.6` 的协议表区域可以同时看到 `preedit_string` 和相邻的 `uss`。）
xime 的 `proto::ctx_preedit` 因此把第三参定义成 `commit: &str`，v1 路径固定传 `""`。

---

## 四、环境变量：这一条最容易白折腾一小时

只要会话里存在下面任意一个，**Qt/GTK 应用就不会创建 `zwp_text_input_v3`**：

```sh
QT_IM_MODULE=fcitx
GTK_IM_MODULE=fcitx
XMODIFIERS=@im=fcitx
```

它们通常来自 `~/.config/environment.d/99-fcitx5-im.conf`（im-config 写的）或
`~/.config/plasma-workspace/env/*.sh`。后果是：

- 应用不建 text-input → KWin 不为它建 `zwp_input_method_context_v1` → 没有 `activate`；
- 按键直通进应用，xime 一行日志都没有；
- 用户看到的现象却是「候选窗能弹（在别的窗口）、这个应用打字是英文字母」。

实测对照（同一台机器、同一次会话，两个 Kate）：

| 启动方式 | xime 日志 | 应用里 |
|---|---|---|
| `kate`（无 IM 模块变量） | 有 `activate`、候选窗、`commit` | 正常上屏「你好」 |
| `QT_IM_MODULE=fcitx kate` | **一行都没有** | 文档里直接出现 `nihao` |

处理：`xime-setup` 的 `kde_env_fix()` 会把这几行**整行注释掉**并保留
`*.xime-backup`，同时提示：

> **需要重新登录一次**：环境变量只在登录时导入，改完当次会话不生效。

注意这是**双向**的：撤掉这些变量后，xime 和 fcitx5 在 KDE 上都走 KWin 的
text-input 通道，都正常；但依赖 XIM / fcitx 客户端模块的 XWayland 应用会失去输入法
（见下一节）。

---

## 五、已知局限：XWayland 应用拿不到输入法

KWin 只为 **`wl_surface`** 建输入法上下文，X11 窗口没有 wl_surface，所以
`zwp_input_method_v1` 覆盖不到 XWayland。实测：

```sh
QT_QPA_PLATFORM=xcb kate --new /tmp/kde-x11.txt    # 用 XWayland 跑 Kate
# → xime 日志无 activate、无按键；文档里直接出现 "NIHAO ZHCN"（按键直通）
```

fcitx5 这块是靠 **XIM 服务**（`XMODIFIERS=@im=fcitx` → X 协议层）补的，xime 没有
XIM 前端。所以微信（Electron/XWayland）、WPS 这类应用在 xime 原生前端下打不了中文。

两条出路，二选一：

1. **`xime --dbus`**：抢 `org.fcitx.Fcitx5` 服务名，走 fcitx 的 DBus 客户端模块。
   这条路要求**保留** `QT_IM_MODULE=fcitx`（与第四节相反），且它不跑 Wayland 事件循环，
   与原生前端互斥（单实例锁）。
2. **让应用走 Wayland**：Electron 加 `--ozone-platform=wayland`，
   其它应用找找有没有 Wayland 后端开关。

---

## 六、实测清单（KDE Plasma 6.6.6 / KWin 6.6.6 / Wayland / 2560×1440）

在 Kate 里逐项验证，全部通过：

| 项目 | 结果 |
|---|---|
| 候选窗贴光标弹出 | ✅ overlay panel，右下角对齐文本光标 |
| 拼音组字（内联 preedit + 下划线） | ✅ |
| 空格上屏 | ✅ `commit "你好" (serial=151)` |
| 数字选词 | ✅ `nihao2` → `commit "你号"` |
| 全角标点 | ✅ `,` → `commit "，"` |
| 退格改拼音 | ✅ |
| Esc 取消组字 | ✅ 候选窗与 preedit 同时消失，不上屏 |
| 不消费的键回注 | ✅ `ctx 回注 key=14 state=1`（空缓冲时退格直达应用） |
| 修饰键状态回传 | ✅ `ctx.modifiers(...)` |
| keymap 解析 | ✅ 454 键 |

组字过程中的一个正常现象，不要当成「双份输入」：拼音是**内联 preedit**（应用自己
渲染、带下划线），候选窗是**另一个 surface**。两者本来就同时在屏幕上，Hyprland 上
一模一样。判断是否为真·双份输入，看 `--verbose` 日志里的 `commit` 次数与
`回注` 次数即可：

```sh
grep -c 回注 /tmp/xime.log     # 只在「不消费的键」上才出现
```

---

## 七、排查手法（这次踩过的坑都在这里）

**1. KWin 的协议错误只在 journal 里**

```sh
journalctl --user -b -o cat | grep -iE "kwin_wayland.*(invalid|NULL|input method)"
```

客户端被踢下线时，xime 侧只看到「连接断了」，真因在合成器。

**2. 看按键到底走到哪**

```sh
XIME_TRACE=1 xime --verbose     # 每帧打印 obj + opcode，确认是「没收到键」还是「收到没处理」
```

**3. KWin 脚本置焦：`loadScript` 之后必须 `start()`**

漏了 `start()` 的现象是「拿到脚本 id，但什么都没发生」：

```sh
qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript /path/focus.js xime-focus
qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.start      # ← 缺这行就白忙
journalctl --user -q --since "10 seconds ago" | grep xime-test   # console.error 落在 journal
```

**4. uinput 注入必须先声明键码，否则内核静默丢弃**

`tools/uinput-inject` 早先只按字符表 `UI_SET_KEYBIT`，结果 Esc、Tab、方向键、
Delete、F 键、CapsLock、小键盘回车**根本没进系统**——内核在
`input_get_disposition()` 里对 `EV_KEY` 先做 `test_bit(code, dev->keybit)`，
没声明的键码直接 `INPUT_IGNORE_EVENT`，不报错、不进 libinput。

一度因此误判成「KWin 只把字符键转发给输入法」，直到改用 `key <keycode>` 子命令并
声明 `1..256` 全部键码才推翻：

```sh
uinput-inject key 1      # Esc
uinput-inject key 105    # Left
uinput-inject key 14 200 # Backspace，按住 200ms
```

**5. 判断「是不是输入法的问题」，看应用里有没有出现原始字母**

字母直接进了应用 = 按键直通 = 没有输入法上下文（不是 xime 处理错）。

**6. KWin 认不认「这个应用要输入法」：查 `/VirtualKeyboard` 的 D-Bus 属性**

比猜日志快得多，一条命令就能定性：

```sh
for p in active activeClientSupportsTextInput available enabled; do
  printf '%-30s = %s\n' "$p" \
    "$(qdbus6 org.kde.KWin /VirtualKeyboard \
        org.freedesktop.DBus.Properties.Get org.kde.kwin.VirtualKeyboard $p)"
done
```

判读：

| 组合 | 含义 |
|---|---|
| `activeClientSupportsTextInput=true` 但 `active=false` | 应用支持 text-input，KWin 却没激活 → 输入法侧问题（没绑上 / 被锁顶掉） |
| `activeClientSupportsTextInput=false` | **当前聚焦的应用根本没建 text-input** → 十有八九是 `QT_IM_MODULE`（第四节），不是 xime |
| `available=false` | KWin 没读到 `X-KDE-Wayland-VirtualKeyboard=true` 的条目 |

`false` 这一格是本次排查里最容易误判的地方：候选窗在别的窗口能弹、这个窗口打字出字母，
看着像「输入法坏了」，实际是应用压根没进输入法通道。

**7. `--verbose` 不能直接写进 desktop 条目的 `Exec`**

KWin **不读**子进程的 stdout。`--verbose` 的 `println!` 把 64KB 管道填满后进程阻塞在
`write()`：表现是「进程在、但一条日志都没有、也不响应任何按键」，看着像协议挂了。

排查时套一层重定向，不要指望 journal：

```sh
#!/bin/sh
exec /home/skyitachi/.local/bin/xime --verbose >/tmp/xime-kde.log 2>&1
```

（`eprintln!` 那几行走 stderr，会被 `kwin_wayland_wrapper` 转进 journal，所以 journal 里
永远只看得到「候选窗使用 Slint 软件渲染」这一行——这是正常的，不是日志丢了。）

**8. 切换输入法之后，先切一次焦点**

KWin 的 `activate` 由**焦点变化**驱动，不会在输入法重载的瞬间补发。所以「切到 xime →
直接打字」有时一个字都不进，切到另一个窗口再切回来就全好了。自动化测试里固定按
「置焦 → 打字」的顺序做，别在切换后立刻敲键。

**9. 拿不到席位就立刻退出，别占着单实例锁**

KWin 只把 `zwp_input_method_v1` 交给**它自己拉起**的进程。手工 `xime &` 在 KDE 上永远
绑不上——如果这时它还安静地占着 `$XDG_RUNTIME_DIR/xime.lock`，KWin 随后拉起的那份会
因为锁被占而退出，用户看到的是「输入法完全不工作且毫无提示」。

`xime` 现在会在注册表扫描结束（`wl_callback`）时判定：既没有 v2 manager、也没有 v1 对象
→ 打印原因并以退出码 1 结束，把位置让出来。

---

## 八、与 fcitx5 共存

`xime-switch` 在 KDE 上的做法：

- 切换 = 改 `kwinrc` 的 `InputMethod` 并 `--notify`；
- KWin 6.6 对单次变更**常常不重载**，所以走两段式：先切到对端，`sleep 2`，再切回目标，
  强制它卸载旧输入法；
- 切完轮询进程（最多 10 秒）确认新输入法真的起来了，再返回。

两边的 `.desktop` 都不带 `--dbus`，`Exec` 直接是二进制路径。
