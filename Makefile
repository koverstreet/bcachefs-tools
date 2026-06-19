# `:=` (simple expansion) is load-bearing here: VERSION feeds into DKMSDIR,
# the dkms.conf PACKAGE_VERSION, version.h, and the `dkms add/remove` args.
# With recursive `=` the $(shell git describe) re-runs on every $(VERSION)
# expansion — so HEAD moving mid-recipe (e.g. a commit/rebase landing during
# a long `make install_dkms`) can land the six install steps in two
# different /usr/src/bcachefs-vN/ trees. Lock VERSION once at make start.
ifneq ($(wildcard .git),)
VERSION:=$(shell git -c safe.directory=$$PWD -c core.abbrev=12 describe)
else ifneq ($(wildcard .version),)
VERSION:=$(shell cat .version)
else
VERSION:=$(shell cargo metadata --format-version 1 | jq -r '.packages[] | select(.name | test("bcachefs-tools")) | .version')
endif

PREFIX?=/usr/local
LIBEXECDIR?=$(PREFIX)/libexec
DKMSDIR?=/usr/src/bcachefs-$(VERSION)
PKG_CONFIG?=pkg-config
INSTALL=install
LN=ln
.DEFAULT_GOAL=all



ifeq ("$(origin V)", "command line")
  BUILD_VERBOSE = $(V)
endif
ifndef BUILD_VERBOSE
  BUILD_VERBOSE = 0
endif

ifeq ($(BUILD_VERBOSE),1)
  Q =
  CARGO_CLEAN_ARGS = --verbose
else
  Q = @
  CARGO_CLEAN_ARGS = --quiet
endif

# when cross compiling, cargo places the built binary in a different location
ifdef CARGO_BUILD_TARGET
	BUILT_BIN = target/$(CARGO_BUILD_TARGET)/release/bcachefs
else
	BUILT_BIN = target/release/bcachefs
endif

# Persist build-option vars across invocations: `make debug` writes
# build.vars; subsequent `make` / `make install` re-reads it. `make clean`
# wipes it. This makes `make debug; make install` install a debug DKMS.
-include build.vars

# Vars that propagate from the userspace build into the host-side
# `dkms build`. install_dkms writes whichever are set into
# $(DKMSDIR)/build.vars; dkms/Makefile re-includes that file and exports
# the same names so fs/Makefile's ifdefs fire during the module build.
BCACHEFS_DKMS_FORWARD := BCACHEFS_DEBUG \
                        BCACHEFS_TESTS \
                        BCACHEFS_INJECT_TRANSACTION_RESTARTS

# Vars persisted into the *local* build.vars across invocations - a
# superset of BCACHEFS_DKMS_FORWARD that also covers MAKE_DEBUG, the
# userspace-side debug switch (see below).
BCACHEFS_LOCAL_PERSIST := MAKE_DEBUG $(BCACHEFS_DKMS_FORWARD)

# `make debug` is a convenience alias that sets:
#   MAKE_DEBUG:        userspace `bcachefs` binary built with
#                      -DCONFIG_BCACHEFS_DEBUG=y -DCONFIG_VALGRIND=y
#   BCACHEFS_DEBUG:    DKMS kernel-module debug build (forwarded via
#                      $(DKMSDIR)/build.vars and fs/Makefile's ifdef)
#   BCACHEFS_TESTS:    in-kernel unit tests in the DKMS module
# All three are persisted via build.vars for the entire invocation and
# any followups (`make debug; make install`).
#
# MAKE_DEBUG is intentionally distinct from BCACHEFS_DEBUG: ktest sets
# BCACHEFS_DEBUG=1 in env to flip the DKMS module debug build, and must
# not, on its own, also flip the userspace binary into a debug build
# (the userspace debug checks blow CI test timeouts).
#
# BCACHEFS_INJECT_TRANSACTION_RESTARTS is deliberately not included -
# the restart-injection build is only useful for its dedicated test
# variant and shouldn't ride along with everyday debug builds.
ifeq ($(filter debug,$(MAKECMDGOALS)),debug)
    MAKE_DEBUG := 1
    BCACHEFS_DEBUG := 1
    BCACHEFS_TESTS := 1
endif

ifdef MAKE_DEBUG
    EXTRA_CFLAGS += -DCONFIG_BCACHEFS_DEBUG=y -DCONFIG_VALGRIND=y
endif

# Prevent recursive expansions of $(CFLAGS) to avoid repeatedly performing
# compile tests
CFLAGS:=$(CFLAGS)

CFLAGS+=-std=gnu11 -O2 -g -MMD -MP -Wall -fPIC		\
	-Wno-pointer-sign					\
	-Wno-deprecated-declarations				\
	-fno-strict-aliasing					\
	-fno-delete-null-pointer-checks				\
	-I. -Ic_src -Ifs -Iinclude -Iraid		\
	-D_FILE_OFFSET_BITS=64					\
	-D_GNU_SOURCE						\
	-D_LGPL_SOURCE						\
	-DRCU_MEMBARRIER					\
	-DZSTD_STATIC_LINKING_ONLY				\
	-DFUSE_USE_VERSION=35					\
	-DNO_BCACHEFS_CHARDEV					\
	-DNO_BCACHEFS_FS					\
	-DCONFIG_DEBUG_FS					\
	-DCONFIG_UNICODE					\
	-DCONFIG_STACKTRACE					\
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

ifeq ($(CARGO_PROFILE),debug)
	CARGO_BUILD_ARGS=
else
ifeq ($(CARGO_PROFILE),release)
	CARGO_BUILD_ARGS=--$(CARGO_PROFILE)
else
	CARGO_BUILD_ARGS=--profile $(CARGO_PROFILE)
endif
endif
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

PKGCONFIG_LIBS="blkid uuid liburcu libsodium zlib liblz4 libzstd libudev libkeyutils libunwind"
CFLAGS+=-DBCACHEFS_FUSE

# Only query pkg-config for targets that compile or do a full install.
# Targets like install_dkms and clean don't need build dependencies.
NO_PKGCONFIG_TARGETS := install_dkms uninstall clean dkms/dkms.conf generate_version TAGS tags
ifneq ($(filter-out $(NO_PKGCONFIG_TARGETS),$(or $(MAKECMDGOALS),all)),)

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

endif # NEEDS_PKGCONFIG

LDLIBS+=-lm -lpthread -lrt -lkeyutils -laio -ldl
LDLIBS+=$(EXTRA_LDLIBS)

ifeq ($(PREFIX),/usr)
	ROOT_SBINDIR?=/sbin
	INITRAMFS_DIR=$(PREFIX)/share/initramfs-tools
else
	ROOT_SBINDIR?=$(PREFIX)/sbin
	INITRAMFS_DIR=/etc/initramfs-tools
endif

PKGCONFIG_SERVICEDIR:=$(shell $(PKG_CONFIG) --variable=systemdsystemunitdir systemd)
ifeq (,$(PKGCONFIG_SERVICEDIR))
  $(warning skipping systemd integration)
else
systemd_services=bcachefs-wait-devices@.service
built_scripts+=bcachefs-wait-devices@.service

%.service: %.service.in
	@echo "    [SED]    $@"
	$(Q)sed -e "s|@sbindir@|$(ROOT_SBINDIR)|g" < $< > $@

optional_build+=$(systemd_services)
optional_install+=install_systemd
endif	# PKGCONFIG_SERVICEDIR

.PHONY: all
all: bcachefs initramfs/hook dkms/dkms.conf $(optional_build)

.PHONY: debug
debug: write-build-vars bcachefs

.PHONY: write-build-vars
write-build-vars:
	@( :; $(foreach v,$(BCACHEFS_LOCAL_PERSIST),$(if $($(v)),printf '%s := %s\n' '$(v)' '$($(v))';)) ) > build.vars

.PHONY: TAGS tags
TAGS:
	ctags -e -R .

tags:
	ctags -R .

SRCS:=$(sort $(shell find . -type f ! -path '*/.*/*' ! -path './vendor/*' ! -path './debian/*' ! -path './target/*' ! -path './build/*' ! -path './ktest-out/*' -iname '*.c'))
# KUnit test — kernel-only, no userspace equivalent for <kunit/test.h>
SRCS:=$(filter-out %/mean_and_variance_test.c, $(SRCS))
# Strip find(1)'s leading './' so objects land at build/<path>, not build/./<path>.
SRCS:=$(patsubst ./%,%,$(SRCS))

# Objects and depfiles live under build/, never beside the source. fs/*.c is
# shared with the kernel module build, which compiles it -mcmodel=kernel (non-PIC);
# keeping the userspace -fPIC objects out of the source tree means the two builds
# can't contaminate each other, and a `cp -a` of the source stays clean.
DEPS:=$(SRCS:%.c=build/%.d)
-include $(DEPS)

# Old depfiles may mention headers that were removed or renamed. GCC's -MP
# emits empty header rules for new depfiles; this keeps pre-existing stale
# depfiles from failing before the object can be rebuilt.
%.h:
	@:

OBJS:=$(SRCS:%.c=build/%.o)

build/%.o: %.c
	@echo "    [CC]     $@"
	@mkdir -p $(@D)
	$(Q)$(CC) $(CPPFLAGS) $(CFLAGS) -c -o $@ $<

BCACHEFS_DEPS=libbcachefs.a
RUST_SRCS:=$(shell find src bch_bindgen/src -type f -iname '*.rs')

bcachefs: $(BCACHEFS_DEPS) $(RUST_SRCS)
	$(Q)$(CARGO_BUILD)

libbcachefs.a: $(OBJS)
	@echo "    [AR]     $@"
	$(Q)$(AR) -rc $@ $+

.PHONY: force

.version: force
	$(Q)echo "$(VERSION)" > .version.new
	$(Q)cmp -s .version.new .version || mv .version.new .version

VERSION_H:=$(shell echo "#define bcachefs_version \\\"$(VERSION)\\\"")

version.h: force
	$(Q)echo "$(VERSION_H)" > version.h.new
	$(Q)cmp -s version.h.new version.h || mv version.h.new version.h

.PHONY: generate_version
generate_version: .version version.h

# Rebuild the 'version' command any time the version string changes
build/c_src/cmd_version.o : version.h
build/dkms/module-version.o : version.h


.PHONY: dkms/dkms.conf
dkms/dkms.conf: dkms/dkms.conf.in version.h
	@echo "    [SED]    $@"
	$(Q)sed "s|@PACKAGE_VERSION@|$(VERSION)|g" dkms/dkms.conf.in > dkms/dkms.conf

.PHONY: initramfs/hook
initramfs/hook: initramfs/hook.in
	@echo "    [SED]    $@"
	$(Q)sed "s|@ROOT_SBINDIR@|$(ROOT_SBINDIR)|g" initramfs/hook.in > initramfs/hook

.PHONY: install
BASH_COMPLETION_DIR?=$(shell $(PKG_CONFIG) --variable=completionsdir bash-completion 2>/dev/null || echo $(PREFIX)/share/bash-completion/completions)

# True cross compile? (target arch != host arch). CARGO_BUILD_TARGET is
# sometimes set to a triple whose arch matches the host (Debian packaging
# passes --target x86_64-unknown-linux-gnu on x86_64 build hosts) - the
# binary still runs natively in that case. Computed here at top level
# because a variable assignment inside the install recipe terminates it
# in some make versions.
ifdef CARGO_BUILD_TARGET
ifneq ($(firstword $(subst -, ,$(CARGO_BUILD_TARGET))),$(shell uname -m))
SKIP_COMPLETIONS := 1
endif
endif

install: INITRAMFS_HOOK=$(INITRAMFS_DIR)/hooks/bcachefs
install: INITRAMFS_SCRIPT=$(INITRAMFS_DIR)/scripts/local-premount/bcachefs
install: all install_dkms $(optional_install)
	$(INSTALL) -m0755 -D $(BUILT_BIN)  -t $(DESTDIR)$(ROOT_SBINDIR)
	$(INSTALL) -m0644 -D bcachefs.8    -t $(DESTDIR)$(PREFIX)/share/man/man8/
	$(INSTALL) -m0755 -D initramfs/hook   $(DESTDIR)$(INITRAMFS_HOOK)
	$(INSTALL) -m0644 -D udev/64-bcachefs.rules -t $(DESTDIR)$(PKGCONFIG_UDEVRULESDIR)/
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mkfs.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/fsck.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mount.bcachefs
	$(INSTALL) -d $(DESTDIR)$(BASH_COMPLETION_DIR)
# Generating completions runs the built binary; on a true cross compile
# we can't, so skip (SKIP_COMPLETIONS is computed at top level above).
ifdef SKIP_COMPLETIONS
	@echo "    [SKIP]   bash completions (cross compiling for $(CARGO_BUILD_TARGET); can't run target binary on host)"
else
	$(BUILT_BIN) completions bash > $(DESTDIR)$(BASH_COMPLETION_DIR)/bcachefs
endif
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mkfs.fuse.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/fsck.fuse.bcachefs
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/mount.fuse.bcachefs

.PHONY: uninstall
uninstall:
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/bcachefs
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/mkfs.bcachefs
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/fsck.bcachefs
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/mount.bcachefs
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/mkfs.fuse.bcachefs
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/fsck.fuse.bcachefs
	$(RM) $(DESTDIR)$(ROOT_SBINDIR)/mount.fuse.bcachefs
	$(RM) $(DESTDIR)$(PREFIX)/share/man/man8/bcachefs.8
	$(RM) $(DESTDIR)$(BASH_COMPLETION_DIR)/bcachefs
	$(RM) -r $(DESTDIR)$(DKMSDIR)
	$(RM) $(addprefix $(DESTDIR)$(PKGCONFIG_SERVICEDIR)/,$(systemd_services))

.PHONY: install_systemd
install_systemd: $(systemd_services) $(systemd_libexecfiles)
	$(INSTALL) -m0644 -D $(systemd_services) -t $(DESTDIR)$(PKGCONFIG_SERVICEDIR)

.PHONY: install_dkms
install_dkms: dkms/dkms.conf dkms/module-version.c
	$(INSTALL) -m0644 -D dkms/Makefile		-t $(DESTDIR)$(DKMSDIR)
	$(INSTALL) -m0644 -D dkms/dkms.conf		-t $(DESTDIR)$(DKMSDIR)
	$(INSTALL) -m0644 -D fs/Makefile	-t $(DESTDIR)$(DKMSDIR)/src/fs/bcachefs
	(cd fs; find -name '*.[ch]' -exec install -m0644 -D {} $(DESTDIR)$(DKMSDIR)/src/fs/bcachefs/{} \; )
	$(INSTALL) -m0755 -D fs/scripts/getdents-layout.sh -t $(DESTDIR)$(DKMSDIR)/src/fs/bcachefs/scripts
	$(INSTALL) -m0644 -D dkms/module-version.c	-t $(DESTDIR)$(DKMSDIR)/src/fs/bcachefs
	$(INSTALL) -m0644 -D version.h			-t $(DESTDIR)$(DKMSDIR)/src/fs/bcachefs
	@( :; $(foreach v,$(BCACHEFS_DKMS_FORWARD),$(if $($(v)),printf '%s := %s\n' '$(v)' '$($(v))';)) ) > $(DESTDIR)$(DKMSDIR)/build.vars

# dkms sizes its build parallelism from nproc, ignoring the -j passed to
# `make dkms-reload`. In a memory-constrained VM — ktest runs tests in
# VMs with as little as 4G — that OOMs while compiling debug-enabled
# bcachefs. Budget 512M per compile job, and never exceed nproc (dkms's
# own default).
DKMS_PARALLEL_JOBS:=$(shell \
	j=$$(( $$(awk '/^MemTotal:/{print $$2}' /proc/meminfo) / 1024 / 512 )); \
	c=$$(nproc); [ $$c -lt $$j ] && j=$$c; [ $$j -lt 1 ] && j=1; echo $$j)

# Build the kernel module via DKMS and load it. Must run as root
# (sudo make dkms-reload). Idempotent — re-running rebuilds + reloads.
#
# No prerequisites: the recipe runs install_dkms itself, and the DKMS
# module build uses nothing from the userspace `all` target — depending
# on it would just force an unnecessary rebuild of the bcachefs binary.
.PHONY: dkms-reload
dkms-reload:
	@if [ "$$(id -u)" -ne 0 ]; then \
		echo "dkms-reload: must run as root (sudo make $@)"; exit 1; \
	fi
	$(Q)$(MAKE) install_dkms
	@echo "    [DKMS]   bcachefs/$(VERSION)"
	$(Q)dkms remove  -m bcachefs -v $(VERSION) --all 2>/dev/null || true
	$(Q)dkms add     -m bcachefs -v $(VERSION)
	$(Q)dkms build   -m bcachefs -v $(VERSION) -j $(DKMS_PARALLEL_JOBS)
	$(Q)dkms install -m bcachefs -v $(VERSION)
	$(Q)modprobe -r bcachefs 2>/dev/null || true
	$(Q)modprobe bcachefs
	@modinfo bcachefs | grep -E '^(version|filename|srcversion):'

# Interactive incremental rebuild for the edit/build/test loop. DKMS is built for
# packaging, not iteration: dkms-reload wipes and re-copies the build tree
# (`dkms remove --all` + `add`) and keys on a per-commit git-describe VERSION, so
# every cycle is a full rebuild. This skips DKMS and builds in place against a
# persistent tree. The ktest VM is snapshotted fresh each run, so the tree lives
# host-side (default under /ktest-out). `cp -a` preserves source mtimes so kbuild
# only recompiles what changed -- install(1), which dkms-reload uses, stamps every
# file "now" and would defeat that. Pass BCACHEFS_DEBUG=1 BCACHEFS_TESTS=1 (etc.)
# the same way ktest does for dkms-reload.
KDIR			?= /lib/modules/$(shell uname -r)/build
DKMS_INTERACTIVE_DIR	?= /ktest-out/bcachefs-module

.PHONY: dkms-reload-interactive
dkms-reload-interactive: version.h
	@if [ "$$(id -u)" -ne 0 ]; then \
		echo "$@: must run as root"; exit 1; \
	fi
	$(Q)mkdir -p $(DKMS_INTERACTIVE_DIR)/src/fs/bcachefs
	$(Q)cp -a fs/. $(DKMS_INTERACTIVE_DIR)/src/fs/bcachefs/
	$(Q)cp -a dkms/Makefile $(DKMS_INTERACTIVE_DIR)/Makefile
	$(Q)cp -a dkms/module-version.c version.h $(DKMS_INTERACTIVE_DIR)/src/fs/bcachefs/
	$(Q)( :; $(foreach v,$(BCACHEFS_DKMS_FORWARD),$(if $($(v)),printf '%s := %s\n' '$(v)' '$($(v))';)) ) > $(DKMS_INTERACTIVE_DIR)/build.vars
	@echo "    [KBUILD] bcachefs.ko  (incremental @ $(DKMS_INTERACTIVE_DIR))"
	$(Q)$(MAKE) -C $(KDIR) M=$(DKMS_INTERACTIVE_DIR) modules -j$(DKMS_PARALLEL_JOBS)
	# Be the only bcachefs.ko under /ktest-out so gdb's lx-symbols loads THIS
	# build's symbols, not a stale dkms-staged copy (it loads the first match).
	$(Q)find /ktest-out -name bcachefs.ko -not -path '$(DKMS_INTERACTIVE_DIR)/*' -delete 2>/dev/null || true
	$(Q)rmmod bcachefs 2>/dev/null || true
	$(Q)insmod $(DKMS_INTERACTIVE_DIR)/src/fs/bcachefs/bcachefs.ko
	@modinfo bcachefs | grep -E '^(version|filename|srcversion):'

.PHONY: clean
clean:
	@echo "Cleaning all"
	$(Q)$(RM) libbcachefs.a c_src/libbcachefs.a .version dkms/dkms.conf build.vars *.tar.xz $(DOCGENERATED)
	$(Q)$(RM) -r build
	$(Q)$(CARGO_CLEAN)
	$(Q)$(RM) -f $(built_scripts)

.PHONY: deb
deb: all
	debuild -us -uc -nc -b -i -I

.PHONY: rpm
rpm: clean
	rpmbuild --build-in-place -bb --define "_version $(subst -,_,$(VERSION))" bcachefs-tools.spec

bcachefs-principles-of-operation.pdf: doc/bcachefs-principles-of-operation.tex docgen
	pdflatex doc/bcachefs-principles-of-operation.tex
	pdflatex doc/bcachefs-principles-of-operation.tex

.PHONY: docgen
docgen: bcachefs
	target/release/bcachefs _doc_gen
	cargo run -p bch-docgen --release

doc: bcachefs-principles-of-operation.pdf

.PHONY: cargo-update-msrv
cargo-update-msrv:
	cargo +nightly generate-lockfile -Zmsrv-policy
	cargo +nightly generate-lockfile --manifest-path bch_bindgen/Cargo.toml -Zmsrv-policy

# Refresh the small set of kernel files we vendor verbatim (not bcachefs
# source — that lives in fs/ and is developed in-tree now). See
# doc/vendored-kernel-files.md for the why and the list.
.PHONY: update-vendored-kernel-sources
update-vendored-kernel-sources:
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

.PHONY: update-commit-vendored-kernel-sources
update-commit-vendored-kernel-sources: update-vendored-kernel-sources
	git commit -m "Update vendored kernel sources to $(shell git -C $(LINUX_DIR) show --oneline --no-patch)"

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
