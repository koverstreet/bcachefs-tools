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
# The module's version, in its .modinfo: init/module.rs
RUSTFLAGS_mod.o += --cfg bch_version -Zunstable-options --env-set BCACHEFS_VERSION=$(bcachefs-version)
else
$(warning bcachefs: no version in $(src)/version.h - the module will carry none, and we're not looking for a prebuilt one (version.h is staged by bcachefs-tools 'make install_dkms'; a hand-copied kernel fs/bcachefs/ tree lacks it))
endif
endif

# Our options: from the environment (ktest, build.vars), and quota as Kconfig
# defaults it, with CONFIG_QUOTA.
bcachefs-config-cppflags := \
	$(foreach o,DEBUG INJECT_TRANSACTION_RESTARTS PATH_TRACEPOINTS TESTS,$(if $(BCACHEFS_$(o)),-DCONFIG_BCACHEFS_$(o)=1)) \
	$(if $(CONFIG_QUOTA),-DCONFIG_BCACHEFS_QUOTA=1)
endif

# The generated rust/extern.c includes our shim headers relative to fs/
# ("../os/linux_kernel/shim.h"); kbuild only adds this itself when building
# out of the source tree.
subdir-ccflags-y += -I$(src)

# BCACHEFS_WERROR=1: our warnings fatal, without CONFIG_WERROR for the whole
# kernel. Per-file -Wno-* waivers still apply.
ifeq ($(BCACHEFS_WERROR),1)
subdir-ccflags-y += -Werror
endif

obj-$(CONFIG_BCACHEFS_FS)	+= bcachefs.o

bcachefs-y		:=			\
	../os/linux_kernel/shim.o

# A prebuilt module builds nothing, so none of Rust's requirements apply - its
# checks fail the build on a kernel that can't build our Rust (#1133).
ifndef bcachefs-prebuilt
include $(src)/Makefile.rust
endif
