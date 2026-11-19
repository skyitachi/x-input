# fcitx5 空预编辑修复补丁

修复 fcitx5 在 Wayland（Electron / Chromium / GTK4 / Qt）下的两个症状：

- 切换中英文、或退格把预编辑退空之后，**第一个字符删不掉**；
- 光标停在预编辑文本的最前面（`|ni hao`）。

完整的症状、根因与取证过程见 `../docs/fcitx5-known-issues.md`。

## 根因

`fcitx5` 的 `src/frontend/waylandim/waylandimserverv2.cpp` 里：

```cpp
if (preedit.textLength() > 0 && ...) { ic_->setPreeditString(...); }
```

预编辑为空时这条更新被丢弃，而 Wayland 客户端正是靠「收到空预编辑」结束 composition。
客户端因此残留一份幽灵 composition，首字退格被它吃掉、光标按残留长度定位。

## 用法

```bash
./build.sh            # 构建并只替换 /usr/lib/fcitx5/libwaylandim.so
./build.sh --revert   # 还原系统原版
```

需要 sudo 与构建依赖（脚本会用 pacman 补齐 `cmake ninja extra-cmake-modules` 等）。
原版 `.so` 备份到 `/usr/lib/fcitx5/libwaylandim.so.pacman-orig`。

安装后可以删掉 `~/.config/fcitx5/config` 里的 `PreeditEnabledByDefault=False`，恢复行内预编辑。

注意：改 fcitx5 配置要先 `systemctl --user stop omarchy-fcitx5`，否则退出时会被内存配置覆盖。

## 文件

| 文件 | 说明 |
|---|---|
| `0001-waylandim-allow-empty-preedit.patch` | 把 `> 0` 改成 `>= 0`，并加注释 |
| `build.sh` | 取同版本源码 → 打补丁 → 只编 `waylandim` → 备份并替换 `.so` |
