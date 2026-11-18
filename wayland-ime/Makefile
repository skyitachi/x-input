CC          ?= cc
PKG_CONFIG  ?= pkg-config

GEN         := build/gen
OBJ         := build/obj
BIN         := build/xime
FONT_PX     ?= 16
FONT        ?= $(shell sh scripts/find-font.sh 2>/dev/null)

WL_CFLAGS   := $(shell $(PKG_CONFIG) --cflags wayland-client)
WL_LIBS     := $(shell $(PKG_CONFIG) --libs wayland-client)
XKB_CFLAGS  := $(shell $(PKG_CONFIG) --cflags xkbcommon)
XKB_LIBS    := $(shell $(PKG_CONFIG) --libs xkbcommon)
WLS_CFLAGS  := $(shell $(PKG_CONFIG) --cflags wayland-server)
WLS_LIBS    := $(shell $(PKG_CONFIG) --libs wayland-server)

CFLAGS      ?= -O2 -g
CFLAGS      += -std=c11 -Wall -Wextra -Wno-unused-parameter -MMD -MP
CPPFLAGS    += -D_GNU_SOURCE -I$(GEN) -Isrc $(WL_CFLAGS) $(XKB_CFLAGS)
LDLIBS      += $(WL_LIBS) $(XKB_LIBS) -lm

CORE_SRC    := src/utf8.c src/dict.c src/engine.c src/render.c src/popup.c src/xime.c
PROTO_SRC   := $(GEN)/input-method-v2.c $(GEN)/virtual-keyboard.c
ATLAS_SRC   := $(GEN)/font_atlas.c

CORE_OBJ    := $(patsubst src/%.c,$(OBJ)/%.o,$(CORE_SRC))
PROTO_OBJ   := $(patsubst $(GEN)/%.c,$(OBJ)/%.o,$(PROTO_SRC))
ATLAS_OBJ   := $(OBJ)/font_atlas.o

.PHONY: all clean gen font dict test test-proto deb

# 自动头文件依赖：dict_gen.h / *.h 变化时相关目标会重编
-include $(CORE_OBJ:.o=.d) $(ATLAS_OBJ:.o=.d)

all: $(BIN)

$(OBJ) $(GEN):
	mkdir -p $@

gen: $(GEN)
	sh proto/gen.sh $(GEN)

$(GEN)/input-method-v2.h: proto/vendor/input-method-unstable-v2.xml
	sh proto/gen.sh $(GEN)

$(GEN)/virtual-keyboard.h: proto/vendor/wlr-virtual-keyboard-unstable-v1.xml
	sh proto/gen.sh $(GEN)

$(GEN)/input-method-v2-server.h $(GEN)/virtual-keyboard-server.h: proto/gen.sh \
        proto/vendor/input-method-unstable-v2.xml proto/vendor/wlr-virtual-keyboard-unstable-v1.xml
	sh proto/gen.sh $(GEN)

$(BIN): $(CORE_OBJ) $(PROTO_OBJ) $(ATLAS_OBJ)
	$(CC) $(CFLAGS) -o $@ $^ $(LDLIBS)

$(OBJ)/%.o: src/%.c | $(OBJ) $(GEN)/input-method-v2.h $(GEN)/virtual-keyboard.h
	$(CC) $(CFLAGS) $(CPPFLAGS) -c -o $@ $<

$(OBJ)/%.o: $(GEN)/%.c | $(OBJ)
	$(CC) $(CFLAGS) $(CPPFLAGS) -c -o $@ $<

$(OBJ)/font_atlas.o: $(ATLAS_SRC) | $(OBJ)
	$(CC) $(CFLAGS) $(CPPFLAGS) -c -o $@ $<

# 从公开数据源重新生成内置词库（需要网络；已缓存的源文件会复用）
dict:
	sh scripts/fetch-dict.sh
	python3 tools/mkdict.py
	@echo "词库已更新，字形图集需要重建：make font"

$(ATLAS_SRC): tools/mkfont.c src/dict.h src/dict_gen.h src/dict.c src/utf8.c scripts/find-font.sh | $(GEN)
	@if [ -z "$(FONT)" ]; then echo "找不到含汉字的字体，请用 FONT=/path/to/font.ttc make"; exit 1; fi
	$(CC) -O2 -Ithird_party -Isrc -o build/mkfont tools/mkfont.c src/dict.c src/utf8.c -lm
	build/mkfont "$(FONT)" $(ATLAS_SRC) $(FONT_PX)

font: $(ATLAS_SRC)

test: build/test_engine
	build/test_engine

test-proto: build/mock_compositor $(BIN)
	build/mock_compositor

build/mock_compositor: test/mock_compositor.c $(GEN)/input-method-v2-server.h \
                       $(GEN)/virtual-keyboard-server.h $(PROTO_OBJ) | $(OBJ)
	$(CC) $(CFLAGS) $(CPPFLAGS) $(WLS_CFLAGS) -o $@ $< $(PROTO_OBJ) $(WLS_LIBS) $(WL_LIBS) $(XKB_LIBS) -lm

build/test_engine: test/test_engine.c src/engine.c src/dict.c src/utf8.c | $(OBJ)
	$(CC) $(CFLAGS) $(CPPFLAGS) -o $@ $^ -lm

clean:
	rm -rf build

deb: all
	sh packaging/build-deb.sh
