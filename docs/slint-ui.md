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

最终方案：固定分配逻辑 1024×128 的大画布渲染，然后**扫描 alpha 通道求非透明内容包围盒**。
panel 是不透明的圆角矩形，其余全透明，所以包围盒就是候选条本身。为此 `.slint` 里
panel 的宽高绑定到内容布局 `items.preferred-width/height`，避免背景铺满整个画布
（否则包围盒又是 1024×128）。缩放时逻辑画布不变，物理画布 = 逻辑 × scale。

## 4. HiDPI 缩放（适配各种分辨率）

Slint UI 按**逻辑像素**布局，再按设备缩放因子光栅化成**物理像素**，因此同一份
`.slint` 定义在不同 DPI 屏上视觉大小一致、且高分屏字体更锐利（矢量重绘，不是位图拉伸）。
整条链路分三段配合：

1. **scale 来源**（优先级从高到低）
   - 环境变量 `XIME_SCALE`（如 `XIME_SCALE=1.5`）：锁定后忽略合成器的值，便于覆盖/测试；
   - `wp_fractional_scale_v1.preferred_scale`：绑定到 popup surface，合成器按该 surface
     所在显示器报告分数缩放（值/120，支持 1.25/1.5/1.75），混合 DPI 多显示器下最准；
   - `wl_output.scale`：第一个 output 的整数缩放，回退用；
   - 都没有则 1.0。

2. **Slint 渲染**：`SlintUi::set_scale()` 向窗口派发 `ScaleFactorChanged` 事件，
   软件渲染器据此把字体/间距/圆角按 scale 放大；物理画布按 `ceil(逻辑×scale)` 分配，
   alpha 裁剪与 ARGB8888 输出都是物理像素。

3. **合成器侧**：仅当 scale≠1 时，通过 `wp_viewport.set_destination(逻辑宽, 逻辑高)`
   告诉合成器"物理 buffer 对应这么大的逻辑区域"。否则合成器会把 2x buffer 当成 2x
   逻辑尺寸，候选条在缩放屏上会放大一倍。scale=1 时完全不设 viewport，保持原行为。

运行时切换显示器导致 scale 变化时，`apply_scale()` 会同步 Slint 并用当前候选重绘一帧，
候选条尺寸立即跟随。**注意**：`wp_viewport` 与 `wp_fractional_scale_v1` 都是 staging 协议，
本机 Hyprland 支持（`--probe` 可见）；合成器不支持时静默回退到 scale=1 的 8-bit 路径。

缩放验证截图见 `xime-rs/docs/screenshots/scale-normalized.png`：scale 1.0/1.25/1.5/1.75/
2.0/3.0 六档物理尺寸 378→1134px，归一化到同宽后外观完全一致，证明是矢量重绘而非拉伸。

## 5. 改动文件

| 文件 | 作用 |
|---|---|
| `xime-rs/ui/candidate.slint` | 候选条声明式 UI（Theme 全局 + Chip 组件 + CandidateBar，横排/竖排/网格三形态） |
| `xime-rs/build.rs` | 编译期调用 `slint_build::compile` |
| `xime-rs/src/ui_slint.rs` | Slint 平台适配、软件渲染、alpha 裁剪、ARGB 转换、BMP 探针、`set_scale` 缩放、形态/主题映射（`reload` 热重载） |
| `xime-rs/src/main.rs` | `Xime` 持有 `Option<SlintUi>`；`popup_update` 按开关分流；scale 来源与 `apply_scale`；viewport destination |
| `xime-rs/src/proto.rs` | 新增 wl_output / fractional-scale / viewporter 的绑定与请求构造 |
| `xime-rs/Cargo.toml` / `Cargo.lock` | slint 1.19 + slint-build 1.19 依赖 |
| `xime-rs/.cargo/config.toml` | rsproxy 镜像（本机直连 crates.io 索引解析卡死） |

另外顺手修了一个既有 bug：`deactivate` 时没有把 `self.grab` 清零，导致重新
`activate` 时跳过重新抓取键盘，输入法再也收不到按键。这个 bug 与 Slint 无关，
但会掩盖候选窗的验证。

## 6. 使用与开关

```sh
cd xime-rs
cargo build --release

# 默认：Slint 渲染候选窗（scale 自动跟随合成器）
./target/release/xime --verbose

# 回退到手写 Canvas（对照 / 救急）
./target/release/xime --legacy-ui

# 锁定缩放（忽略合成器，便于测试/覆盖）
XIME_SCALE=2 ./target/release/xime --verbose

# 离屏校验 Slint 渲染（写 BMP，不需要合成器 / 输入法焦点）
./target/release/xime --ui-probe /tmp/slint.bmp nihao
#   → 402x38，6 条候选，棋盘格背景暴露圆角与透明区
# 形态/主题跟随 --popup / --popup-cols / --theme（离屏预览三种形态）
./target/release/xime --ui-probe /tmp/vertical.bmp nihao 1 --popup vertical
./target/release/xime --ui-probe /tmp/grid.bmp nihao 1 --popup grid --theme dark
# 指定缩放校验 HiDPI（物理尺寸按比例放大，外观一致）
./target/release/xime --ui-probe /tmp/slint2x.bmp nihao 2
#   → 756x76（正好 2x）
```

`--popup` / `--popup-cols` / `--theme` 对 Slint UI **已生效**：形态（横排/竖排/网格）、
列数、颜色、度量（圆角/间距/边框）都从主题映射进 `.slint`，配置热重载同步更新。

## 7. 验证结果

- **离屏渲染**：`--ui-probe nihao` 产出 402×38 BMP，6 个候选带序号、首项蓝色高亮、
  圆角背景贴合内容、文字清晰。三种形态均已验证：横排 402×38、竖排 82×262（9 条）、
  网格 210×94（3 列 3 行）；`--theme dark` 深色主题与 `--popup-cols` 列数均生效。
  与手写渲染（480×36）视觉一致，Slint 版字重略细、圆角略大。
- **运行时链路**：`XIME_SELFTEST=nihao` 触发 `engine.push_char → refresh →
  popup_update: 27 条 slint=true → commit "你好"`，证明 Slint 渲染路径在实际运行中
  被调用、引擎产出的候选送进了 Slint。
- **HiDPI 缩放**：`--ui-probe ... <scale>` 六档（1.0/1.25/1.5/1.75/2.0/3.0）物理尺寸
  378/472/567/662/756/1134 px，比例与 scale 吻合；归一化到同宽后外观完全一致
  （见 `docs/screenshots/scale-normalized.png`），证明字体是矢量按 scale 重绘。
  修复过一个初始化 bug：scale=1.0 时 `set_scale` 被"相同值跳过"短路导致缓冲未分配而 panic，
  已改为初始 scale 哨兵值 0.0 强制首次分配。
- **测试**：`cargo test --release` 51 项全过；release 编译通过（无警告）。
- **体积**：release 二进制 9.0MB → **15.9MB**（Slint + 字体栈 + 渲染器，+7MB）。
  这是放弃零依赖的直接代价。

## 8. 已知限制 / 未完成

- **端到端 popup 定位未在本机截到**：GTK4 探针与 xime 的 text-input-v3 握手会
  反复 `activate ↔ deactivate`，导致键盘 grab 不稳定，按键直接进应用、popup 不弹。
  这是 xime Wayland 前端的既有问题（master 的 `--inline-candidates` 绕开了它），
  不是 Slint 引入的。离屏渲染 + selftest 链路已分别验证两部分都正确。
- 网格形态的列宽按各行内容自适应，不做全局等宽对齐（Slint 的 GridLayout 要求
  列位置是编译期常量，动态列数只能退化为「竖排套横排」的嵌套布局）。
- `set_cursor_rect`（应用报告光标位置）事件仍未在 `on_im` 中处理，popup 定位完全
  依赖合成器的隐式行为。
