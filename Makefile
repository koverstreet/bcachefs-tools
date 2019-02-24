
PREFIX?=/usr/local
INSTALL=install
CFLAGS+=-std=gnu89 -O2 -g -MMD -Wall				\
	-Wno-pointer-sign					\
	-fno-strict-aliasing					\
	-I. -Iinclude -Iraid					\
	-D_FILE_OFFSET_BITS=64					\
	-D_GNU_SOURCE						\
	-D_LGPL_SOURCE						\
	-DRCU_MEMBARRIER					\
	-DZSTD_STATIC_LINKING_ONLY				\
	-DNO_BCACHEFS_CHARDEV					\
	-DNO_BCACHEFS_FS					\
	-DNO_BCACHEFS_SYSFS					\
	-DVERSION_STRING='"$(VERSION)"'				\
	$(EXTRA_CFLAGS)
LDFLAGS+=$(CFLAGS) $(EXTRA_LDFLAGS)

VERSION?=$(shell git describe --dirty=+ 2>/dev/null || echo v0.1-nogit)

CC_VERSION=$(shell $(CC) -v 2>&1|grep -E '(gcc|clang) version')

ifneq (,$(findstring gcc,$(CC_VERSION)))
	CFLAGS+=-Wno-unused-but-set-variable
endif

ifneq (,$(findstring clang,$(CC_VERSION)))
	CFLAGS+=-Wno-missing-braces
endif

ifdef D
	CFLAGS+=-Werror
	CFLAGS+=-DCONFIG_BCACHEFS_DEBUG=y
endif

PKGCONFIG_LIBS="blkid uuid liburcu libsodium zlib liblz4 libzstd"

PKGCONFIG_CFLAGS:=$(shell pkg-config --cflags $(PKGCONFIG_LIBS))
ifeq (,$(PKGCONFIG_CFLAGS))
    $(error pkg-config error)
endif
PKGCONFIG_LDLIBS:=$(shell pkg-config --libs   $(PKGCONFIG_LIBS))
ifeq (,$(PKGCONFIG_LDLIBS))
    $(error pkg-config error (libs))
endif

CFLAGS+=$(PKGCONFIG_CFLAGS)
LDLIBS+=$(PKGCONFIG_LDLIBS)

LDLIBS+=-lm -lpthread -lrt -lscrypt -lkeyutils -laio
LDLIBS+=$(EXTRA_LDLIBS)

ifeq ($(PREFIX),/usr)
	ROOT_SBINDIR=/sbin
	INITRAMFS_DIR=$(PREFIX)/share/initramfs-tools
else
	ROOT_SBINDIR=$(PREFIX)/sbin
	INITRAMFS_DIR=/etc/initramfs-tools
endif

.PHONY: all
all: bcachefs

SRCS=$(shell find . -type f -iname '*.c')
DEPS=$(SRCS:.c=.d)
-include $(DEPS)

OBJS=$(SRCS:.c=.o)
bcachefs: $(OBJS)

# If the version string differs from the last build, update the last version
ifneq ($(VERSION),$(shell cat .version 2>/dev/null))
.PHONY: .version
endif
.version:
	echo '$(VERSION)' > $@

# Rebuild the 'version' command any time the version string changes
cmd_version.o : .version

.PHONY: install
install: INITRAMFS_HOOK=$(INITRAMFS_DIR)/hooks/bcachefs
install: INITRAMFS_SCRIPT=$(INITRAMFS_DIR)/scripts/local-premount/bcachefs
install: bcachefs
	$(INSTALL) -m0755 -D bcachefs      -t $(DESTDIR)$(ROOT_SBINDIR)
	$(INSTALL) -m0755    fsck.bcachefs    $(DESTDIR)$(ROOT_SBINDIR)
	$(INSTALL) -m0755    mkfs.bcachefs    $(DESTDIR)$(ROOT_SBINDIR)
	$(INSTALL) -m0644 -D bcachefs.8    -t $(DESTDIR)$(PREFIX)/share/man/man8/
	$(INSTALL) -m0755 -D initramfs/script $(DESTDIR)$(INITRAMFS_SCRIPT)
	$(INSTALL) -m0755 -D initramfs/hook   $(DESTDIR)$(INITRAMFS_HOOK)
	sed -i '/^# Note: make install replaces/,$$d' $(DESTDIR)$(INITRAMFS_HOOK)
	echo "copy_exec $(ROOT_SBINDIR)/bcachefs /sbin/bcachefs" >> $(DESTDIR)$(INITRAMFS_HOOK)

.PHONY: clean
clean:
	$(RM) bcachefs .version $(OBJS) $(DEPS)

.PHONY: deb
deb: all
# --unsigned-source --unsigned-changes --no-pre-clean --build=binary
# --diff-ignore --tar-ignore
	debuild -us -uc -nc -b -i -I

.PHONE: update-bcachefs-sources
update-bcachefs-sources:
	git rm -rf --ignore-unmatch libbcachefs
	test -d libbcachefs || mkdir libbcachefs
	cp $(LINUX_DIR)/fs/bcachefs/*.[ch] libbcachefs/
	cp $(LINUX_DIR)/include/trace/events/bcachefs.h include/trace/events/
	$(RM) libbcachefs/*.mod.c
	git -C $(LINUX_DIR) rev-parse HEAD | tee .bcachefs_revision
	git add libbcachefs/*.[ch] include/trace/events/bcachefs.h .bcachefs_revision

.PHONE: update-commit-bcachefs-sources
update-commit-bcachefs-sources: update-bcachefs-sources
	git commit -m "Update bcachefs sources to $(shell git -C $(LINUX_DIR) show --oneline --no-patch)"
