#!/bin/sh
# 把静态二进制组装成可 pacman -U 安装的 Arch 包（.pkg.tar.zst）。
#
# 与 build-deb.sh 对应：文件布局一致（/usr/bin/xime、/usr/bin/xime-setup、文档），
# 安装/卸载钩子取自 packaging/xime.install（pacman 约定的 post_install/pre_remove），
# 复制为包根部 .INSTALL，libalpm 按文件名自动识别执行。
#
# 为什么不用 makepkg：它内部要跑 rm，在受限环境（无 rm 沙箱）里跑不动；
# 这里按 Arch 包格式手工组装 .PKGINFO / .MTREE / .INSTALL，pacman -U 可直接安装。
# 不依赖 rm：暂存目录原地覆盖，可重复构建。
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

BIN=target/x86_64-unknown-linux-musl/release/xime
STAGE=build/arch-pkg
OUTDIR=dist

[ -f "$BIN" ] || { echo "先执行 cargo build --release --target x86_64-unknown-linux-musl" >&2; exit 1; }
command -v bsdtar >/dev/null 2>&1 || { echo "缺少 bsdtar（libarchive）" >&2; exit 1; }

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
ARCH=x86_64
if command -v zstd >/dev/null 2>&1; then
    EXT=zst
else
    EXT=gz   # pacman 同样接受 .pkg.tar.gz
fi
OUT="$OUTDIR/xime-$VERSION-1-$ARCH.pkg.tar.$EXT"

mkdir -p "$STAGE/usr/bin" "$STAGE/usr/share/doc/xime" \
         "$STAGE/usr/share/applications" "$STAGE/etc/xdg/autostart" \
         "$STAGE/usr/lib/systemd/user" "$OUTDIR"

install -m 755 "$BIN" "$STAGE/usr/bin/xime"
install -m 755 packaging/xime-both "$STAGE/usr/bin/xime-both"
install -m 755 packaging/xime-setup "$STAGE/usr/bin/xime-setup"
install -m 755 packaging/xime-switch "$STAGE/usr/bin/xime-switch"
install -m 644 README.md "$STAGE/usr/share/doc/xime/README.md"
install -m 644 packaging/copyright "$STAGE/usr/share/doc/xime/copyright"
install -m 644 packaging/config.ini.example "$STAGE/usr/share/doc/xime/config.ini.example"
# KDE「系统设置 → 键盘 → 虚拟键盘」靠这个条目发现 xime
install -m 644 packaging/xime.desktop "$STAGE/usr/share/applications/xime.desktop"
# fcitx5 式的开机自启（NotShowIn 挡掉了 KDE/GNOME；xime 内置单实例锁，重复拉起无害）
install -m 644 packaging/xime-autostart.desktop "$STAGE/etc/xdg/autostart/xime.desktop"
# 与 fcitx5 同款生命周期管理（Omarchy 上 fcitx5 就是 systemd 用户单元）
install -m 644 packaging/xime.service "$STAGE/usr/lib/systemd/user/xime.service"
install -m 644 packaging/xime.install "$STAGE/.INSTALL"

SIZE=$(du -sk "$STAGE/usr" | cut -f1)
DATE=$(date +%s)

cat > "$STAGE/.PKGINFO" <<EOF
pkgname = xime
pkgbase = xime
xdata = pkgtype=pkg
pkgver = $VERSION-1
pkgdesc = 单包装完即用的 Wayland 原生中文输入法（Rust 实现，musl 静态链接，支持简繁与英文补全）
url = https://localhost/xime
builddate = $DATE
packager = xime <xime@localhost>
size = $SIZE
arch = $ARCH
license = MIT
# optdepend：X11/XWayland 应用覆盖需要 fcitx5 客户端模块（DBus 前端靠它们做客户端）。
# 只拉客户端模块，不拉 fcitx5 守护进程。pacman 不会自动装 optdepend，
# 缺省由 xime-setup --auto 里的 ensure_frontends 补装。
optdepend = fcitx5-qt: X11/XWayland Qt 应用输入（QT_IM_MODULE=fcitx 走 DBus 前端）
optdepend = fcitx5-gtk: X11/XWayland GTK 应用输入（GTK_IM_MODULE=fcitx 走 DBus 前端）
EOF

# .MTREE：pacman 安装时核对文件清单与校验和（不含 .MTREE 自身）。
# makepkg 生成的是 gzip 压缩流，libalpm 也按 gzip 读取，保持一致。
( cd "$STAGE" && bsdtar --format=mtree --options='!all,md5,sha256,cksum' \
      -cf - .PKGINFO .INSTALL usr etc ) | gzip -n > "$STAGE/.MTREE"

# 组包：元数据在包根部，数据目录归 root，一次 tar 流过去压缩
if [ "$EXT" = zst ]; then
    ( cd "$STAGE" && tar --owner=0 --group=0 --numeric-owner -cf - \
          .MTREE .PKGINFO .INSTALL usr etc ) | zstd -q -f -o "$OUT" -T0 --no-progress
else
    ( cd "$STAGE" && tar --owner=0 --group=0 --numeric-owner -cf - \
          .MTREE .PKGINFO .INSTALL usr ) | gzip -n > "$OUT"
fi

echo "  $OUT  ($(du -h "$OUT" | cut -f1)，零硬依赖；optdepend: fcitx5-qt/gtk，pacman -U 安装)"
