# xime 在应用启动器里「打不出任何字符」——键盘 grab 未释放

- 记录日期：2026-11-25
- 影响版本：`0.3.0-1` 及此前所有版本
- 严重度：高（输入法在部分客户端里完全不可用，且表现为「什么都没发生」，无任何报错）
- 状态：已定位、已修复、已用 evdev 级注入实测通过；待打包替换 `/usr/bin/xime`

## 1. 现象

在 Omarchy（Quickshell）应用启动器里输入中文：

- 搜索框**一个字符都不出现**——既没有拼音，也没有候选条，连原始英文字母都没有
- 同一台机器上，终端、Electron（WorkBuddy）、GTK 应用全部正常
- 换成 fcitx5 后同一个启动器正常，所以问题一直被归到「xime 的组字/D-Bus 逻辑」
- 关闭 xime 服务后，同一个启动器立刻能收到键盘输入

## 2. 根因

`zwp_input_method_keyboard_grab_v2` 只有 `grab_keyboard`（协议 request 5），**从来没有配对的 `release`**（request 6）。

```
compositor → activate      → xime 调 grab_keyboard，席位键盘交给输入法
compositor → deactivate    → xime 清了 self.grab = 0，但没发 release
```

Hyprland 0.56 在 `deactivate` 后**不销毁 grab 对象**，席位键盘继续把按键投进这个 grab；而 xime 此时已 reset 引擎、也不再消费这些键 → 键被吞掉，应用侧零字符、候选窗零反馈。

为什么启动器最致命：Quickshell 走 fcitx5-qt 插件（D-Bus 路径），不是 `zwp_text_input_v3` 客户端。文本输入类应用（终端/Electron）在失焦/重激活时会重建 text-input，触发 xime 重新 `activate` + `grab_keyboard`，链路能自愈；启动器只是短暂拿一次焦点，激活窗口很窄，正好落在「grab 已存在但不消费」的空洞里。

## 3. 排查中被证伪的假设（保留以防重复踩坑）

| 假设 | 证伪方式 | 结论 |
|---|---|---|
| `CreateInputContext` 参数签名 `a(ss)` vs `a{sv}` 不匹配 | 切到 fcitx5 抓包，同为 `a(ss)` | 否 |
| 客户端报 `content_purpose=13`（terminal）导致透传 | 直接构造含该值的报文，仍正常组字 | 否 |
| capability 第 32 位 `Terminal` 导致禁用 | 模拟 `0x1e001800032`，仍正常上屏 | 否 |
| `DestroyIC` 对象泄漏导致焦点错乱 | 确实是 bug（方法名被 zbus 转成 `DestroyIc`，客户端调用全部失败），但修完仍打不出字 | 是 bug，但不是本问题 |
| 候选词数组为空导致 Qt 插件不画 | 完整报文里候选齐全（你、呢、妳、泥） | 否 |

同时记下一个方法论教训：**`hl.dsp.send_shortcut` 这类合成器级注入会绕过 seat 的 keyboard grab**，用它验证输入法「能用」永远是假阳性。这正是本问题被反复误判为「已经好了」的原因。

## 4. 修复

```rust
// src/main.rs  on_im() → deactivate 分支
if self.grab != 0 {
    let _ = proto::im_grab_release(&mut self.conn, self.grab);
}
self.grab = 0;
```

```rust
// src/proto.rs
pub fn im_grab_release(c: &mut Conn, grab: u32) -> io::Result<()> {
    // zwp_input_method_keyboard_grab_v2.release（request 6），无参数
    c.send(grab, 6, Msg::new())
}
```

要点：`release` 必须在 `grab` 对象销毁前发出；发完立刻把 `self.grab` 清零，否则下次 `activate` 会因 `grab != 0` 跳过重新抓取，导致「第一次能用、之后再也收不到键」。

## 5. 验证方法（可复现）

前置：`/dev/uinput` 可写（`sudo chmod 666 /dev/uinput`，或 udev `TAG+="uaccess"`）。

```sh
# 1. 打开启动器
omarchy-menu summon root

# 2. evdev 级注入（与物理键盘等价：libinput → compositor → seat → grab）
cd xime-rs/tools && ./uinput-inject type nihao
./uinput-inject tap space

# 3. 断言
grep -E 'commit|消费' ~/.local/state/xime-dbus.log | tail
```

修复前：搜索框空、`ProcessKeyEvent` 为 0。
修复后：搜索框出现拼音与候选条，空格后 `xime-dbus: commit "你好"`。

对照实验（定位用，不需改代码）：

```sh
systemctl --user stop xime.service   # 停掉输入法
omarchy-menu summon root && ./uinput-inject type nihao
# 若这时按键能进搜索框 → 元凶在输入法（本次即如此）
```

## 6. 同期修复的关联问题

| 问题 | 修复 |
|---|---|
| `DestroyIC` 被子 zbus 自动转成 `DestroyIc`，客户端销毁调用全部 `Unknown method`，`/inputcontext/N` 无限累积 | `#[zbus(name = "DestroyIC")]` + 真正 `ObjectServer::remove`，实测开关菜单后 IC 归零 |
| 中英状态两个前端各存一份：终端里按 Shift 只切 Wayland 前端，Qt 应用仍是中文，反之亦然 | 新增 `src/im_state.rs`，用 `$XDG_RUNTIME_DIR/xime-im-state`（`zh`/`en`）双向同步 |
| 英文态弹出前缀补全候选窗，与 fcitx5 不一致 | 默认纯透传（不组字、不推 preedit、不弹候选），`XIME_EN_COMPLETION=1` 可恢复旧行为 |

## 7. 仍未解决

1. **候选窗位置飘**：D-Bus 路径下候选窗由客户端（fcitx5-qt）自绘，xime 拿不到有效光标矩形——Omarchy 的 `FilterInput` 用了空 `cursorDelegate`，报给输入法的矩形是 `0x16 @0,-16`（宽 0）。要修得靠「矩形无效时退回屏幕底部居中」这类兜底，属独立议题。
2. **`/usr/bin/xime` 需重新装包**：`/usr/bin` 需要 sudo，修复目前只在 `~/.local/bin/xime`（`xime-both` 优先取它）。装包命令：
   ```sh
   cd ~/lab/x-input/xime-rs
   sudo pacman -U dist/xime-0.3.0-1-x86_64.pkg.tar.zst
   systemctl --user restart xime.service
   ```
3. **Apps 应用网格（`AppGrid.qml`）的搜索框本身不支持任何输入法**：它用 `Keys.onPressed` 手搓字符串，不是 `TextInput`。这是 Omarchy 上游问题，需要改上游 QML。

## 8. 排查手段清单（下次直接用）

```sh
xime-both --status                       # 双前端进程 + 中英状态 + 服务名归属
busctl --user tree org.fcitx.Fcitx5      # inputcontext/N 列表
busctl --user monitor org.fcitx.Fcitx5   # 实时 D-Bus 报文（ProcessKeyEvent / CommitString）
hyprctl devices -j                       # 键盘设备与 main keyboard
hyprctl layers -j                        # 菜单 / 候选窗这类 layer-shell 表面
tools/verify-real-input.sh nihao 你好     # 直接对真实 IC 断言的端到端脚本
tools/uinput-inject type|tap <keys>      # 唯一等价于物理键盘的注入
```
