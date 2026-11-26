# `:=` (simple expansion) is load-bearing here: VERSION feeds into DKMSDIR,
# the dkms.conf PACKAGE_VERSION, BCACHEFS_VERSION, and the `dkms add/remove` args.
# With recursive `=` the $(shell git describe) re-runs on every $(VERSION)
# expansion — so HEAD moving mid-recipe (e.g. a commit/rebase landing during
# a long `make install_dkms`) can land the install steps in two
# different /usr/src/bcachefs-vN/ trees. Lock VERSION once at make start.
#
# --dirty: a modified working tree describes as <tag>-dirty, which both marks
# the build non-pristine and gates the DKMS prebuilt-module fetch — the farm
# only builds clean refs, so a -dirty version never matches a published module
# and we always build locally from the modified source (see fs/Makefile).
ifneq ($(wildcard .git),)
VERSION:=$(shell git -c safe.directory=$$PWD -c core.abbrev=12 describe --dirty)
else
VERSION:=$(shell cargo metadata --format-version 1 | jq -r '.packages[] | select(.name | test("bcachefs-tools")) | .version')
endif

# `bcachefs version`'s, by env!, which cargo tracks; and in build.vars, the module's
export BCACHEFS_VERSION:=$(VERSION)

PREFIX?=/usr/local
DKMSDIR?=/usr/src/bcachefs-$(VERSION)
PKG_CONFIG?=pkg-config
INSTALL=install
LN=ln
.DEFAULT_GOAL=all

ifeq ($(origin V),command line)
  BUILD_VERBOSE := $(V)
endif
Q := $(if $(filter 1,$(BUILD_VERBOSE)),,@)

# when cross compiling, cargo places the built binary in a different location
BUILT_BIN := target/$(if $(CARGO_BUILD_TARGET),$(CARGO_BUILD_TARGET)/)release/bcachefs

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
                        BCACHEFS_INJECT_TRANSACTION_RESTARTS \
                        BCACHEFS_PATH_TRACEPOINTS \
                        BCACHEFS_WERROR

# Vars persisted into the *local* build.vars across invocations - a
# superset of BCACHEFS_DKMS_FORWARD that also covers MAKE_DEBUG, the
# userspace-side debug switch (see below).
BCACHEFS_LOCAL_PERSIST := MAKE_DEBUG $(BCACHEFS_DKMS_FORWARD)

# Write the variables in $(1) that are set to build.vars file $(2).
bch-build-vars = ( :; $(foreach v,$(1),$(if $($(v)),printf '%s := %s\n' '$(v)' '$($(v))';)) ) > $(2)

# `make debug` is a convenience alias that sets:
#   MAKE_DEBUG:        userspace `bcachefs` binary built with
#                      CONFIG_BCACHEFS_DEBUG
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
    BCACHEFS_CONFIG += CONFIG_BCACHEFS_DEBUG
endif

# CONFIG_* to set, or -CONFIG_* to unset, as the Rust's cfgs:
# fs/build_config.rs.
export BCACHEFS_CONFIG

CARGO=cargo $(CARGO_ARGS)

# Cold paths out of line, as the module's are: fs/Makefile.rust
export RUSTFLAGS:=$(RUSTFLAGS) -C default-linker-libraries -C llvm-args=-hot-cold-split=true

PKGCONFIG_LIBS="blkid liburcu zlib liblz4 libzstd"

# Only query pkg-config for targets that compile or do a full install.
# Targets like install_dkms and clean don't need build dependencies.
NO_PKGCONFIG_TARGETS := install_dkms uninstall clean dkms/dkms.conf
ifneq ($(filter-out $(NO_PKGCONFIG_TARGETS),$(or $(MAKECMDGOALS),all)),)

# The libraries cargo links: missing, said here rather than mid-build
ifeq (,$(shell $(PKG_CONFIG) --exists $(PKGCONFIG_LIBS) && echo y))
    $(error pkg-config error, command: $(PKG_CONFIG) --exists $(PKGCONFIG_LIBS))
endif
PKGCONFIG_UDEVDIR:=$(shell $(PKG_CONFIG) --variable=udevdir udev)
ifeq (,$(PKGCONFIG_UDEVDIR))
    $(error pkg-config error, command: $(PKG_CONFIG) --variable=udevdir udev)
endif
PKGCONFIG_UDEVRULESDIR:=$(PKGCONFIG_UDEVDIR)/rules.d

endif # NEEDS_PKGCONFIG

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

optional_build+=$(systemd_services)
optional_install+=install_systemd

# The generator raises the mount timeout for bcachefs entries in fstab -
# see src/commands/generator.rs for why that can't be done from the mount
# helper. Older systemd doesn't advertise the variable; skip rather than
# guess a path, since a generator in the wrong directory is never run and
# nothing says so.
PKGCONFIG_GENERATORDIR:=$(shell $(PKG_CONFIG) --variable=systemdsystemgeneratordir systemd)
ifeq (,$(PKGCONFIG_GENERATORDIR))
  $(warning skipping systemd generator)
else
optional_install+=install_systemd_generator
endif	# PKGCONFIG_GENERATORDIR
endif	# PKGCONFIG_SERVICEDIR

built_scripts+=udev/64-bcachefs.rules initramfs/hook dkms/dkms.conf

# dracut has no bcachefs support of its own; without this module a dracut
# initramfs has no mount helper, no udev rules and no generator.
built_scripts+=dracut/90bcachefs/module-setup.sh
PKGCONFIG_DRACUTMODULESDIR:=$(shell $(PKG_CONFIG) --variable=dracutmodulesdir dracut 2>/dev/null)
ifneq (,$(PKGCONFIG_DRACUTMODULESDIR))
optional_build+=dracut/90bcachefs/module-setup.sh
optional_install+=install_dracut
endif	# PKGCONFIG_DRACUTMODULESDIR

.PHONY: all
all: bcachefs initramfs/hook dkms/dkms.conf udev/64-bcachefs.rules $(optional_build)

.PHONY: debug
debug: write-build-vars bcachefs

.PHONY: write-build-vars
write-build-vars:
	@$(call bch-build-vars,$(BCACHEFS_LOCAL_PERSIST),build.vars)

# Always cargo's to decide: it tracks every source, build script and env!.
# make's mtimes can't - a binary newer than its sources (crane's deps-only
# build, sources at mtime 1 in the nix store) was taken as built, and the
# dummy installed.
bcachefs: force
	$(Q)$(CARGO) build --release

# The cargo test suite, as the nix check runs it (crane-build.nix).
.PHONY: check
check:
	$(Q)$(CARGO) test --release -- --nocapture

.PHONY: force

# Each from its .in, with where things are installed: absolute paths - udev,
# for one, looks in /usr/lib/udev for a bare name, not $$PATH.
.PHONY: $(built_scripts)
$(built_scripts): %: %.in
	@echo "    [SED]    $@"
	$(Q)sed -e "s|@ROOT_SBINDIR@|$(ROOT_SBINDIR)|g" \
		-e "s|@UDEVRULESDIR@|$(PKGCONFIG_UDEVRULESDIR)|g" \
		-e "s|@GENERATORDIR@|$(PKGCONFIG_GENERATORDIR)|g" \
		-e "s|@PACKAGE_VERSION@|$(VERSION)|g" $< > $@

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

# The names it's run by: src/bcachefs.rs dispatches on argv[0]
SBIN_LINKS := $(foreach t,mkfs fsck mount,$(foreach f,bcachefs fuse.bcachefs fuseblk.bcachefs,$(t).$(f)))

install: INITRAMFS_HOOK=$(INITRAMFS_DIR)/hooks/bcachefs
install: all install_dkms $(optional_install)
	$(INSTALL) -m0755 -D $(BUILT_BIN)  -t $(DESTDIR)$(ROOT_SBINDIR)
	$(INSTALL) -m0644 -D bcachefs.8    -t $(DESTDIR)$(PREFIX)/share/man/man8/
	$(INSTALL) -m0755 -D initramfs/hook   $(DESTDIR)$(INITRAMFS_HOOK)
	$(INSTALL) -m0644 -D udev/64-bcachefs.rules -t $(DESTDIR)$(PKGCONFIG_UDEVRULESDIR)/
	$(foreach l,$(SBIN_LINKS),$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs $(DESTDIR)$(ROOT_SBINDIR)/$(l);)
	$(INSTALL) -d $(DESTDIR)$(BASH_COMPLETION_DIR)
# Generating completions runs the built binary; on a true cross compile
# we can't, so skip (SKIP_COMPLETIONS is computed at top level above).
ifdef SKIP_COMPLETIONS
	@echo "    [SKIP]   bash completions (cross compiling for $(CARGO_BUILD_TARGET); can't run target binary on host)"
else
	$(BUILT_BIN) completions bash > $(DESTDIR)$(BASH_COMPLETION_DIR)/bcachefs
endif

.PHONY: uninstall
uninstall:
	$(RM) $(addprefix $(DESTDIR)$(ROOT_SBINDIR)/,bcachefs $(SBIN_LINKS))
	$(RM) $(DESTDIR)$(PREFIX)/share/man/man8/bcachefs.8
	$(RM) $(DESTDIR)$(BASH_COMPLETION_DIR)/bcachefs
	$(RM) -r $(DESTDIR)$(DKMSDIR)
	$(RM) $(addprefix $(DESTDIR)$(PKGCONFIG_SERVICEDIR)/,$(systemd_services))

.PHONY: install_systemd
install_systemd: $(systemd_services)
	$(INSTALL) -m0644 -D $(systemd_services) -t $(DESTDIR)$(PKGCONFIG_SERVICEDIR)

# A symlink, dispatched on argv[0] like mount.bcachefs - see the "generator"
# arm in src/bcachefs.rs, which has to come before the "mount" one because this
# name contains it.
# After install_systemd: under make -j the two would create
# $(DESTDIR)/usr/lib/systemd at once, and install -D can lose that race.
.PHONY: install_systemd_generator
install_systemd_generator: install_systemd
	$(INSTALL) -d $(DESTDIR)$(PKGCONFIG_GENERATORDIR)
	$(LN) -sfr $(DESTDIR)$(ROOT_SBINDIR)/bcachefs \
		$(DESTDIR)$(PKGCONFIG_GENERATORDIR)/bcachefs-mount-generator

.PHONY: install_dracut
install_dracut: dracut/90bcachefs/module-setup.sh
	$(INSTALL) -m0755 -D $< -t $(DESTDIR)$(PKGCONFIG_DRACUTMODULESDIR)/90bcachefs

# The module's source tree, in $(1), for DKMS or dkms-reload-interactive:
# dkms/install.rs. In a target directory of its own: dkms-reload runs as root,
# and root's files in target/debug would fail the user's next build.
bch-stage-module = $(Q)$(CARGO) run -q -p bcachefs-dkms --target-dir target/dkms -- $(1) \
	$(foreach v,BCACHEFS_VERSION $(BCACHEFS_DKMS_FORWARD),$(if $($(v)),'$(v)=$($(v))'))

.PHONY: install_dkms
install_dkms: dkms/dkms.conf
	$(call bch-stage-module,$(DESTDIR)$(DKMSDIR))
	$(INSTALL) -m0644 dkms/dkms.conf $(DESTDIR)$(DKMSDIR)

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

# Incremental rebuild for the edit/build/test loop. dkms-reload rebuilds from
# scratch every time (`dkms remove --all` + `add`, keyed on a per-commit
# VERSION); this skips DKMS and builds against a persistent tree, host-side
# (the ktest VM is fresh each run). Options as for dkms-reload:
# BCACHEFS_DEBUG=1 BCACHEFS_TESTS=1 etc.
KDIR			?= /lib/modules/$(shell uname -r)/build
DKMS_INTERACTIVE_DIR	?= /ktest-out/bcachefs-module

.PHONY: dkms-reload-interactive
dkms-reload-interactive:
	@if [ "$$(id -u)" -ne 0 ]; then \
		echo "$@: must run as root"; exit 1; \
	fi
	$(call bch-stage-module,$(DKMS_INTERACTIVE_DIR))
	@echo "    [KBUILD] bcachefs.ko  (incremental @ $(DKMS_INTERACTIVE_DIR))"
	$(Q)$(MAKE) -C $(KDIR) M=$(DKMS_INTERACTIVE_DIR) modules -j$(DKMS_PARALLEL_JOBS)
	# Be the only bcachefs.ko under /ktest-out so gdb's lx-symbols loads THIS
	# build's symbols, not a stale dkms-staged copy (it loads the first match).
	$(Q)find /ktest-out -name bcachefs.ko -not -path '$(DKMS_INTERACTIVE_DIR)/*' -delete 2>/dev/null || true
	$(Q)rmmod bcachefs 2>/dev/null || true
	$(Q)insmod $(DKMS_INTERACTIVE_DIR)/src/fs/bcachefs/bcachefs.ko
	# modinfo by path: the module is insmod'd directly, not installed into the
	# module tree, so a by-name lookup wouldn't find it.
	@modinfo $(DKMS_INTERACTIVE_DIR)/src/fs/bcachefs/bcachefs.ko | grep -E '^(version|filename|srcversion):'

.PHONY: clean
clean:
	@echo "Cleaning all"
	$(Q)$(RM) build.vars $(DOCGENERATED) $(built_scripts)
	$(Q)$(CARGO) clean $(if $(Q),--quiet)

.PHONY: deb
deb: all
	debuild -us -uc -nc -b -i -I

.PHONY: rpm
rpm: clean
	rpmbuild --build-in-place -bb --define "_version $(subst -,_,$(VERSION))" bcachefs-tools.spec

DOCGENERATED=doc/generated/build-version.tex

doc/generated/build-version.tex: force
	$(Q)mkdir -p doc/generated
	$(Q)printf '\\renewcommand{\\bchdocversion}{%s}\n' '$(VERSION)' > $@

bcachefs-principles-of-operation.pdf: doc/bcachefs-principles-of-operation.tex docgen doc/generated/build-version.tex
	pdflatex doc/bcachefs-principles-of-operation.tex
	pdflatex doc/bcachefs-principles-of-operation.tex

.PHONY: docgen
docgen: bcachefs
	$(BUILT_BIN) _doc_gen
	cargo run -p bch-docgen --release

doc: bcachefs-principles-of-operation.pdf

.PHONY: cargo-update-msrv
cargo-update-msrv:
	cargo +nightly generate-lockfile -Zmsrv-policy
	cargo +nightly generate-lockfile --manifest-path fs/Cargo.toml -Zmsrv-policy
