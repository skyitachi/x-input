# 候选窗 UI：从手写软光栅迁移到 Slint

分支：`feat/slint-ui`。这个分支**放弃了 master 的「零动态依赖」前提**——按需求改用
Slint 画候选窗。输入法协议层（harness、按键路由、解码引擎、wl_shm 上传）完全不变，
替换的只是「候选条长什么样、由谁画」这一层。

## 1. 为什么这样切

master 的候选窗是手写软光栅：`font.rs`（构建期烘焙 8bpp 字形图集）+
`render.rs`（ARGB8888 圆角/字形叠加）+ `popup.rs`（三种布局度量）。代价是
字号、圆角、配色、布局全部硬编码在 Rust 里，改一个间距要重编译，字形也只覆盖
图集里的 7536 个。

Slint 把这一整层换成声明式 UI：

- 布局、间距、圆角、配色写在 `xime-rs/ui/candidate.slint`；
- 字形交给 Slint 软件渲染器 + 系统字体（不再依赖自建图集）；
- 改样式只改 `.slint`，由 `build.rs` 在编译期转成 Rust。

## 2. 关键约束：Slint 只当渲染器，不当窗口系统

Wayland 下候选窗**不能**是普通 `xdg_toplevel`：它必须挂在
`zwp_input_popup_surface_v2` 上，位置由合成器贴着光标决定，而且不能抢焦点。
Slint 自带的 winit/femtovg 后端在这里都不能用。

所以这里只取 Slint 的**软件渲染器**（`renderer-software` feature），把界面画进
一块内存缓冲，再把像素交给现有的 `wl_shm` popup 流程：

```
slint .slint 布局
   → MinimalSoftwareWindow (软件光栅)
   → 预乘 RGBA8 缓冲
   → 扫描 alpha 求内容包围盒（裁掉留白）
   → PremultipliedRgbaColor → ARGB8888
   → Shm::upload → wl_shm buffer
   → zwp_input_popup_surface_v2.attach + commit
```

Cargo 侧：`slint` 关掉 default-features，只开 `std` + `renderer-software` +
`compat-1-18`（1.19+ 强制的前向兼容开关）；`slint-build` 必须保留默认 feature，
否则编译期嵌入的字体格式与运行时 renderer-software 对不上。

## 3. 尺寸的确定（踩过的坑）

`MinimalSoftwareWindow` **不会**根据内容自动撑开——它的 size 完全由 `set_size()`
驱动，初始是 0。两条错误路径都试过：

1. 读 `window.size()` → 初始 0 → 兜底成 1x1，渲染出空图；
2. 在 `.slint` 里导出 `panel.preferred-width` → 对含 `for` 循环的布局求值不可靠，
   实测给出 22px（实际内容宽约 378px）。

最终方案：固定分配 1024×128 的大画布渲染，然后**扫描 alpha 通道求非透明内容包围盒**。
panel 是不透明的圆角矩形，其余全透明，所以包围盒就是候选条本身。为此 `.slint` 里
panel 的宽高绑定到内容布局 `items.preferred-width/height`，避免背景铺满整个画布
（否则包围盒又是 1024×128）。

## 4. 改动文件

| 文件 | 作用 |
|---|---|
| `xime-rs/ui/candidate.slint` | 候选条声明式 UI（Theme 全局 + Chip 组件 + CandidateBar） |
| `xime-rs/build.rs` | 编译期调用 `slint_build::compile` |
| `xime-rs/src/ui_slint.rs` | Slint 平台适配、软件渲染、alpha 裁剪、ARGB 转换、BMP 探针 |
| `xime-rs/src/main.rs` | `Xime` 持有 `Option<SlintUi>`；`popup_update` 按开关分流；`--ui-probe` / `--legacy-ui` |
| `xime-rs/Cargo.toml` / `Cargo.lock` | slint 1.19 + slint-build 1.19 依赖 |
| `xime-rs/.cargo/config.toml` | rsproxy 镜像（本机直连 crates.io 索引解析卡死） |

另外顺手修了一个既有 bug：`deactivate` 时没有把 `self.grab` 清零，导致重新
`activate` 时跳过重新抓取键盘，输入法再也收不到按键。这个 bug 与 Slint 无关，
但会掩盖候选窗的验证。

## 5. 使用与开关

```sh
cd xime-rs
cargo build --release

# 默认：Slint 渲染候选窗
./target/release/xime --verbose

# 回退到手写 Canvas（对照 / 救急）
./target/release/xime --legacy-ui

# 离屏校验 Slint 渲染（写 BMP，不需要合成器 / 输入法焦点）
./target/release/xime --ui-probe /tmp/slint.bmp nihao
#   → 378x38，6 条候选，棋盘格背景暴露圆角与透明区
```

`--popup` / `--theme` 在这版对 Slint UI **不生效**（样式固定在 `.slint` 里），
只对 `--legacy-ui` 有效。

## 6. 验证结果

- **离屏渲染**：`--ui-probe nihao` 产出 378×38 BMP，6 个候选带序号、首项蓝色高亮、
  圆角背景贴合内容、文字清晰。与手写渲染（480×36）视觉一致，Slint 版字重略细、
  圆角略大。
- **运行时链路**：`XIME_SELFTEST=nihao` 触发 `engine.push_char → refresh →
  popup_update: 27 条 slint=true → commit "你好"`，证明 Slint 渲染路径在实际运行中
  被调用、引擎产出的候选送进了 Slint。
- **测试**：`cargo test --release` 51 项全过；release 编译通过。
- **体积**：release 二进制 9.0MB → **16MB**（Slint + 字体栈 + 渲染器，+7MB）。
  这是放弃零依赖的直接代价。

## 7. 已知限制 / 未完成

- **端到端 popup 定位未在本机截到**：GTK4 探针与 xime 的 text-input-v3 握手会
  反复 `activate ↔ deactivate`，导致键盘 grab 不稳定，按键直接进应用、popup 不弹。
  这是 xime Wayland 前端的既有问题（master 的 `--inline-candidates` 绕开了它），
  不是 Slint 引入的。离屏渲染 + selftest 链路已分别验证两部分都正确。
- Slint UI 目前只实现了横排一种形态；master 的 vertical/grid 布局尚未移植。
- `set_cursor_rect`（应用报告光标位置）事件仍未在 `on_im` 中处理，popup 定位完全
  依赖合成器的隐式行为。
