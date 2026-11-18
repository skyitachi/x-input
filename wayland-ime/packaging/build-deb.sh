#!/bin/sh
# 生成单个 deb 包。
# 优先用 dpkg-deb；没有 dpkg-deb 的机器（如 Arch 构建机）退化为 ar + tar 手工组装，
# 两条路径产出的包结构完全一致，都可以被 dpkg/apt 直接安装。
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

[ -x build/xime ] || { echo "build-deb: 先构建二进制（make all）" >&2; exit 1; }

VERSION=$(sed -n 's/^#define XIME_VERSION "\(.*\)"/\1/p' src/xime.c | head -n1)
VERSION=${VERSION:-0.1.0}
if [ -z "${ARCH:-}" ]; then
    if command -v dpkg >/dev/null 2>&1; then
        ARCH=$(dpkg --print-architecture)
    else
        case "$(uname -m)" in
            x86_64) ARCH=amd64 ;;
            aarch64|arm64) ARCH=arm64 ;;
            *) ARCH=$(uname -m) ;;
        esac
    fi
fi

STAGE=build/deb/xime
OUT=dist/xime_${VERSION}_${ARCH}.deb

mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/doc/xime" dist

install -m 755 build/xime "$STAGE/usr/bin/xime"
install -m 755 packaging/xime-setup "$STAGE/usr/bin/xime-setup"
install -m 644 README.md "$STAGE/usr/share/doc/xime/README.md"
install -m 644 packaging/copyright "$STAGE/usr/share/doc/xime/copyright"
install -m 755 packaging/postinst "$STAGE/DEBIAN/postinst"
install -m 755 packaging/prerm "$STAGE/DEBIAN/prerm"
install -m 755 packaging/postrm "$STAGE/DEBIAN/postrm"

SIZE=$(du -sk "$STAGE/usr" | awk '{print $1}')
sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" packaging/control > "$STAGE/DEBIAN/control"

( cd "$STAGE" && find usr -type f -exec md5sum {} + > DEBIAN/md5sums )

if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null
    echo "build-deb: dpkg-deb 打包完成 -> $OUT"
else
    ( cd "$STAGE/DEBIAN" && tar -czf "$root/build/deb/control.tar.gz" --owner=0 --group=0 --numeric-owner . )
    ( cd "$STAGE" && tar -czf "$root/build/deb/data.tar.gz" --owner=0 --group=0 --numeric-owner ./usr )
    printf '2.0\n' > build/deb/debian-binary
    ( cd build/deb && ar rcs "$root/$OUT" debian-binary control.tar.gz data.tar.gz )
    echo "build-deb: ar+tar 打包完成 -> $OUT"
fi

echo "build-deb: 包大小 $(du -h "$OUT" | awk '{print $1}')，安装后占用 ${SIZE}KB"
