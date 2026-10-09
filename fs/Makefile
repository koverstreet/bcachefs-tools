# bcachefs's kbuild Makefile. Built in-tree (scripts/install-to-kernel.sh),
# configured by the kernel's .config like any filesystem; or as an external
# module by DKMS (dkms/Makefile sets BCACHEFS_DKMS), where no .config has our
# options and we set them here.

ifdef BCACHEFS_DKMS
CONFIG_BCACHEFS_FS := m

# If the build farm has a signed bcachefs.ko for this kernel and version,
# fetch-module.sh puts it where kbuild would put ours, and we build nothing.
# (It logs to stderr: stdout is our "hit" or nothing.) Not on clean, which
# reads this file too.
ifneq ($(.DEFAULT_GOAL),__clean)
bcachefs-version := $(shell sed -n 's/.*bcachefs_version "\(.*\)".*/\1/p' $(src)/version.h 2>/dev/null)
ifneq ($(bcachefs-version),)
ifeq ($(shell $(CONFIG_SHELL) $(src)/scripts/fetch-module.sh '$(KERNELRELEASE)' '$(bcachefs-version)' '$(obj)/bcachefs.ko' && echo hit),hit)
CONFIG_BCACHEFS_FS :=
endif
else
$(warning bcachefs: no version in $(src)/version.h - not looking for a prebuilt module (version.h is staged by bcachefs-tools 'make install_dkms'; a hand-copied kernel fs/bcachefs/ tree lacks it))
endif
endif

# Our options: from the environment (ktest, build.vars), and quota as Kconfig
# defaults it, with CONFIG_QUOTA.
bcachefs-config-cppflags := \
	$(foreach o,DEBUG INJECT_TRANSACTION_RESTARTS TESTS,$(if $(BCACHEFS_$(o)),-DCONFIG_BCACHEFS_$(o)=1)) \
	$(if $(CONFIG_QUOTA),-DCONFIG_BCACHEFS_QUOTA=1)
endif

# bindgen and rustc get these too (Makefile.rust): some change struct layouts
# (CONFIG_BCACHEFS_DEBUG adds btree_iter.ip_allocated), and C and Rust
# disagreeing on one is silent memory corruption.
subdir-ccflags-y += $(bcachefs-config-cppflags)

# Our includes are relative to fs/ ("alloc/background.h"), and the
# generated header below is in $(obj); kbuild only adds these itself when
# building out of the source tree.
subdir-ccflags-y += -I$(src) -I$(obj)

# BCACHEFS_WERROR=1: our warnings fatal, without CONFIG_WERROR for the whole
# kernel. Per-file -Wno-* waivers still apply.
ifeq ($(BCACHEFS_WERROR),1)
subdir-ccflags-y += -Werror
endif

# Not fatal even with CONFIG_WERROR (Proxmox: CONFIG_FRAME_WARN=1024): the
# functions that trip it run in kthreads and workqueues, on a full stack. The
# trailing '=' is required: GCC 15 rejects -Wno-error=frame-larger-than.
subdir-ccflags-y += -Wno-error=frame-larger-than=
# "note: xyz changed in GCC X.X"
subdir-ccflags-y += $(call cc-disable-warning, psabi)

# fs/dirent.c's copy of struct getdents_callback64, checked against the
# kernel's (pahole on vmlinux) in external builds; without the check, the
# filldir fastpath is compiled out. In-tree, fs/readdir.c is the same tree.
$(obj)/fs/dirent.o: $(obj)/bch2_getdents_layout.h
$(obj)/bch2_getdents_layout.h: FORCE
	$(Q)$(CONFIG_SHELL) $(src)/scripts/getdents-layout.sh \
		"$(if $(KBUILD_EXTMOD),$(objtree)/vmlinux,)" $@
clean-files += bch2_getdents_layout.h

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

ifdef BCACHEFS_DKMS
ifneq ($(wildcard $(src)/module-version.c),)
	bcachefs-y += module-version.o
else
# A hand-copied tree (field report, 2026-07-23): say so, rather than fail
# with "no rule to make target".
$(warning bcachefs: $(src)/module-version.c missing - module will carry no MODULE_VERSION)
endif
else
	obj-$(CONFIG_MEAN_AND_VARIANCE_UNIT_TEST)   += util/mean_and_variance_test.o
endif

include $(src)/Makefile.rust
