export BCACHEFS_DKMS=1

# Pick up build options written by `make install_dkms` (e.g.
# BCACHEFS_DEBUG, BCACHEFS_TESTS), and hand them to the recursive kbuild
# so fs/Makefile's matching ifdefs fire. Missing file is fine.
#
# Export what build.vars actually defined, rather than a fixed list:
# `export FOO` on an unset FOO still hands the sub-make FOO="" at
# origin=environment, and any environment value - empty included - makes
# fs/Makefile's `BCACHEFS_RUST ?= auto` a no-op. That pinned every DKMS
# build to the auto-probe and defeated the documented BCACHEFS_RUST=0
# escape hatch (koverstreet/bcachefs#1197). A name that was never set
# isn't in $(.VARIABLES), so it can't be exported empty.
-include $(CURDIR)/build.vars
$(foreach v,$(filter BCACHEFS_%,$(.VARIABLES)),$(eval export $(v)))

ifneq (${KERNELRELEASE},)
ccflags-y := -I$(src)/include
obj-m += src/fs/bcachefs/
else
KDIR ?= /lib/modules/`uname -r`/build

default:
	$(MAKE) -C $(KDIR) M=$$PWD modules

clean:
	$(MAKE) -C $(KDIR) M=$$PWD clean
endif
