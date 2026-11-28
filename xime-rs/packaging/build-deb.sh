#!/bin/sh
# 把编译产物（Slint 主线为动态链接 gnu 二进制）组装成可 dpkg -i 的单包 deb。
#
# 优先用 dpkg-deb；缺失时（如 Arch 系）用 ar + tar 手工组装，两者产物结构一致。
# 注意：这里不依赖 rm，保证在受限环境里也能重复构建（原地覆盖）。
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# Slint 主线走宿主 gnu target（动态链接 fontconfig）；保留 musl 路径兼容旧构建。
# 可用 XIME_BIN 显式指定，供交叉编译产物打包。
BIN="${XIME_BIN:-}"
if [ -z "$BIN" ]; then
    if [ -f target/release/xime ]; then
        BIN=target/release/xime
    else
        BIN=target/x86_64-unknown-linux-musl/release/xime
    fi
fi
STAGE=build/deb
OUTDIR=dist

[ -f "$BIN" ] || { echo "先执行 cargo build --release（或用 XIME_BIN 指定二进制路径）" >&2; exit 1; }
echo "  打包二进制: $BIN"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
ARCH=amd64
OUT="$OUTDIR/xime_${VERSION}_${ARCH}.deb"

mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/doc/xime" \
         "$STAGE/usr/share/applications" "$STAGE/etc/xdg/autostart" \
         "$STAGE/usr/share/ibus/component" \
         "$STAGE/usr/lib/systemd/user" "$OUTDIR"

install -m 755 "$BIN" "$STAGE/usr/bin/xime"
install -m 755 packaging/xime-both "$STAGE/usr/bin/xime-both"
install -m 755 packaging/xime-setup "$STAGE/usr/bin/xime-setup"
install -m 755 packaging/xime-switch "$STAGE/usr/bin/xime-switch"
install -m 755 packaging/xime-compat "$STAGE/usr/bin/xime-compat"
install -m 755 packaging/xime-reconnect-shell "$STAGE/usr/bin/xime-reconnect-shell"
install -m 644 README.md "$STAGE/usr/share/doc/xime/README.md"
install -m 644 packaging/copyright "$STAGE/usr/share/doc/xime/copyright"
# 外观配置样例：复制到 ~/.config/xime/config.ini 后按需改
install -m 644 packaging/config.ini.example "$STAGE/usr/share/doc/xime/config.ini.example"
# KDE「系统设置 → 键盘 → 虚拟键盘」靠这个条目发现 xime
install -m 644 packaging/xime.desktop "$STAGE/usr/share/applications/xime.desktop"
# fcitx5 式的开机自启（NotShowIn 挡掉了 KDE/GNOME；xime 内置单实例锁，重复拉起无害）
install -m 644 packaging/xime-autostart.desktop "$STAGE/etc/xdg/autostart/xime.desktop"
# 与 fcitx5 同款生命周期管理（Omarchy 上 fcitx5 就是 systemd 用户单元）
install -m 644 packaging/xime.service "$STAGE/usr/lib/systemd/user/xime.service"
# IBus 组件：GNOME 靠它发现 xime 引擎（Mutter 没有 zwp_input_method_v2）
install -m 644 packaging/xime-ibus.xml "$STAGE/usr/share/ibus/component/xime.xml"
install -m 644 packaging/postinst "$STAGE/DEBIAN/postinst"
install -m 644 packaging/prerm "$STAGE/DEBIAN/prerm"
install -m 644 packaging/postrm "$STAGE/DEBIAN/postrm"
chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

# 安装后占用（KB）
SIZE=$(du -sk "$STAGE/usr" | cut -f1)

sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" \
    packaging/control > "$STAGE/DEBIAN/control"

# md5sums 里列的是相对根目录的路径（usr/bin/xime），不带 ./ 前缀
( cd "$STAGE" && find usr -type f -printf '%p\0' | sort -z | xargs -0 md5sum ) \
    > "$STAGE/DEBIAN/md5sums"

find "$STAGE" -type d -exec chmod 755 {} +
chmod 644 "$STAGE/DEBIAN/control" "$STAGE/DEBIAN/md5sums"

if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null
else
    # 手工组装：debian-binary / control.tar.gz / data.tar.gz，顺序不能变
    mkdir -p build/deb-arch
    printf '2.0\n' > build/deb-arch/debian-binary
    ( cd "$STAGE/DEBIAN" && tar -czf "$root/build/deb-arch/control.tar.gz" \
          --owner=0 --group=0 --numeric-owner . )
    ( cd "$STAGE" && tar -czf "$root/build/deb-arch/data.tar.gz" \
          --owner=0 --group=0 --numeric-owner ./usr ./etc )
    ( cd build/deb-arch && ar rc "$root/$OUT" debian-binary control.tar.gz data.tar.gz )
fi

echo "  $OUT  ($(du -h "$OUT" | cut -f1)，安装后 ${SIZE}KB，Depends: libc6/libfontconfig1/libgcc-s1)"
