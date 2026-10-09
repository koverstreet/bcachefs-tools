
ifdef BCACHEFS_DKMS
CONFIG_BCACHEFS_FS := m
# Enable other features here?

# Prebuilt-module fast path: before compiling, try to download a signed
# bcachefs.ko already built for this exact kernel + bcachefs version. On a hit,
# fetch-module.sh drops it where kbuild would emit the module and we build
# nothing (empty CONFIG_BCACHEFS_FS) — DKMS then collects the fetched .ko as if
# it had been compiled. Any miss (offline, no matching build, wrong vermagic, a
# -dirty source tree the farm never builds) falls through to a normal local
# compile. fetch-module.sh logs only to stderr, so stdout is empty unless a
# module was placed, making the `hit` test unambiguous.
bcachefs-version := $(shell sed -n 's/.*bcachefs_version "\(.*\)".*/\1/p' $(src)/version.h 2>/dev/null)
ifneq ($(bcachefs-version),)
ifeq ($(shell $(CONFIG_SHELL) $(src)/scripts/fetch-module.sh '$(KERNELRELEASE)' '$(bcachefs-version)' '$(obj)/bcachefs.ko' && echo hit),hit)
CONFIG_BCACHEFS_FS :=
endif
else
# No version means no lookup key — say so rather than silently compiling:
# the missing file is the tell that this tree wasn't staged the supported way.
$(warning bcachefs: no version in $(src)/version.h - not looking for a prebuilt module (version.h is staged by bcachefs-tools 'make install_dkms'; a hand-copied kernel fs/bcachefs/ tree lacks it))
endif
endif

# DKMS / external module builds: bcachefs isn't in the host kernel's .config,
# so its CONFIG_BCACHEFS_* options never get set the normal way. ktest forwards
# them via env (BCACHEFS_DEBUG=1 etc., see tests/fs/bcachefs/bcachefs-test-libs.sh);
# we turn them into -D defines.
#
# CRITICAL: collect them in bcachefs-config-cppflags, which is fed to BOTH the C
# compile (subdir-ccflags-y, below) AND bindgen (bcachefs-codegen-cflags, further
# down). Several of these change struct layout — e.g. CONFIG_BCACHEFS_DEBUG
# enables TRACK_PATH_ALLOCATED, adding btree_iter.ip_allocated; CONFIG_BCACHEFS_QUOTA
# adds inode quota fields. If bindgen and the C compile disagree on even one -D,
# struct sizes diverge silently and the C code writes past Rust-allocated structs
# (e.g. the MaybeUninit<btree_iter> in BtreeIter::new) — stack-smashing corruption.
bcachefs-config-cppflags :=
ifdef BCACHEFS_DEBUG
	bcachefs-config-cppflags += -DCONFIG_BCACHEFS_DEBUG=1
endif

# Transaction-restart injection — ktest sets this for the restart-injection variant.
ifdef BCACHEFS_INJECT_TRANSACTION_RESTARTS
	bcachefs-config-cppflags += -DCONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS=1
endif

# The in-kernel unit tests (CONFIG_BCACHEFS_TESTS).
ifdef BCACHEFS_TESTS
	bcachefs-config-cppflags += -DCONFIG_BCACHEFS_TESTS=1
endif

# Make bcachefs's own warnings fatal without CONFIG_WERROR for the whole
# tree: make BCACHEFS_WERROR=1. subdir-ccflags-y scopes it to this directory
# and below, so nothing else the invocation happens to compile is affected;
# per-file CFLAGS_* -Wno-* waivers (e.g. rust/extern.o's) still apply on top.
# For CI that wants exactly this subtree warning-clean, whatever kernel it is
# built into.
ifeq ($(BCACHEFS_WERROR),1)
subdir-ccflags-y += -Werror
endif

# Verify the hand-copied struct getdents_callback64 in fs/dirent.c against
# the target kernel's real layout (fs/scripts/getdents-layout.sh, pahole on
# $(objtree)/vmlinux - present in external builds since kbuild needs it for
# module BTF). In-tree builds compile from the same tree as fs/readdir.c
# and are trusted; external builds are verified or the fastpath is
# compiled out.
subdir-ccflags-y += -I$(obj)
$(obj)/fs/dirent.o: $(obj)/bch2_getdents_layout.h
# The script is not a prerequisite: FORCE + write-if-changed already
# covers script edits, and a source tree that didn't ship it (it's not
# *.[ch]) must degrade to UNVERIFIED, not fail the build.
$(obj)/bch2_getdents_layout.h: FORCE
	$(Q)if [ -f $(src)/scripts/getdents-layout.sh ]; then \
		$(CONFIG_SHELL) $(src)/scripts/getdents-layout.sh \
			"$(if $(KBUILD_EXTMOD),$(objtree)/vmlinux,)" $@; \
	else \
		echo '#define BCH_GETDENTS_LAYOUT_UNVERIFIED 1' > $@.tmp; \
		if cmp -s $@.tmp $@; then rm -f $@.tmp; else mv $@.tmp $@; fi; \
	fi
clean-files += bch2_getdents_layout.h

# Quota is gated by CONFIG_BCACHEFS_QUOTA, which a DKMS build never gets
# from a .config. Mirror the Kconfig's `default y if QUOTA`: the host
# kernel's CONFIG_QUOTA reaches the module build via its auto.conf, so
# key off that. DKMS only: in-tree builds also see CONFIG_QUOTA via
# auto.conf, and keying off it there would force quota on and make
# CONFIG_BCACHEFS_QUOTA=n a no-op.
ifdef BCACHEFS_DKMS
ifdef CONFIG_QUOTA
	bcachefs-config-cppflags += -DCONFIG_BCACHEFS_QUOTA=1
endif
endif

# Feed the collected bcachefs CONFIG defines to the C compile. bindgen gets the
# same set via bcachefs-codegen-cflags, so the two cannot diverge (see the big
# comment above bcachefs-config-cppflags).
subdir-ccflags-y += $(bcachefs-config-cppflags)

obj-$(CONFIG_BCACHEFS_FS)	+= bcachefs.o

bcachefs-y		:=			\
	alloc/accounting.o			\
	alloc/background.o			\
	alloc/backpointers.o			\
	alloc/buckets.o				\
	alloc/check.o				\
	alloc/check_data.o			\
	alloc/discard.o				\
	alloc/disk_groups.o			\
	alloc/foreground.o			\
	alloc/lru.o				\
	alloc/replicas.o			\
	btree/bkey.o				\
	btree/bkey_methods.o			\
	btree/bset.o				\
	btree/cache.o				\
	btree/check.o				\
	btree/commit.o				\
	btree/init.o				\
	btree/interior.o			\
	btree/iter.o				\
	btree/journal_overlay.o			\
	btree/key_cache.o			\
	btree/locking.o				\
	btree/node_scan.o			\
	btree/read.o				\
	btree/sort.o				\
	btree/update.o				\
	btree/write.o				\
	btree/write_buffer.o			\
	data/checksum.o				\
	data/compress.o				\
	data/copygc.o				\
	data/ec/create.o			\
	data/ec/init.o				\
	data/ec/io.o				\
	data/ec/trigger.o			\
	data/extents.o				\
	data/extents_sb.o			\
	data/extent_update.o			\
	data/io_misc.o				\
	data/keylist.o				\
	data/migrate.o				\
	data/move.o				\
	data/nocow_locking.o			\
	data/read.o				\
	data/reconcile/check.o			\
	data/reconcile/trigger.o		\
	data/reconcile/work.o			\
	data/reflink.o				\
	data/update.o				\
	data/write.o				\
	debug/debug.o				\
	debug/sysfs.o				\
	debug/trace.o				\
	errcode.o				\
	fs/check.o				\
	fs/dirent.o				\
	fs/inode_opts.o				\
	fs/logged_ops.o				\
	fs/quota.o				\
	fs/str_hash.o				\
	fs/xattr.o				\
	init/chardev.o				\
	init/dev.o				\
	init/damage.o				\
	init/error.o				\
	init/fs.o				\
	init/progress.o				\
	init/recovery.o				\
	init/passes.o				\
	journal/init.o				\
	journal/journal.o			\
	journal/read.o				\
	journal/reclaim.o			\
	journal/sb.o				\
	journal/seq_blacklist.o			\
	journal/validate.o			\
	journal/write.o				\
	opts.o					\
	sb/clean.o				\
	sb/counters.o				\
	sb/downgrade.o				\
	sb/errors.o				\
	sb/io.o					\
	sb/members.o				\
	snapshots/delete.o			\
	snapshots/snapshot.o			\
	snapshots/subvolume.o			\
	util/clock.o				\
	util/cuckoo.o				\
	util/darray.o				\
	util/enumerated_ref.o			\
	util/eytzinger.o			\
	util/fast_list.o			\
	util/locking.o				\
	util/mean_and_variance.o		\
	util/printbuf.o				\
	util/rcu_pending.o			\
	util/siphash.o				\
	util/six.o				\
	util/time_stats.o			\
	util/thread_with_file.o			\
	util/two_state_shared_lock.o		\
	util/util.o				\
	util/varint.o				\
	vendor/bio_iov_iter.o			\
	vendor/closure.o			\
	vendor/min_heap.o			\
	vfs/fiemap.o				\
	vfs/fs.o				\
	vfs/ioctl.o				\
	vfs/io.o				\
	vfs/buffered.o				\
	vfs/direct.o				\
	vfs/pagecache.o

ifdef CONFIG_DEBUG_FS
	bcachefs-y += debug/async_objs.o
endif

ifndef BCACHEFS_DKMS
	obj-$(CONFIG_MEAN_AND_VARIANCE_UNIT_TEST)   += util/mean_and_variance_test.o
endif

ifdef BCACHEFS_DKMS
ifneq ($(wildcard $(src)/module-version.c),)
	bcachefs-y += module-version.o
else
# Staged by bcachefs-tools 'make install_dkms'; a hand-copied kernel
# fs/bcachefs/ tree lacks it. Degrade rather than fail with a bare
# "no rule to make target" (field report: CRCinAU, 2026-07-23).
$(warning bcachefs: $(src)/module-version.c missing - module will carry no MODULE_VERSION)
endif
endif

include $(src)/Makefile.rust

# Silence "note: xyz changed in GCC X.X" messages
subdir-ccflags-y += $(call cc-disable-warning, psabi)

# Don't let a large stack frame fail the build (e.g. Proxmox ships
# CONFIG_FRAME_WARN=1024, stricter than the usual 2048, + CONFIG_WERROR). The
# functions that trip it — mainly the data-update path — run from kthread /
# workqueue context with a full stack, and we're careful not to blow it, so
# drop the error but keep the warning informational rather than playing
# frame-size whack-a-mole.
#
# The trailing '=' is required, not a typo: -Wframe-larger-than takes a value
# (-Wframe-larger-than=N), so -Wno-error= must name it as 'frame-larger-than=';
# GCC 15 rejects the form without it.
subdir-ccflags-y += -Wno-error=frame-larger-than=

# kbuild weirdness - sometimes this gets passed automatically, other times we
# need to specify it. no idea why:
subdir-ccflags-y += -I$(src)
