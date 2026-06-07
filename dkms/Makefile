export BCACHEFS_DKMS=1

# Pick up build options written by `make install_dkms` (e.g.
# BCACHEFS_DEBUG, BCACHEFS_TESTS). Whichever of these names the included
# file defines flow through to the recursive kbuild and trip the matching
# ifdefs in fs/Makefile. Empty / missing file is fine - unset names
# export as no-ops.
-include $(CURDIR)/build.vars
export BCACHEFS_DEBUG BCACHEFS_TESTS BCACHEFS_INJECT_TRANSACTION_RESTARTS BCACHEFS_RUST

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
