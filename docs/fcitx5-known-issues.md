# fcitx5 现存问题清单

> 环境：Arch Linux / Hyprland 0.56.2 / fcitx5 5.1.23-1、fcitx5-gtk 5.1.7-1、fcitx5-chinese-addons 5.1.15-2
> 记录日期：2026-11-19
> 范围：**与 xime 无关**，纯 fcitx5 自身的行为问题。xime-rs 的 DBus 前端问题见 `xime-rs/docs/fcitx5-dbus-frontend.md`。

---

## 摘要

| # | 问题 | 触发场景 | 影响面 | 状态 |
|---|---|---|---|---|
| 1 | 空预编辑被 Wayland 前端丢弃，客户端残留「幽灵 composition」 | 切换中英、退格把预编辑退空 | 所有走 `text-input-v3` 的客户端（Electron/WorkBuddy、Qt、GTK4） | 已止血（关行内预编辑）；根治需打补丁 |
| 2 | 行内预编辑光标被钉在最前面 | 拼音组字过程中 | 走 DBus 的 GTK3/Qt 客户端 | 已修（改 pinyin 配置） |
| 3 | 切换输入法时提交的是原始拼音而非汉字 | 组字未上屏时切换 | 全部客户端 | 未改（行为偏好） |
| 4 | 退出时用内存配置重写 addon 配置 | 运行中手改 `conf/*.conf` | 配置管理 | 已知，绕开方式明确 |

---

## 一、空预编辑被丢弃，导致「第一个字符删不掉 / 光标跳到行首」

### 1.1 症状

在任意应用中出现，WorkBuddy 输入框、微信、浏览器都可能命中：

- 拼音组字中切换中英文，**之后再按 BackSpace 删不掉刚提交的第一个字符**；
- 光标表现为停在预编辑文本的最前面（`|ni hao`）；
- 有时退格会把光标位置算错，删到不该删的地方。

关键特征：**在「切换输入法」和「退格」这两个动作之后最容易出现**——这两件事都会让预编辑变空。

### 1.2 根因

`fcitx5 5.1.23` 的 `src/frontend/waylandim/waylandimserverv2.cpp`（约 694–700 行）：

```cpp
// Validate not empty and within wayland limit.
if (preedit.textLength() > 0 &&
    preedit.textLength() < WaylandIMServerBase::safeStringLimit) {
    if (cursorStart < 0) {
        cursorStart = cursorEnd = preedit.textLength();
    }
    ic_->setPreeditString(...);
}
ic_->commit(serial_);
```

`textLength() > 0` 意味着**预编辑为空时整条 `set_preedit_string` 不发**。

但 Wayland 客户端（Chromium/Electron、Qt、GTK4）正是靠「收到一条空预编辑」来结束 composition 的。这条更新被丢弃后：

1. 客户端仍认为自己处于 composition 状态（幽灵预编辑）；
2. 第一个 BackSpace 被这份残留的 composition 消费掉，不再往下传；
3. 客户端按残留预编辑的长度定位光标，表现为停在最前面。

### 1.3 证据

| 证据 | 等级 | 说明 |
|---|---|---|
| 源码判断 `textLength() > 0` | 确证 | 本机 5.1.23 与上游 master 均为此写法 |
| 第三方补丁 `fix_waylandimserverv2_bug.patch` | 佐证 | 补丁标题即「修复 Electron Wayland 无法删除第一个预编辑字母」，改动就是把 `> 0` 改成 `>= 0` |
| 本机端到端复现 | **未完成** | WorkBuddy 窗口在滚动布局中且拒绝程序化聚焦，无法注入真实按键；GTK3/GTK4 探针未复现该症状 |

> 诚实结论：根因链条在源码层面成立且有上游补丁佐证，但本机没有拿到「用户实际应用里首字删不掉」的直接抓包。GTK3 探针走的是 DBus 通道，不经过这个函数，所以之前的测试全是阴性——这是排查中最容易走偏的地方。

### 1.4 为什么 GTK3 探针测不出来

| 客户端 | 输入通道 | 是否经过 `waylandimserverv2.cpp` |
|---|---|---|
| GTK3 + `GTK_IM_MODULE=fcitx` | fcitx5-gtk 的 **DBus** 前端 | ❌ |
| 微信（自带 fcitx5-qt） | **DBus** 前端 | ❌ |
| GTK4 应用 | Wayland `text-input-v3` | ✅ |
| Chromium/Electron（Wayland 原生） | Wayland `text-input-v3` | ✅ |

所以「GTK3 里怎么试都正常」不能推翻这个结论，必须测 GTK4 或原生 Wayland 客户端。

### 1.5 当前处置（本机已生效）

关掉「行内预编辑」，让 fcitx5 用自己的浮窗画拼音和候选，客户端不再持有 composition，也就碰不到这条被丢弃的更新：

```ini
# ~/.config/fcitx5/config
[Behavior]
PreeditEnabledByDefault=False
```

验证方式与结果：

```bash
dbus-monitor --session | grep -c UpdateFormattedPreedit   # 改动后为 0
```

客户端只剩 `UpdateClientSideUI`（候选窗自绘），不再收到任何 `UpdateFormattedPreedit`。

**代价**：拼音不再显示在输入框内的下划线区域，改由候选框上方显示。

**回退**：

```bash
systemctl --user stop omarchy-fcitx5
sed -i '/PreeditEnabledByDefault/d' ~/.config/fcitx5/config
systemctl --user start omarchy-fcitx5
```

### 1.6 根治方案（需 root，未执行）

补丁与脚本已备好：`fcitx5-preedit-fix/`。

```bash
cd ~/lab/x-input/fcitx5-preedit-fix && ./build.sh          # 编译并只替换 libwaylandim.so
cd ~/lab/x-input/fcitx5-preedit-fix && ./build.sh --revert # 还原系统原版
```

打上补丁后可以删掉 `PreeditEnabledByDefault=False`，恢复行内预编辑。脚本要求 sudo 与 cmake/ninja/extra-cmake-modules，本机当前缺构建依赖且无免密 sudo，故未执行。

---

## 二、行内预编辑的光标被钉在最前面

### 2.1 症状

拼音组字时，输入框里的 `ni hao` 光标停在最左（`|ni hao`），整串带蓝色高亮；退格位置因此错位。

### 2.2 根因

拼音插件默认 **`PreeditCursorPositionAtBeginning=True`**（Linux 上默认开启，源码注释：「Fix embedded preedit cursor at the beginning of the preedit」）。

`im/pinyin/pinyin.cpp` 在默认的 `PreeditMode=ComposingPinyin` 分支下：

```cpp
clientPreedit.setCursor(0);
```

这个 `0` 被 dbus 前端原样发给客户端，而 fcitx5-gtk 的 `gtk3/fcitximcontext.cpp` 把它当**字节偏移**：

```c
context->cursor_pos = g_utf8_pointer_to_offset(str, str + cursor_pos);
```

于是光标落在预编辑的第 0 个字符处。

### 2.3 证据

抓 `UpdateFormattedPreedit(preedit, cursorpos)` 的第三参：

| 配置 | `n` | `ni` | `nih` | `niha` | `nihao` |
|---|---|---|---|---|---|
| 默认（True） | 0 | 0 | 0 | 0 | 0 |
| 改为 False | 1 | 2 | 3 | 4 | 6 |

截图对照同样明确：默认是「整串蓝色高亮 + 光标在最左」，改后是「普通下划线 + 光标在末尾」。

### 2.4 处置（已生效）

```ini
# ~/.config/fcitx5/conf/pinyin.conf
PreeditCursorPositionAtBeginning=False
```

重启 fcitx5 后确认配置持久、omarchy 的 `configure.py` 只重写 `FirstRun`，不会覆盖这一项。

### 2.5 需要注意

这个 `setCursor(0)` 是**有意为之**的历史权衡——避免候选窗跟着光标左右跳动。关掉后候选窗位置会跟随光标，视觉上更「活」，如果觉得晃眼可以改回 True 并接受光标钉在最前。

---

## 三、切换输入法时提交的是原始拼音

`SwitchInputMethodBehavior` 默认值是 `"Commit current preedit"`，即组字到一半按切换键，落下的是 `nihao` 而不是`你好`。

| 取值 | 行为 |
|---|---|
| `"Commit current preedit"`（默认） | 提交原始拼音串 |
| `"Commit default candidate"` | 提交首选汉字 |
| `"Do nothing"` | 丢弃组字 |

配置位置：`~/.config/fcitx5/conf/pinyin.conf` 的 `SwitchInputMethodBehavior`。

本机**保持默认未改**：提交拼音在多数场景下更安全（不会替用户猜字），属于行为偏好而非缺陷。需要改的话在 fcitx5 配置界面「拼音 → 切换输入法时」调整。

---

## 四、配置管理上的两个坑

### 4.1 退出时用内存配置重写文件

fcitx5 **停止时会用内存中的配置重写 `~/.config/fcitx5/conf/*.conf`**。运行中直接编辑这些文件，改动会在下次退出时被覆盖（默认值甚至会被整行删掉）。

正确顺序：

```bash
systemctl --user stop omarchy-fcitx5
# 此时再改 conf/*.conf
systemctl --user start omarchy-fcitx5
```

### 4.2 `fcitx5-remote -r` 不重载 addon 配置

`-r` 只重新加载全局配置与输入法列表，**不会重载拼音这类 addon 的 conf**。改完 `pinyin.conf` 必须重启 fcitx5。

> 这两个坑叠加的效果是：改完配置、`-r` 一下、再抓包，读数完全没变——非常容易被误判成「这个配置项没用」。

---

## 五、与环境交互的问题（非 fcitx 缺陷）

| 现象 | 原因 | 绕开方式 |
|---|---|---|
| `Ctrl+Space` 切换中英在应用里无效 | omarchy 把它绑成 Hyprland 全局快捷键，应用收不到该按键 | 用 `Shift` 单击切中英，或 `SUPER+I`（`omarchy-input-method cycle`） |
| 用 `hyprctl eval hl.dsp.send_key_state` 注入 `mods="SUPER"` 不触发全局绑定 | Hyprland 只认真实修饰键状态 | 测试时直接执行 `omarchy-input-method cycle` |

---

## 附录 A：复现与取证方法

### A.1 三条通道的适用性

| 工具 | 通道 | 能测什么 | 局限 |
|---|---|---|---|
| `xime-rs/tools/gtk3-probe.py` | DBus（`GTK_IM_MODULE=fcitx`） | preedit 文本、cursorpos、提交时序 | 测不到 Wayland 前端的问题 |
| `/tmp/gtk4-probe.py`（临时脚本） | Wayland `text-input-v3` | 与 Electron/Qt 同一条链路 | 需 GTK4，日志只能靠窗口内文本 |
| Chromium `--enable-wayland-ime` | Wayland `text-input-v3` | 理论上最贴近 Electron | 本机实测未接上 IME，打字是裸字符，**不能当探针** |

### A.2 按键注入

| 手段 | 可用性 |
|---|---|
| `tools/uinput-inject`（`/dev/uinput`） | ❌ 本机桌面会话登录早于 `input` 组生效，会话进程无该组，全部 Permission denied（需重新登录恢复） |
| `wtype` | ⚠️ 只能打文本；`-k BackSpace` 静默无效（稀疏 keymap），不能用于测退格 |
| `hyprctl eval 'hl.dispatch(hl.dsp.send_key_state({mods="",key="BackSpace",state="down"}))'` | ✅ 普通键与退格都有效，本机首选 |

### A.3 抓包

```bash
dbus-monitor --session > /tmp/trace.dbus
# 关注：ProcessKeyEvent / UpdateFormattedPreedit / CommitString / UpdateClientSideUI
```

Wayland `text-input-v3` 通道**不经过 DBus**，抓不到；这条链路只能靠客户端侧读数或源码推理。

---

## 附录 B：本机当前状态

| 项 | 值 |
|---|---|
| fcitx5 | 5.1.23-1（Arch 官方包，未打补丁） |
| `~/.config/fcitx5/config` | `[Behavior] PreeditEnabledByDefault=False` |
| `~/.config/fcitx5/conf/pinyin.conf` | `PreeditCursorPositionAtBeginning=False` |
| 服务 | `omarchy-fcitx5` active，`xime-dbus` inactive |
| 备份 | `~/.config/fcitx5/conf/pinyin.conf.bak-*` |

## 参考

- `fcitx/fcitx5` `src/frontend/waylandim/waylandimserverv2.cpp`
- `fcitx/fcitx5-chinese-addons` `im/pinyin/pinyin.cpp`、`im/pinyin/pinyin.h`
- `fcitx/fcitx5-gtk` `gtk3/fcitximcontext.cpp`
- 第三方补丁 `fix_waylandimserverv2_bug.patch`（`busyoGG/librime-patch`）
- `fcitx5-preedit-fix/0001-waylandim-allow-empty-preedit.patch`（本仓库）
