#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# rust-gc-link.sh OUT WORKDIR OBJ...
#
# Link bcachefs's Rust objects into one, leaving out the code nothing reaches:
# ld -r --gc-sections. kbuild links a module without section GC, and rustc
# compiles every function of a crate that a dependent could call - all of
# core's and the vendored crates' - so without this, all of it is in the
# module, with whatever it references: float intrinsics the kernel doesn't
# have, for one.
#
# Roots: what C can call - the defined globals that aren't v0-mangled (_R*),
# which is the #[no_mangle] set. And every allocated section that isn't code
# or data is kept whole: metadata nothing references, which the GC would
# otherwise drop silently - .modinfo, __param, __bug_table, __jump_table,
# __ex_table, .altinstructions, and whatever else turns up. Taken from the
# objects themselves, so a new one is kept, not lost.
#
# ld.bfd keeps the undefined symbols of what it dropped (ld.lld doesn't), and
# modpost takes those for symbols the module needs: they're stripped - only
# those no relocation names, which objcopy enforces, refusing the rest.
#
# Takes LD (with its flags), NM, OBJCOPY and READELF from the environment.

set -e

out=$1
work=$2
shift 2

lds=$work/gc.lds

{
	echo 'SECTIONS {'
	$READELF -SW "$@" | awk '
		/^ *\[ *[0-9]+\]/ {
			sub(/^ *\[ *[0-9]+\] */, "")
			# $7 is the flags column - absent where there are none,
			# when it is the next, numeric, column
			if ($7 ~ /^[A-Za-z]+$/ && $7 ~ /A/ &&
			    $1 !~ /^\.(text|rodata|data|bss)(\.|$)/)
				print $1
		}' | sort -u | while read -r s; do
		echo "	$s 0 : { KEEP(*($s)) }"
	done
	echo '}'

	$NM --defined-only --extern-only "$@" |
		awk 'NF == 3 && $3 !~ /^_R/ { print "EXTERN(" $3 ")" }' | sort -u
} > "$lds"

$LD -r --gc-sections -T "$lds" -o "$out" "$@"

$NM --undefined-only "$out" | awk '{ print $NF }' | sort -u > "$work/gc.undef"
$READELF -rW "$out" | awk '$3 ~ /^R_/ && NF >= 5 { print $5 }' | sort -u > "$work/gc.relocated"
comm -23 "$work/gc.undef" "$work/gc.relocated" > "$work/gc.unneeded"

if [ -s "$work/gc.unneeded" ]; then
	$OBJCOPY --strip-symbols="$work/gc.unneeded" "$out"
fi
