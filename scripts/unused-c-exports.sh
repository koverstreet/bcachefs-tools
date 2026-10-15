#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Rust fns exported to C that no C calls any more:
#
#   scripts/unused-c-exports.sh
#
# in a built tree. Prints each, where it's defined, and exits 1 if there are
# any: a #[c_export] whose last C caller was converted to Rust can be a plain
# Rust fn again.
#
# The exports are the build's own: the records #[c_export] leaves in
# build/cstruct/records.bin, macro-generated ones included, #[cfg] resolved -
# so this checks the configuration that was built. A C use is either
#  - a reference from one of the build's C objects (nm -u), which sees names C
#    makes by token pasting - the recovery passes' bch2_##pass. Only objects
#    whose .c is still there: one left from C since converted is stale.
#  - the name in any C source, comments stripped: kernel-only C, which a
#    userspace build doesn't compile, still counts.
# What both miss - a pasted name in C this build didn't compile - shows as
# unused, and unexporting it fails the C build at the reference.

set -euo pipefail

top=$(git rev-parse --show-toplevel)
cd "$top"
records=build/cstruct/records.bin
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

die() {
    echo "unused-c-exports.sh: $*" >&2
    exit 1
}

[ $# -eq 0 ] || die "usage: scripts/unused-c-exports.sh"
[ -f "$records" ] || die "no $records: run make first"

# Each record's text is the fn as written, the body dropped: #[c_export], its
# other attributes - docs, #[cfg] - then the signature.
grep -aozP '#\[c_export\](?:\s*#\[(?:[^\]"]|"(?:[^"\\]|\\.)*")*\])*\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:extern\s+"C"\s+)?fn\s+\K\w+' \
        "$records" | tr '\0' '\n' | sort -u > "$tmp/exports"
[ -s "$tmp/exports" ] || die "no #[c_export] records in $records"

# Names in C sources, comments stripped - lexed only, no includes or macros
# expanded. The generated headers' prototypes don't count. Lines are spliced
# first: -fpreprocessed takes them as spliced already, and a #define's
# parameters continued past a backslash don't parse.
git ls-files -z '*.c' '*.h' | grep -zv '_gen\.h$' |
    xargs -0 -n 64 sh -c '
        for f; do
            sed -e ":a" -e "/\\\\\$/{N;s/\\\\\\n//;ba" -e "}" "$f" |
                ${CC:-cc} -fpreprocessed -dD -E -P -x c - 2>/dev/null ||
                echo "unused-c-exports.sh: $f: not lexed, its uses not counted" >&2
        done' sh |
    grep -ohE '\b[A-Za-z_][A-Za-z0-9_]*\b' > "$tmp/used" || true

# Names the build's C objects reference
git ls-files '*.c' | sed 's|^|build/|; s|\.c$|.o|' |
    while read -r o; do [ -f "$o" ] && echo "$o"; done |
    xargs -r nm -u | awk '$1 == "U" { print $2 }' >> "$tmp/used"

sort -u "$tmp/used" -o "$tmp/used"
comm -23 "$tmp/exports" "$tmp/used" > "$tmp/unused"

while read -r name; do
    where=$(git grep -n -E "fn $name\b" -- '*.rs' | head -n1 | cut -d: -f1,2)
    echo "${where:-(made by a macro)}: $name: exported to C, and no C calls it"
done < "$tmp/unused"

[ ! -s "$tmp/unused" ]
