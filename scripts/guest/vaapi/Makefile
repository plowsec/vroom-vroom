# vroom's VA-API driver for Linux guests. Needs libva-dev, libdrm-dev,
# libgbm-dev. `make install` puts it where libva looks (Debian/Ubuntu).
CFLAGS ?= -O2 -g
CFLAGS += -std=gnu11 -Wall -Wextra -Wno-unused-parameter -fPIC -fvisibility=hidden $(shell pkg-config --cflags libva libdrm gbm)
LDLIBS = $(shell pkg-config --libs gbm) -lpthread
DRIVERS_DIR ?= $(shell pkg-config --variable=driverdir libva)

vroom_drv_video.so: vroom_drv_video.c
	$(CC) $(CFLAGS) -shared -o $@ $< $(LDLIBS)

install: vroom_drv_video.so
	install -D -m 644 vroom_drv_video.so $(DESTDIR)$(DRIVERS_DIR)/vroom_drv_video.so

clean:
	rm -f vroom_drv_video.so

.PHONY: install clean
