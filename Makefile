include Makefile.common

LIBEXECDIR?=$(PREFIX)/libexec
PKG_CONFIG?=pkg-config
LN=ln
.DEFAULT_GOAL=all

ifeq ($(BUILD_VERBOSE),1)
  CARGO_CLEAN_ARGS = --verbose
else
  CARGO_CLEAN_ARGS = --quiet
endif

# when cross compiling, cargo places the built binary in a different location
ifdef CARGO_BUILD_TARGET
	BUILT_BIN = target/$(CARGO_BUILD_TARGET)/release/bcachefs
else
	BUILT_BIN = target/release/bcachefs
endif

# Prevent recursive expansions of $(CFLAGS) to avoid repeatedly performing
# compile tests
CFLAGS:=$(CFLAGS)

CFLAGS+=-std=gnu11 -O2 -g -MMD -Wall -fPIC			\
	-Wno-pointer-sign					\
	-Wno-deprecated-declarations				\
	-fno-strict-aliasing					\
	-fno-delete-null-pointer-checks				\
	-I. -Ic_src -Ilibbcachefs -Iinclude -Iraid		\
	-D_FILE_OFFSET_BITS=64					\
	-D_GNU_SOURCE						\
	-D_LGPL_SOURCE						\
	-DRCU_MEMBARRIER					\
	-DZSTD_STATIC_LINKING_ONLY				\
	-DFUSE_USE_VERSION=35					\
	-DNO_BCACHEFS_CHARDEV					\
	-DNO_BCACHEFS_FS					\
	-DNO_BCACHEFS_SYSFS					\
	-DCONFIG_UNICODE					\
	-D__SANE_USERSPACE_TYPES__				\
	$(EXTRA_CFLAGS)

# Intenionally not doing the above to $(LDFLAGS) because we rely on
# recursive expansion here (CFLAGS is not yet completely built by this line)
LDFLAGS+=$(CFLAGS) $(EXTRA_LDFLAGS)

ifdef CARGO_TOOLCHAIN_VERSION
  CARGO_TOOLCHAIN = +$(CARGO_TOOLCHAIN_VERSION)
endif

override CARGO_ARGS+=${CARGO_TOOLCHAIN}
CARGO=cargo $(CARGO_ARGS)
CARGO_PROFILE=release
# CARGO_PROFILE=debug

CARGO_BUILD_ARGS=--$(CARGO_PROFILE)
CARGO_BUILD=$(CARGO) build $(CARGO_BUILD_ARGS)

CARGO_CLEAN=$(CARGO) clean $(CARGO_CLEAN_ARGS)

include Makefile.compiler

CFLAGS+=$(call cc-disable-warning, unused-but-set-variable)
CFLAGS+=$(call cc-disable-warning, stringop-overflow)
CFLAGS+=$(call cc-disable-warning, zero-length-bounds)
CFLAGS+=$(call cc-disable-warning, missing-braces)
CFLAGS+=$(call cc-disable-warning, zero-length-array)
CFLAGS+=$(call cc-disable-warning, shift-overflow)
CFLAGS+=$(call cc-disable-warning, enum-conversion)
CFLAGS+=$(call cc-disable-warning, gnu-variable-sized-type-not-at-end)
export RUSTFLAGS:=$(RUSTFLAGS) -C default-linker-libraries

PKGCONFIG_LIBS="blkid uuid liburcu libsodium zlib liblz4 libzstd libudev libkeyutils"
ifdef BCACHEFS_FUSE
	PKGCONFIG_LIBS+="fuse3 >= 3.7"
	CFLAGS+=-DBCACHEFS_FUSE
	RUSTFLAGS+=--cfg feature="fuse"
endif

PKGCONFIG_CFLAGS:=$(shell $(PKG_CONFIG) --cflags $(PKGCONFIG_LIBS))
ifeq (,$(PKGCONFIG_CFLAGS))
    $(error pkg-config error, command: $(PKG_CONFIG) --cflags $(PKGCONFIG_LIBS))
endif
PKGCONFIG_LDLIBS:=$(shell $(PKG_CONFIG) --libs   $(PKGCONFIG_LIBS))
ifeq (,$(PKGCONFIG_LDLIBS))
    $(error pkg-config error, command: $(PKG_CONFIG) --libs $(PKGCONFIG_LIBS))
endif
PKGCONFIG_UDEVDIR:=$(shell $(PKG_CONFIG) --variable=udevdir udev)
ifeq (,$(PKGCONFIG_UDEVDIR))
    $(error pkg-config error, command: $(PKG_CONFIG) --variable=udevdir udev)
endif
PKGCONFIG_UDEVRULESDIR:=$(PKGCONFIG_UDEVDIR)/rules.d

CFLAGS+=$(PKGCONFIG_CFLAGS)
LDLIBS+=$(PKGCONFIG_LDLIBS)
LDLIBS+=-lm -lpthread -lrt -lkeyutils -laio -ldl
LDLIBS+=$(EXTRA_LDLIBS)

ifeq ($(PREFIX),/usr)
	ROOT_SBINDIR?=/sbin
	INITRAMFS_DIR=$(PREFIX)/share/initramfs-tools
else
	ROOT_SBINDIR?=$(PREFIX)/sbin
	INITRAMFS_DIR=/etc/initramfs-tools
endif

.PHONY: all
all: bcachefs initramfs/hook dkms/dkms.conf

.PHONY: debug
debug: CFLAGS+=-Werror -DCONFIG_BCACHEFS_DEBUG=y -DCONFIG_VALGRIND=y
debug: bcachefs

.PHONY: TAGS tags
TAGS:
	ctags -e -R .

tags:
	ctags -R .

SRCS:=$(sort $(shell find . -type f ! -path '*/.*/*' -iname '*.c'))
DEPS:=$(SRCS:.c=.d)
-include $(DEPS)

OBJS:=$(SRCS:.c=.o)

%.o: %.c
	@echo "    [CC]     $@"
	$(Q)$(CC) $(CPPFLAGS) $(CFLAGS) -c -o $@ $<

BCACHEFS_DEPS=libbcachefs.a
RUST_SRCS:=$(shell find src bch_bindgen/src -type f -iname '*.rs')

bcachefs: $(BCACHEFS_DEPS) $(RUST_SRCS)
	$(Q)$(CARGO_BUILD)

libbcachefs.a: $(OBJS)
	@echo "    [AR]     $@"
	$(Q)$(AR) -rc $@ $+

.version: force
	$(Q)echo "$(VERSION)" > .version.new
	$(Q)cmp -s .version.new .version || mv .version.new .version


.PHONY: generate_version
generate_version: .version version.h

# Rebuild the 'version' command any time the version string changes
c_src/cmd_version.o : version.h
c_src/cmd_fusemount.o: version.h

.PHONY: initramfs/hook
initramfs/hook: initramfs/hook.in
	@echo "    [SED]    $@"
	$(Q)sed "s|@ROOT_SBINDIR@|$(ROOT_SBINDIR)|g" initramfs/hook.in > initramfs/hook

.PHONY: install
install: INITRAMFS_HOOK=$(INITRAMFS_DIR)/hooks/bcachefs
install: INITRAMFS_SCRIPT=$(INITRAMFS_DIR)/scripts/local-premount/bcachefs
install: all install_dkms
	$(INSTALL) -m0755 -D $(BUILT_BIN)  -t $(DESTDIR)$(ROOT_SBINDIR)
	$(INSTALL) -m0644 -D bcachefs.8    -t $(DESTDIR)$(PREFIX)/share/man/man8/
	$(INSTALL) -m0755 -D initramfs/hook   $(DESTDIR)$(INITRAMFS_HOOK)
	$(INSTALL) -m0644 -D udev/64-bcachefs.rules -t $(DESTDIR)$(PKGCONFIG_UDEVRULESDIR)/
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mkfs.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/fsck.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mount.bcachefs
ifdef BCACHEFS_FUSE
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mkfs.fuse.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/fsck.fuse.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mount.fuse.bcachefs
endif

.PHONY: install_dkms
install_dkms: version.h
	$(MAKE) -f Makefile.dkms install

.PHONY: clean
clean:
	@echo "Cleaning all"
	$(Q)$(RM) libbcachefs.a c_src/libbcachefs.a .version *.tar.xz $(OBJS) $(DEPS) $(DOCGENERATED)
	$(Q)$(CARGO_CLEAN)
	$(Q)$(RM) -f $(built_scripts)
	$(Q)$(MAKE) -f Makefile.dkms clean

.PHONY: deb
deb: all
	debuild -us -uc -nc -b -i -I

.PHONY: rpm
rpm: clean
	rpmbuild --build-in-place -bb --define "_version $(subst -,_,$(VERSION))" bcachefs-tools.spec

bcachefs-principles-of-operation.pdf: doc/bcachefs-principles-of-operation.tex
	pdflatex doc/bcachefs-principles-of-operation.tex
	pdflatex doc/bcachefs-principles-of-operation.tex

doc: bcachefs-principles-of-operation.pdf

.PHONY: cargo-update-msrv
cargo-update-msrv:
	cargo +nightly generate-lockfile -Zmsrv-policy
	cargo +nightly generate-lockfile --manifest-path bch_bindgen/Cargo.toml -Zmsrv-policy

.PHONY: update-bcachefs-sources
update-bcachefs-sources:
	git rm -rf --ignore-unmatch libbcachefs
	mkdir -p libbcachefs/vendor
	cp -r $(LINUX_DIR)/fs/bcachefs/* libbcachefs/
	git add libbcachefs/*.[ch]
	git add libbcachefs/*/*.[ch]
	git add libbcachefs/Makefile
	git add libbcachefs/Kconfig
	git rm -f libbcachefs/util/mean_and_variance_test.c
	cp $(LINUX_DIR)/include/linux/xxhash.h include/linux/
	git add include/linux/xxhash.h
	cp $(LINUX_DIR)/lib/xxhash.c linux/
	git add linux/xxhash.c
	cp $(LINUX_DIR)/include/linux/list_nulls.h include/linux/
	git add include/linux/list_nulls.h
	cp $(LINUX_DIR)/include/linux/poison.h include/linux/
	git add include/linux/poison.h
	cp $(LINUX_DIR)/include/linux/generic-radix-tree.h include/linux/
	git add include/linux/generic-radix-tree.h
	cp $(LINUX_DIR)/lib/generic-radix-tree.c linux/
	git add linux/generic-radix-tree.c
	cp $(LINUX_DIR)/include/linux/kmemleak.h include/linux/
	git add include/linux/kmemleak.h
	cp $(LINUX_DIR)/lib/math/int_sqrt.c linux/
	git add linux/int_sqrt.c
	cp $(LINUX_DIR)/scripts/Makefile.compiler ./
	git add Makefile.compiler
	$(RM) libbcachefs/*.mod.c
	git -C $(LINUX_DIR) rev-parse HEAD | tee .bcachefs_revision
	git add .bcachefs_revision


.PHONY: update-commit-bcachefs-sources
update-commit-bcachefs-sources: update-bcachefs-sources
	git commit -m "Update bcachefs sources to $(shell git -C $(LINUX_DIR) show --oneline --no-patch)"

SRCTARXZ = bcachefs-tools-$(VERSION).tar.xz
SRCDIR=bcachefs-tools-$(VERSION)

.PHONY: tarball
tarball: $(SRCTARXZ)

$(SRCTARXZ) : .gitcensus
	$(Q)tar --transform "s,^,$(SRCDIR)/," -Jcf $(SRCDIR).tar.xz  \
	    `cat .gitcensus`
	@echo Wrote: $@

.PHONY: .gitcensus
.gitcensus:
	$(Q)if test -d .git; then \
	  git ls-files > .gitcensus; \
	fi
