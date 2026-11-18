#!/bin/sh
# 找一个含汉字字形的字体，供 mkfont 烘焙字形图集。可通过 FONT=/path 覆盖。
for f in \
  /usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc \
  /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc \
  /usr/share/fonts/noto-cjk/NotoSansCJK-Light.ttc \
  /usr/share/fonts/truetype/wqy/wqy-microhei.ttc \
  /usr/share/fonts/truetype/wqy/wqy-zenhei.ttc \
  /usr/share/fonts/truetype/arphic/uming.ttc \
  /usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf \
  /usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc ; do
  [ -f "$f" ] && { echo "$f"; exit 0; }
done
if command -v fc-list >/dev/null 2>&1; then
  f=$(fc-list :lang=zh file 2>/dev/null | head -n1 | sed 's/:$//')
  [ -n "$f" ] && [ -f "$f" ] && { echo "$f"; exit 0; }
fi
exit 1
