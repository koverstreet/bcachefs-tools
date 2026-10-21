#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# linux-version-cfgs.sh KERNELVERSION SRCDIR
#
# The Rust's kernel version checks: for each cfg(linux_ge = "V") in SRCDIR's
# Rust, --cfg 'linux_ge="V"' if the kernel is V or later.
#
# V is "X.Y" - from X.Y-rc1, where a merge window's changes first appear, as
# C's KERNEL_VERSION(X, Y, 0) - or "X.Y-rcN", from that rc, for a change that
# came after rc1, which C's can't say. Neither can tell a kernel in X.Y's
# merge window from X.(Y-1): it's still versioned that.

kv=$1
src=$2

grep -rhE --include='*.rs' --exclude-dir=vendor 'linux_ge *= *"' "$src" |
	grep -vE '^\s*//' | grep -oE 'linux_ge *= *"[^"]*"' |
	sed -E 's/.*"(.*)"/\1/' | sort -u |
	awk -v kv="$kv" '
	# X.Y[...][-rcN...] as a sortable key; without -rcN, rc is @release.
	function key(v, release,   a, rc) {
		rc = match(v, /-rc[0-9]+/) ? substr(v, RSTART + 3, RLENGTH - 3) : release
		split(v, a, /[.-]/)
		return sprintf("%05d%05d%05d", a[1], a[2], rc)
	}
	BEGIN {
		if (kv !~ /^[0-9]+\.[0-9]+/) {
			print "linux-version-cfgs.sh: kernel version \"" kv "\" isn'\''t X.Y..." > "/dev/stderr"
			exit 1
		}
		k = key(kv, 99999)
	}
	!/^[0-9]+\.[0-9]+(-rc[0-9]+)?$/ {
		print "linux-version-cfgs.sh: linux_ge = \"" $0 "\" isn'\''t X.Y or X.Y-rcN" > "/dev/stderr"
		exit 1
	}
	key($0, 0) <= k { printf "--cfg '\''linux_ge=\"%s\"'\''\n", $0 }'
