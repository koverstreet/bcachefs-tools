
ifdef BCACHEFS_DKMS
	CONFIG_BCACHEFS_FS := m
	# Enable other features here?
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
	fs/acl.o				\
	fs/check.o				\
	fs/check_dir_structure.o		\
	fs/check_extents.o			\
	fs/check_nlinks.o			\
	fs/dirent.o				\
	fs/inode.o				\
	fs/logged_ops.o				\
	fs/namei.o				\
	fs/quota.o				\
	fs/str_hash.o				\
	fs/xattr.o				\
	init/chardev.o				\
	init/dev.o				\
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
	snapshots/check_snapshots.o		\
	snapshots/delete.o			\
	snapshots/snapshot.o			\
	snapshots/subvolume.o			\
	util/clock.o				\
	util/darray.o				\
	util/enumerated_ref.o			\
	util/eytzinger.o			\
	util/fast_list.o			\
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
	bcachefs-y += module-version.o
endif

# Rust support — optional, gated on CONFIG_RUST. The C side owns the module
# entry points; mod.o is a member object compiled by the kernel's Rust rules
# and linked into bcachefs.ko. CONFIG_RUST=y -> bcachefs-y, unset -> dropped.
#
# mod.rs include!s generated bindings (bcachefs.rs + *_gen.rs) plus the
# static-inline C wrappers (extern.c). The userspace tools build produces these
# via cargo/build.rs; the in-kernel/DKMS build has no cargo, so we run the same
# codegen logic (codegen.rs, shared via include!) as a standalone host tool.
#
# DKMS builds should not fail just because the host Rust toolchain does not
# match the kernel's Rust setup. Auto-probe the pieces we need and silently fall
# back to the C-only module when they are not usable. BCACHEFS_RUST=1 forces the
# Rust build (useful for debugging); BCACHEFS_RUST=0 disables it explicitly.
ifdef CONFIG_RUST
# The Rust build shells out to bindgen even outside DKMS; if bindgen isn't
# installed, fall back to the C-only module rather than failing the build
# (reported by feedc0de). DKMS refines this further via the auto-probe below.
bcachefs-rust-y := $(if $(shell command -v $(BINDGEN) 2>/dev/null),y,n)
ifdef BCACHEFS_DKMS
BCACHEFS_RUST ?= auto
ifeq ($(BCACHEFS_RUST),1)
bcachefs-rust-y := y
else ifeq ($(BCACHEFS_RUST),0)
bcachefs-rust-y := n
else
bcachefs-rust-y := $(shell RUSTC='$(RUSTC)' HOSTRUSTC='$(HOSTRUSTC)'	\
	BINDGEN='$(BINDGEN)' CC='$(CC)' KERNEL_SRC='$(srctree)'		\
	KERNEL_OBJ='$(objtree)' CONFIG_RUSTC_VERSION='$(CONFIG_RUSTC_VERSION)' \
	$(CONFIG_SHELL) $(src)/scripts/rust-is-available-dkms.sh)
endif
endif

ifeq ($(bcachefs-rust-y),y)

# Tell the C side that bcachefs's Rust is actually compiled in. This is NOT
# CONFIG_RUST: the DKMS auto-probe (bcachefs-rust-y) can drop Rust even when
# CONFIG_RUST=y, so C that calls into Rust must gate on CONFIG_BCACHEFS_RUST.
subdir-ccflags-y += -DCONFIG_BCACHEFS_RUST=1

# bindgen must see the headers exactly as the C build does, or struct layouts
# diverge silently (e.g. NO_BCACHEFS_FS gates bch_fs.vfs; -fplan9-extensions
# changes struct filename's layout — both come from the kernel's compile flags,
# not just the preprocessor). So we pass the kernel's full compile flags
# ($(LINUXINCLUDE) for includes + CONFIG defines, $(KBUILD_CPPFLAGS) and
# $(KBUILD_CFLAGS) for the rest), filtering out the gcc-only flags libclang
# can't parse — same hack the kernel's own bindgen does. We can't reference its
# bindgen_skip_c_flags (rust/Makefile-local, and we build out-of-tree), so we
# carry a copy below; it may need syncing for newer kernels.
#
# -w because KBUILD_CFLAGS carries -Werror (CONFIG_WERROR) and clang emits piles
# of spurious warnings on kernel headers. NOSTDINC_FLAGS is omitted on purpose:
# it points at the C compiler's resource dir, but bindgen drives libclang, which
# brings its own freestanding headers. -D__BINDGEN__ is the marker the kernel's
# own bindgen pass sets (guards e.g. btf_type_tag); -DRUST_BINDGEN is the
# bcachefs guard that swaps DECLARE_FLEX_ARRAY for a layout-identical s[0].
bcachefs-codegen-skip-cflags := \
	-mno-fp-ret-in-387 -mpreferred-stack-boundary=% \
	-mskip-rax-setup -mgeneral-regs-only -msign-return-address=% \
	-mindirect-branch=thunk-extern -mindirect-branch-register \
	-mfunction-return=thunk-extern -mrecord-mcount -mabi=lp64 \
	-mindirect-branch-cs-prefix -mstack-protector-guard% -mtraceback=no \
	-mno-pointers-to-nested-functions -mno-string \
	-mno-strict-align -mstrict-align -mdirect-extern-access \
	-mexplicit-relocs -mno-check-zero-division \
	-fconserve-stack -falign-jumps=% -falign-loops=% \
	-femit-struct-debug-baseonly -fno-ipa-cp-clone -fno-ipa-sra \
	-fno-partial-inlining -fplugin-arg-arm_ssp_per_task_plugin-% \
	-fno-reorder-blocks -fno-allow-store-data-races -fasan-shadow-offset=% \
	-fzero-call-used-regs=% -fno-stack-clash-protection \
	-fno-inline-functions-called-once -fsanitize=bounds-strict \
	-fstrict-flex-arrays=% -fmin-function-alignment=% \
	-fzero-init-padding-bits=% -mno-fdpic \
	-fdiagnostics-show-context -fdiagnostics-show-context=% \
	--param=% --param asan-% -fno-isolate-erroneous-paths-dereference

# Also drop -funsigned-char: the kernel builds char unsigned, which makes bindgen
# bind `char *` as `*const u8`, but the fs/ Rust (shared with userspace, where
# char is signed) expects `c_char`. Char signedness doesn't affect layout, so
# stripping it just keeps the char bindings consistent across both builds.
bcachefs-codegen-cflags := \
	$(LINUXINCLUDE) $(KBUILD_CPPFLAGS) \
	$(filter-out -funsigned-char $(bcachefs-codegen-skip-cflags),$(KBUILD_CFLAGS)) \
	$(bcachefs-config-cppflags) \
	-I$(src) -include $(srctree)/include/linux/compiler_types.h \
	-w -D__BINDGEN__ -DRUST_BINDGEN

# Types defined outside fs/bcachefs/ are blocklisted and resolved through
# `kernel::bindings` (see mod.rs, cfg(kernel)) — mirroring how the userspace
# build resolves the kernel-compat types through bcachefs-shim. The blocklist is
# the kernel header trees LINUXINCLUDE points at.
bcachefs-codegen-blocklist := \
	$(srctree)/include:$(objtree)/include:$(srctree)/arch/$(SRCARCH)/include:$(objtree)/arch/$(SRCARCH)/include

# The codegen tool: zero-dep, plain host rustc. codegen_main.rs include!s
# codegen.rs, so list the latter as a prereq to rebuild on either change.
quiet_cmd_bch_codegen_host = HOSTRUSTC $@
      cmd_bch_codegen_host = $(HOSTRUSTC) --edition 2021 -O $< -o $@

$(obj)/codegen: $(src)/codegen_main.rs $(src)/codegen.rs FORCE
	$(call if_changed,bch_codegen_host)

# Run it: emits bcachefs.rs, the *_gen.rs files, and extern.c into $(obj)/rust/.
# --target is left to default to the host triple (native DKMS builds only).
#
# The clang flags go after `--`, as separate shell words — exactly as the C
# compile receives them — NOT wrapped in --cflags '$(...)'. A KBUILD flag like
# arm64's -DARM64_ASM_ARCH='"armv8.5-a"' carries embedded quotes: the outer
# single-quote wrapping let the inner ' break out of it, the shell then ate the
# ", and bindgen saw ARM64_ASM_ARCH as the bare token armv8.5-a — desyncing
# clang into an "expected ')'" cascade in the __tlbi asm (aarch64-only,
# deterministic). Unwrapped, each flag's own quoting is self-balanced and parses
# identically to the C build. Flags still appear in the recorded command, so
# if_changed re-runs the codegen on a cflags change.
quiet_cmd_bch_codegen = CODEGEN $(obj)/rust
      cmd_bch_codegen = $(obj)/codegen --src $(src) --out $(obj)/rust \
	--blocklist '$(bcachefs-codegen-blocklist)' \
	--ptr-width $(if $(CONFIG_64BIT),64,32) \
	-- $(bcachefs-codegen-cflags)

# genradix: bindgen emits the generic radix tree itself (kernel::bindings doesn't
# bind it) from a copy of the kernel header — the in-tree <linux/generic-radix-tree.h>
# is blocklisted. Copy it into the *build* dir via a real rule that bcachefs.rs
# depends on, so make orders it. It must NOT be a codegen recipe side-effect into
# $(src): writing a generated file into the (shared) source tree raced under -j —
# a concurrent codegen run's half-written copy got read by bindgen, a truncated
# header that desynced the parse into an "expected ')'" cascade (and it polluted
# $(src) for later userspace builds). The build dir is per-build: no sharing.
$(obj)/rust/generic-radix-tree.h: $(srctree)/include/linux/generic-radix-tree.h
	$(Q)mkdir -p $(@D)
	$(Q)cp $< $@

# Every fs/ header feeds bindgen via codegen-wrapper.h, so a struct/define edit
# must regenerate the bindings — list them as prerequisites. FORCE alone does
# NOT do this: if_changed filters out phony prereqs ($(filter-out $(PHONY),$?)),
# so it only re-runs the codegen on a tool or cflags change, never a header edit
# — silently desyncing the Rust bindings from the C (how a stale extern.c and,
# worse, mismatched struct layouts can slip through). The userspace build.rs gets
# this for free via cargo:rerun-if-changed; this is the kernel/DKMS equivalent.
# (Covers fs/ headers; kernel-header edits still ride in via a cflags/tool change
# or a full rebuild.)
bcachefs-codegen-headers := $(shell find $(src) -name '*.h')

$(obj)/rust/bcachefs.rs: $(obj)/codegen $(obj)/rust/generic-radix-tree.h $(bcachefs-codegen-headers) FORCE
	$(call if_changed,bch_codegen)

# extern.c falls out of the same codegen run. Its wrappers are global functions
# with no prior prototype (bindgen doesn't emit C declarations for them), so
# silence -Wmissing-prototypes for just this generated object.
$(obj)/rust/extern.c: $(obj)/rust/bcachefs.rs ;
CFLAGS_rust/extern.o += -Wno-missing-prototypes -Wno-missing-declarations

# mod.rs keys on cfg(kernel) to source the kernel-compat types from the kernel
# crate (vs bcachefs-shim in userspace); the kernel build doesn't set it on its
# own, so do it here for every Rust object in this dir.
rustflags-y += --cfg kernel

# Vendored Rust crates the kernel doesn't provide: paste (a proc-macro) and
# bitfield (a plain rlib), both dependency-free, under fs/vendor/. We build them
# here, replicating the kernel's own rustc_procmacro / rustc_library commands
# (those live in rust/Makefile and aren't callable out-of-tree), then feed them
# to mod.o via --extern. The flag vars they use ($(rust_common_flags),
# $(rust_flags), $(KBUILD_PROCMACROLDFLAGS)) are all global, so reachable here.
quiet_cmd_bch_procmacro = RUSTC P  $@
      cmd_bch_procmacro = $(RUSTC) $(rust_common_flags) \
	-Clinker-flavor=gcc -Clinker=$(HOSTCC) \
	-Clink-args='$(call escsq,$(KBUILD_PROCMACROLDFLAGS))' \
	--emit=dep-info=$(depfile) --emit=link=$@ --extern proc_macro \
	--crate-type proc-macro --crate-name paste \
	@$(objtree)/include/generated/rustc_cfg $<

$(obj)/libpaste.so: $(src)/vendor/paste/src/lib.rs FORCE
	$(call if_changed_dep,bch_procmacro)

# Generic leaf-rlib build (crate name derived from the object stem), shared by
# the vendored macro_rules crates bitfield and bitflags.
quiet_cmd_bch_library = RUSTC L  $@
      cmd_bch_library = $(RUSTC) $(rust_flags) \
	--emit=dep-info=$(depfile) --emit=obj=$@ \
	--emit=metadata=$(dir $@)lib$(patsubst %.o,%,$(notdir $@)).rmeta \
	--crate-type rlib -L$(objtree)/rust --sysroot=/dev/null \
	-Zunstable-options --crate-name $(patsubst %.o,%,$(notdir $@)) $<

$(obj)/bitfield.o: $(src)/vendor/bitfield/src/lib.rs FORCE
	$(call if_changed_dep,bch_library)

$(obj)/bitflags.o: $(src)/vendor/bitflags/src/lib.rs FORCE
	$(call if_changed_dep,bch_library)

$(obj)/uuid.o: $(src)/vendor/uuid/src/lib.rs FORCE
	$(call if_changed_dep,bch_library)

# mod.o (only — not the crates' own builds) gets them as externs.
RUSTFLAGS_mod.o += --extern paste=$(obj)/libpaste.so --extern bitfield=$(obj)/libbitfield.rmeta --extern bitflags=$(obj)/libbitflags.rmeta --extern uuid=$(obj)/libuuid.rmeta
$(obj)/mod.o: $(obj)/libpaste.so $(obj)/bitfield.o $(obj)/bitflags.o $(obj)/uuid.o

# mod.rs finds the generated files via env!("OUT_DIR"); point it at the gen dir
# (absolute, since include! concatenates it) and make mod.o wait for them.
$(obj)/mod.o: export OUT_DIR := $(abspath $(obj)/rust)
$(obj)/mod.o: $(obj)/rust/bcachefs.rs

targets += codegen rust/bcachefs.rs libpaste.so bitfield.o bitflags.o uuid.o

# mod.o: Rust glue. rust/extern.o: the static-inline wrappers, a normal kernel C
# object. bitfield.o/bitflags.o/uuid.o: the vendored rlibs' objects, linked in.
bcachefs-y += mod.o rust/extern.o bitfield.o bitflags.o uuid.o

else
$(warning bcachefs: skipping optional Rust support for DKMS module; set BCACHEFS_RUST=1 to force)
endif # bcachefs-rust-y

endif # CONFIG_RUST

# Silence "note: xyz changed in GCC X.X" messages
subdir-ccflags-y += $(call cc-disable-warning, psabi)

# kbuild weirdness - sometimes this gets passed automatically, other times we
# need to specify it. no idea why:
subdir-ccflags-y += -I$(src)
