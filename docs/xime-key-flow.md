# 从按键到汉字：xime 全链路与 fcitx5 对比

> 目标读者：想弄清输入法内部到底发生了什么的人。
> 依据：`xime-rs` 源码（章节内标注文件与行号）、fcitx5 5.1.23 源码、本机 Hyprland 实测。
> 日期：2026-11-19。

---

## 一、定位：它不是插件，是协议的一端

多数人对输入法的印象是「装在系统里的一个服务，应用把按键发给它」。Wayland 下不是这样。

xime 是 `zwp_input_method_v2` 这个协议的**一端**，应用是 `zwp_text_input_v3` 的另一端，两者从不直接对话，全部由合成器转接：

```
应用（GTK / Qt / Electron）
   ↕  zwp_text_input_v3：提交文本、预编辑、光标矩形
合成器（Hyprland / sway / KWin）
   ↕  zwp_input_method_v2：按键、keymap、popup surface
xime（wire / keymap / 引擎 / 渲染）
   ↕  zwp_virtual_keyboard_v1：把不消费的按键还回去
```

这个结构带来两个直接后果：

1. **不需要往任何进程里注入东西**——不用 `GTK_IM_MODULE`，不用改应用，也不依赖某个工具包。
2. **按键会先到输入法**，输入法必须对每个键做出「吃掉 / 还回去」的决定，这一步做错就是 `Ctrl+C` 失效或修饰键卡死。

代价是覆盖面被协议锁死：合成器没实现 `zwp_input_method_v2`（GNOME/Mutter），或者应用根本不在 Wayland 上（XWayland、X11 会话），这条路就走不通。这也是后来加 `--dbus` 前端的原因（见 4.4）。

---

## 二、启动期：四步拿到键盘

1. **连上 Wayland socket**，自己实现 wire 协议（`src/wire.rs`）。消息头是 `u32 object_id` + `u32 (长度 << 16 | opcode)`，字符串带 NUL 与 4 字节对齐，fd 不占消息体、走 `SCM_RIGHTS` 附属数据。
2. **在 registry 里找全局对象**：`zwp_input_method_manager_v2`、`zwp_virtual_keyboard_manager_v1`、`wl_seat`、`wl_compositor`、`wl_shm`。缺 input-method 管理器就直接报「席位被占用」退出。
3. **`get_input_method(seat)`**（`proto.rs:111`），等 `activate` 事件。收到后立刻 `grab_keyboard`（`proto.rs:137`）——注意这不是「订阅按键」，而是**键盘所有权的转移**。
4. **解析 keymap**。合成器通过 `grab.keymap` 送来约 35 KB 的 XKB 文本，xime 只解析两段：
   - `xkb_keycodes`：`<AD01> = 24`，名字到 keycode；
   - `xkb_symbols`：`key <AD01> { [ 0x71, 0x51 ] }`，keycode 到前两列 keysym。

   输入法真正要回答的只有一个问题：**这个键在当前 shift/caps 下产生哪个符号**。所以不需要完整实现 XKB，也不需要 libxkbcommon。解析失败时回退内置 US 布局；如果只是缺 `xkb_symbols` 的残缺 keymap，则沿用上一份能用的，避免非 US 布局全盘错乱。

同一份 keymap 会原样交给虚拟键盘（`vk_send_keymap`），否则回注的按键没有解释依据。

---

## 三、运行期：一个按键的六步

```
① 合成器送来 key 事件        code=38, state=1（按下）
② wire 层解出参数            定长头 + opcode，fd 走 SCM_RIGHTS
③ keycode → keysym          keymap.sym(38, shift) → 'a'
④ on_key 路由               Ctrl／密码框／Shift／数字 → 否则进组字
⑤ engine 重算候选           push_char → N-best 词网格 + 二分查词
⑥ 写回客户端                preedit + commit(serial)；空格 → 提交候选
```

对应 `src/main.rs`：`on_grab`（314）收事件 → `on_key`（389）路由 → `engine.recompute`（`engine.rs:121`）→ `refresh`（734）发射协议请求。

### 3.1 第 4 步：路由的分支最容易被低估

`grab_keyboard` 之后所有按键都先到 xime，它必须逐键决定去留，并且**按 keycode 记账**（`consumed[]`）：按下时吃了，对应的抬起也要吞掉，否则应用会收到一个没有配对的抬起事件，修饰键就此卡住。

几个具体判断（`main.rs:389-560`）：

| 键 | 处理 | 原因 |
|---|---|---|
| Shift 按下 | 挂起（`shift_pending`），不立即切换 | 松开且中间没敲别的键才判定；否则想打大写 `A` 会先被切成中文 |
| Ctrl / Alt / Super + 任意键 | 一律回注 | `Ctrl+C` 不能被当成字母 `c` 吃掉 |
| 密码框（`content_type` 的 `purpose=8/9`） | 全键透传，连修饰键状态都不截留 | 同时清掉残留组字，避免半截拼音突然上屏 |
| `1`–`9` | 有候选时选对应项 | 无候选时当普通数字回注 |
| 空格 / 回车 | 有组字则提交候选 | 无组字时回注，不吞键 |
| 标点 | 先提交候选，再上全角标点 | 与中文输入习惯一致 |
| 其它（字母） | 进组字缓冲 | 触发第 5 步 |
| 其它（未识别） | 回注 | 方向键、Tab、功能键都在这里 |

### 3.2 第 5 步：引擎不是查词，是在词网格上找最优路径

`src/engine.rs`。输入串没有分隔符，切分本身就是歧义源：

- `xian` 可能是「先」，也可能是「西安」；
- `nihaom` 可能是「你/哈/欧盟」，也可能是「你好/吗」。

贪心切分（每次取最长匹配）会永久丢掉后一种解释，所以必须在**所有切分方式**上比较总代价。

做法是词网格上的前向 N-best：

1. 输入串每个字节位置是一层，`paths[i]` 存放「到达位置 i 的所有路径」，每条带累计代价。
2. 从位置 i 出发枚举所有边（`collect_edges`，`engine.rs:193`），三类来源：

| 边的类型 | 例子 | 代价 |
|---|---|---|
| 整词精确 | `nihao` → 你好 | `10 - log10(freq)` |
| 简拼 | `nh` → 你好 | 上式 `+ 2.0` |
| 尾部音节补全 | `nih` → 你好 | 上式 `+ 2.0 + 0.7 × (缺字数 - 1)` |

3. 一条边把路径推进到 `i + k`，代价相加；每个位置只保留最优的 `BEST = 27` 条（正好三页候选），同音词每个位置取 `HITS = 6` 条参与搜索。
4. 扫到串尾，按累计代价排序输出，去重后作为候选（上限 `CAND_MAX = 128`）。

代价函数的常数项取 10 而不是理论的 9，是有意的：词频是概率乘以 1e9，纯 unigram 模型没有上下文约束，如果不额外惩罚「多切一刀」，高频单字拼接（「你/号」）会压过整词（「你好」）。同理，补全惩罚必须存在，否则输入一个 `n` 时「中国」会因为词频高而排在「你」前面。

### 3.3 词库为什么能 O(1) 判断长度

`data/dict.bin` 是构建期烘焙的定长表（`tools/mkdict.py --bin`），运行时 `include_bytes!` 映射进只读段，零解析、零拷贝。词条按拼音字节序排列，同音词按词频降序连续存放。

关键技巧：key 与 value 在 blob 里紧邻存放，所以

```
key_len = val_off - key_off - 1
```

是算出来的，不是数出来的。二分定位到候选区间后，**长度不匹配立刻返回**，不必逐个字符比较。

这一条把「查单字母」从 34.25 μs 压到 0.105 μs。拼音是逐键输入的，每个位置都要查 `rem` 次，不做这个优化，开销会随输入长度平方增长——C 版就是没做，所以 37 字整句要 9.32 ms，Rust 版 5.67 ms。

### 3.4 第 6 步：两条出口

| 场景 | 协议请求 | 效果 |
|---|---|---|
| 组字过程中 | `set_preedit_string(text, begin, end)` + `commit(serial)`（`proto.rs:122/128`） | 应用显示带下划线的拼音，光标位置由 `begin/end` 指明 |
| 空格 / 数字选词 | `commit_string(text)`（`proto.rs:117`） | 汉字真正进入应用，随后清空预编辑 |

`commit(serial)` 的 serial 必须等于已收到的 `done` 事件计数。写错不会报错，只会被严格实现的合成器**静默丢弃**——预编辑不更新，表现为「打字没反应」。

降级路径：合成器不支持 popup surface 或终端字体更可预测时，`--inline-candidates` 把拼音和编号候选一起塞进预编辑，由应用自己的字体渲染。

### 3.5 候选窗是自己画的

`main.rs:793` 起：

1. `create_surface` + **`get_input_popup`**（`proto.rs:132`）拿到 `zwp_input_popup_surface_v2` 角色，位置由合成器贴合光标决定；
2. 把 surface 的 input region 设为空区域，鼠标事件穿透；
3. `memfd` + `mmap` 提供 ARGB8888 像素内存，`wl_shm_pool` 包成 buffer；
4. 字形来自构建期烘焙的 8bpp 图集（`data/atlas.bin`），软光栅逐像素叠加 alpha（`render.rs`），不碰 fontconfig/freetype，也不携带 TTF；
5. 尺寸变化才重建 buffer，内容变化只重绘并 `attach + damage + commit`。

---

## 四、与 fcitx5 对比

### 4.1 层数差异

```
xime                                  fcitx5
────────────────────────────          ────────────────────────────
应用（text-input-v3）                  应用（需 IM_MODULE 或 XIM）
   ↓                                     ↓
合成器转接（input-method-v2）          客户端模块（fcitx5-gtk / fcitx5-qt → DBus）
   ↓                                     ↓
xime 单进程                            fcitx5 守护进程（addon 调度）
   ↓                                     ↓
内置数据（词条 + 字形图集）             引擎与 UI addon（libime / ClassicUI / …）
```

一句话：**fcitx5 用多一层换覆盖更广，xime 用少一层换零依赖和可预测**。

### 4.2 逐项对照

| 维度 | xime | fcitx5 |
|---|---|---|
| 形态 | 单个静态二进制（musl，`ldd` 显示 statically linked） | 守护进程 + 一组 addon + 客户端 IM 模块 |
| 怎么拿到按键 | 直接 bind `zwp_input_method_v2` 并 `grab_keyboard` | `waylandim` addon 也这么做；也可完全不碰协议，走 DBus 收客户端模块送来的键 |
| 应用侧要做什么 | 什么都不用（合成器转接 `text-input-v3`） | 要 `GTK_IM_MODULE` / `QT_IM_MODULE` / `XMODIFIERS`，或走 XIM |
| 候选窗 | 自己画：popup surface + 共享内存 + 8bpp 字形图集 + 软光栅 | ClassicUI 自绘；也可声明 `ClientSideInputPanel` 让客户端画 |
| 引擎 | 自研 N-best 词网格，33.3 万词条，打分 = 词频 + 三阶语言模型（modified KN，ARPA 回退链） | libime：词图 + 三阶语言模型（kenlm 二进制）+ 用户词频学习 + 云拼音 |
| 覆盖 | Hyprland / sway / KWin 原生；GNOME、XWayland 要加 `--dbus` | 全覆盖（X11、GNOME、XWayland 都能用） |
| 扩展 | 拼音全拼、简拼、英文前缀补全、简繁 | 双拼、五笔、仓颉、注音、emoji、手写、皮肤…… |
| 配置 | 命令行参数 + 环境变量 | addon conf + 图形配置工具 |
| 解码耗时 | 37 字整句逐键重算 17.0 ms（其中约 2.7 ms 花在三阶模型查表） | 未做同条件测量，不作比较 |

### 4.3 一个实测细节：fcitx5 两条路都修了

同一个桌面里：

- GTK3 应用（`GTK_IM_MODULE=fcitx`）走 im-fcitx5 的 **DBus** 通道，按键走 `ProcessKeyEvent`；
- GTK4 应用走 **`text-input-v3`**，由 fcitx5 的 `waylandim` addon 接。

这解释了一个排查中常见的错觉：用 GTK3 探针测 fcitx5 的行为，测不到 Wayland 那条链路上的问题（例如 `waylandimserverv2.cpp` 丢弃空预编辑导致的 composition 残留，见 `docs/fcitx5-known-issues.md`）。判定一个客户端走哪条路的可靠办法：看它聚焦时会不会在会话总线上创建 `inputcontext/` 对象——会，就是 DBus 路径；不会，就是 Wayland 路径。

### 4.4 `--dbus`：补短板的那条腿

xime 的 DBus 前端不碰 Wayland 协议，而是抢下 `org.fcitx.Fcitx5` 与 `org.fcitx.Fcitx-0` 两个服务名，让应用**已经编译进去的** fcitx5-qt / fcitx5-gtk 客户端连上来。

| 场景 | Wayland 前端 | `--dbus` 前端 |
|---|---|---|
| Hyprland / sway / KDE | ✅ | ✅ |
| GNOME Wayland | ❌ Mutter 无该协议 | ✅ GTK / Qt 应用可用 |
| X11 会话 / XWayland 应用 | ❌ | ✅ |
| 微信 / WPS / Electron | ❌ | ✅ 已实测 |

代价与前提：

- 与 fcitx5 互斥（服务名唯一），必须先停掉 fcitx5；
- 系统里要有 `fcitx5-qt` / `fcitx5-gtk` 模块（只需模块，不需要跑守护进程）；
- 必须实现 `/controller` 上的 `Controller1`，否则桌面顶栏指示器会每 500 ms 轮询失败并退出，用户看到的是「装了它别的应用输入法坏了」。

反直觉的收益：走 DBus 时 Qt 客户端恒声明 `ClientSideInputPanel`，**候选窗由应用自己画**，xime 那套 atlas / render / shm / popup 代码一行都用不上——恰好是 Wayland 前端最费劲的那一半。

---

## 五、取舍

| | 得到 | 付出 |
|---|---|---|
| xime | 零动态依赖、零系统接线、单进程可预测的延迟、安装包没有 Depends | 覆盖面窄（GNOME/XWayland 需第二条前端）、功能少、协议与渲染全部自己写 |
| fcitx5 | 全覆盖、生态完整、引擎带语言模型与用户词频学习 | 数十个动态库与数据包、环境变量接线、多一层 IPC、行为受 addon 配置与版本影响 |

两者定位不重叠：fcitx5 是「装上就什么都能打」的通用框架；xime 是把整条链压进一个进程的极简实现。

---

## 六、关键数值与代码位置

| 项 | 值 | 位置 |
|---|---|---|
| 输入串上限 | 32 字节 | `engine.rs:14` |
| 候选上限 | 128 | `engine.rs:15` |
| 每个位置保留路径数 | 27（三页） | `engine.rs:18` |
| 同音候选参与搜索数 | 6 | `engine.rs:17` |
| 每位置最大边数 | 96 | `engine.rs:19` |
| 代价常数项 | 10.0 | `engine.rs:25` |
| 简拼惩罚 / 补全基准 | 2.0 / 2.0 + 0.7×n | `engine.rs:28-29` |
| 词条定长 | 12 字节 | `dict.rs:16` |
| DBus 前端每页候选 | 9 | `fcitx_dbus.rs:62` |

| 环节 | 函数 |
|---|---|
| 键盘抓取与 keymap | `main.rs:314` `on_grab` |
| 按键路由 | `main.rs:389` `on_key` |
| 候选重算 | `engine.rs:121` `recompute` |
| 边枚举 | `engine.rs:193` `collect_edges` |
| 预编辑/候选刷新 | `main.rs:734` `refresh` |
| 提交候选 | `main.rs:645` `commit_selection` |
| 按键回注 | `main.rs:913` `forward_key` |
| 候选窗 | `main.rs:793` `popup_create` / `main.rs:845` `popup_update` |

> 注：`README.md` 第四节写的「每位置保留 9 条路径」是 DBus 前端补齐之前的数字，代码现为 27。

## 参考

- `xime-rs/README.md`：设计取舍、性能数据、验证方式
- `xime-rs/docs/fcitx5-dbus-frontend.md`：DBus 协议细节与实测记录
- `docs/fcitx5-known-issues.md`：fcitx5 侧的已知问题
- `wayland-ime/`：同一方案的 C 版实现（对照用，2026-11 起冻结，不再加新功能）
