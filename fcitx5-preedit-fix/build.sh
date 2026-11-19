#!/bin/bash
# 给 fcitx5 打「允许空预编辑通过」的补丁，修复 Wayland(Electron/Chromium) 下
# 「切换输入法或退格后第一个字符删不掉、光标跳到预编辑开头」的问题。
#
# 只替换 /usr/lib/fcitx5/libwaylandim.so 这一个文件，不重装整个 fcitx5。
# 需要 sudo（安装这一步）+ 构建依赖。
#
#   ./build.sh            构建并安装
#   ./build.sh --revert   还原成系统原版
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$HERE/fcitx5-src"
BUILD="$SRC/build"
PATCH="$HERE/0001-waylandim-allow-empty-preedit.patch"
TARGET="/usr/lib/fcitx5/libwaylandim.so"
BACKUP="/usr/lib/fcitx5/libwaylandim.so.pacman-orig"

VER="$(fcitx5 --version 2>/dev/null | head -1 | awk '{print $NF}')"
VER="${VER%%-*}"
echo "检测到 fcitx5 版本: ${VER:-未知}"

if [[ "${1:-}" == "--revert" ]]; then
  [[ -f "$BACKUP" ]] || { echo "没有找到备份 $BACKUP"; exit 1; }
  sudo install -m 755 "$BACKUP" "$TARGET"
  systemctl --user restart omarchy-fcitx5
  echo "已还原系统原版 libwaylandim.so"
  exit 0
fi

# 1. 构建依赖
sudo pacman -S --needed --noconfirm base-devel cmake ninja extra-cmake-modules \
  libxkbcommon wayland wayland-protocols fmt iso-codes cairo pango \
  xcb-util xcb-util-wm xcb-util-keysyms xcb-util-cursor libxcb

# 2. 取同版本源码
if [[ ! -d "$SRC/.git" ]]; then
  git clone --depth 1 --branch "$VER" https://github.com/fcitx/fcitx5 "$SRC"
fi
cd "$SRC"
git checkout -q "$VER"
git stash list >/dev/null 2>&1 || true
git checkout -q -- . 2>/dev/null || true

# 3. 打补丁（幂等）
if ! grep -q "textLength() >= 0" src/frontend/waylandim/waylandimserverv2.cpp; then
  patch -p1 < "$PATCH"
  echo "补丁已应用"
else
  echo "补丁已在源码中"
fi

# 4. 编译（只编 waylandim 这个 addon，省时间）
cmake -S "$SRC" -B "$BUILD" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX=/usr \
  -DENABLE_TEST=OFF \
  -DENABLE_DOCS=OFF
ninja -C "$BUILD" waylandim

BUILT="$(find "$BUILD" -name 'libwaylandim.so' -print -quit)"
[[ -n "$BUILT" ]] || { echo "构建产物没找到"; exit 1; }
echo "产物: $BUILT"

# 5. 备份原版并安装
[[ -f "$BACKUP" ]] || sudo cp -a "$TARGET" "$BACKUP"
sudo install -m 755 "$BUILT" "$TARGET"
systemctl --user restart omarchy-fcitx5
sleep 2
systemctl --user is-active omarchy-fcitx5
echo "完成。现在「行内预编辑」可以保持开启（~/.config/fcitx5/config 里删掉 PreeditEnabledByDefault=False）。"
echo "还原：$0 --revert"
