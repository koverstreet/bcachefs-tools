# bcachefs's kbuild Makefile. Built in-tree (scripts/install-to-kernel.sh),
# configured by the kernel's .config like any filesystem; or as an external
# module (M=, as DKMS builds it), where no .config has our options and we set
# them here.

ifdef KBUILD_EXTMOD
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
bcachefs-prebuilt := y
endif
else
$(warning bcachefs: no version in $(src)/version.h - not looking for a prebuilt module (version.h is staged by bcachefs-tools 'make install_dkms'; a hand-copied kernel fs/bcachefs/ tree lacks it))
endif
endif

# Our options: from the environment (ktest, build.vars), and quota as Kconfig
# defaults it, with CONFIG_QUOTA.
bcachefs-config-cppflags := \
	$(foreach o,DEBUG INJECT_TRANSACTION_RESTARTS PATH_TRACEPOINTS TESTS,$(if $(BCACHEFS_$(o)),-DCONFIG_BCACHEFS_$(o)=1)) \
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

obj-$(CONFIG_BCACHEFS_FS)	+= bcachefs.o

bcachefs-y		:=			\
	data_structures/darray.o		\
	debug/trace.o				\
	errcode.o				\
	init/error.o				\
	init/fs.o				\
	locking/locking.o			\
	util/printbuf.o				\
	util/rcu.o				\
	util/util.o				\
	vendor/bio_iov_iter.o			\
	vendor/bio_map.o			\
	vendor/closure.o

ifdef KBUILD_EXTMOD
ifneq ($(wildcard $(src)/module-version.c),)
	bcachefs-y += module-version.o
else
# A hand-copied tree (field report, 2026-07-23): say so, rather than fail
# with "no rule to make target".
$(warning bcachefs: $(src)/module-version.c missing - module will carry no MODULE_VERSION)
endif
endif

# A prebuilt module builds nothing, so none of Rust's requirements apply - its
# checks fail the build on a kernel that can't build our Rust (#1133).
ifndef bcachefs-prebuilt
include $(src)/Makefile.rust
endif
