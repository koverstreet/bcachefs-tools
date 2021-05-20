PREFIX?=/usr/local
PKG_CONFIG?=pkg-config
INSTALL=install
PYTEST=pytest-3
CFLAGS+=-std=gnu89 -O2 -g -MMD -Wall				\
	-Wno-pointer-sign					\
	-fno-strict-aliasing					\
	-fno-delete-null-pointer-checks				\
	-I. -Iinclude -Iraid					\
	-D_FILE_OFFSET_BITS=64					\
	-D_GNU_SOURCE						\
	-D_LGPL_SOURCE						\
	-DRCU_MEMBARRIER					\
	-DZSTD_STATIC_LINKING_ONLY				\
	-DFUSE_USE_VERSION=32					\
	-DNO_BCACHEFS_CHARDEV					\
	-DNO_BCACHEFS_FS					\
	-DNO_BCACHEFS_SYSFS					\
	-DVERSION_STRING='"$(VERSION)"'				\
	$(EXTRA_CFLAGS)
LDFLAGS+=$(CFLAGS) $(EXTRA_LDFLAGS)

VERSION?=$(shell git describe --dirty=+ 2>/dev/null || echo v0.1-nogit)

CC_VERSION=$(shell $(CC) -v 2>&1|grep -E '(gcc|clang) version')

ifneq (,$(findstring gcc,$(CC_VERSION)))
	CFLAGS+=-Wno-unused-but-set-variable			\
		-Wno-zero-length-bounds				\
		-Wno-stringop-overflow
endif

ifneq (,$(findstring clang,$(CC_VERSION)))
	CFLAGS+=-Wno-missing-braces				\
		-Wno-zero-length-array				\
		-Wno-shift-overflow				\
		-Wno-enum-conversion
endif

ifdef D
	CFLAGS+=-Werror
	CFLAGS+=-DCONFIG_BCACHEFS_DEBUG=y
endif
	CFLAGS+=-DCONFIG_VALGRIND=y

PKGCONFIG_LIBS="blkid uuid liburcu libsodium zlib liblz4 libzstd libudev"
ifdef BCACHEFS_FUSE
	PKGCONFIG_LIBS+="fuse3 >= 3.7"
	CFLAGS+=-DBCACHEFS_FUSE
endif

PKGCONFIG_CFLAGS:=$(shell $(PKG_CONFIG) --cflags $(PKGCONFIG_LIBS))
ifeq (,$(PKGCONFIG_CFLAGS))
    $(error pkg-config error, command: $(PKG_CONFIG) --cflags $(PKGCONFIG_LIBS))
endif
PKGCONFIG_LDLIBS:=$(shell $(PKG_CONFIG) --libs   $(PKGCONFIG_LIBS))
ifeq (,$(PKGCONFIG_LDLIBS))
    $(error pkg-config error, command: $(PKG_CONFIG) --libs $(PKGCONFIG_LIBS))
endif

CFLAGS+=$(PKGCONFIG_CFLAGS)
LDLIBS+=$(PKGCONFIG_LDLIBS)

LDLIBS+=-lm -lpthread -lrt -lscrypt -lkeyutils -laio -ldl
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

.PHONY: tests
tests: tests/test_helper

.PHONY: check
check: tests bcachefs
	cd tests; $(PYTEST)

.PHONY: TAGS tags
TAGS:
	ctags -e -R .

tags:
	ctags -R .

SRCS=$(shell find . -type f -iname '*.c')
DEPS=$(SRCS:.c=.d)
-include $(DEPS)

OBJS=$(SRCS:.c=.o)
bcachefs: $(filter-out ./tests/%.o, $(OBJS))

MOUNT_SRCS=$(shell find mount/src -type f -iname '*.rs') \
    mount/Cargo.toml mount/Cargo.lock mount/build.rs
libbcachefs_mount.a: $(MOUNT_SRCS)
	LIBBCACHEFS_INCLUDE=$(CURDIR) cargo build --manifest-path mount/Cargo.toml --release
	cp mount/target/release/libbcachefs_mount.a $@

MOUNT_OBJ=$(filter-out ./bcachefs.o ./tests/%.o ./cmd_%.o , $(OBJS))
mount.bcachefs: libbcachefs_mount.a $(MOUNT_OBJ)
	$(CC) -Wl,--gc-sections libbcachefs_mount.a $(MOUNT_OBJ) -o $@ $(LDLIBS)

tests/test_helper: $(filter ./tests/%.o, $(OBJS))

# If the version string differs from the last build, update the last version
ifneq ($(VERSION),$(shell cat .version 2>/dev/null))
.PHONY: .version
endif
.version:
	echo '$(VERSION)' > $@

# Rebuild the 'version' command any time the version string changes
cmd_version.o : .version

doc/bcachefs.5: doc/bcachefs.5.txt
	a2x -f manpage doc/bcachefs.5.txt

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
	$(INSTALL) -m0755 -D mount.bcachefs.sh $(DESTDIR)$(ROOT_SBINDIR)
	sed -i '/^# Note: make install replaces/,$$d' $(DESTDIR)$(INITRAMFS_HOOK)
	echo "copy_exec $(ROOT_SBINDIR)/bcachefs /sbin/bcachefs" >> $(DESTDIR)$(INITRAMFS_HOOK)

.PHONY: clean
clean:
	$(RM) bcachefs mount.bcachefs libbcachefs_mount.a tests/test_helper .version $(OBJS) $(DEPS)
	$(RM) -rf mount/target

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
	git add libbcachefs/*.[ch]
	cp $(LINUX_DIR)/include/trace/events/bcachefs.h include/trace/events/
	git add include/trace/events/bcachefs.h
	cp $(LINUX_DIR)/kernel/locking/six.c linux/
	git add linux/six.c
	cp $(LINUX_DIR)/include/linux/six.h include/linux/
	git add include/linux/six.h
	cp $(LINUX_DIR)/include/linux/list_nulls.h include/linux/
	git add include/linux/list_nulls.h
	cp $(LINUX_DIR)/include/linux/poison.h include/linux/
	git add include/linux/poison.h
	$(RM) libbcachefs/*.mod.c
	git -C $(LINUX_DIR) rev-parse HEAD | tee .bcachefs_revision
	git add .bcachefs_revision

.PHONE: update-commit-bcachefs-sources
update-commit-bcachefs-sources: update-bcachefs-sources
	git commit -m "Update bcachefs sources to $(shell git -C $(LINUX_DIR) show --oneline --no-patch)"
