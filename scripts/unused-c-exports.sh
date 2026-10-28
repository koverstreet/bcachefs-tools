#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# What the Rust/C boundary still has that nothing crosses, and the Rust that
# nothing names:
#
#   scripts/unused-c-exports.sh
#
# in a built tree. Prints each, where it's defined, and exits 1 if there are
# any:
#  - a Rust fn exported to C, #[c_export], that nothing calls through its C
#    name: whose last C caller was converted to Rust - it can be a plain Rust
#    fn again, or go;
#  - a C fn declared to Rust, c_extern!, that no Rust calls: its declaration
#    goes - and the fn too, if it was there for Rust and C doesn't call it.
#    Not one C calls with no prototype but the declaration's own, in the
#    generated header: that's C's header now;
#  - a pub fn that nothing names: rustc warns of a private fn no one calls,
#    but a pub one is the crate's interface whether or not anything uses it.
#    What's left once its callers moved on - the Rust behind an export the
#    first check removed, a shim from before the conversion. Some are a C
#    API translated whole, its other half not yet needed: that's a call to
#    make, not a deletion.
#
# The first two come from the build's own records, build/cstruct/records.bin
# - macro-generated ones included, #[cfg] resolved - so they check the
# configuration that was built. The third reads the sources: a name is used
# if it's anywhere in our Rust, comments stripped, but its definitions, or
# anywhere in C - so it sees every configuration, and a fn behind a cfg this
# build didn't take is still checked.
#
# A call through a C name is one of:
#  - a reference from one of the build's C objects (nm -u), which sees names C
#    makes by token pasting - bch2_##name. Only objects whose .c is still
#    there: one left from C since converted is stale.
#  - the name in any C source, comments stripped: kernel-only C, which a
#    userspace build doesn't compile, still counts.
#  - Rust's own extern "C" declaration of it - a dispatch table calling
#    exports by their C names, as the recovery passes' does - literal, or
#    pasted in a macro over an x-macro: fn [<bch2_ $name>] in a macro M,
#    invoked as XMACRO!(M), is bch2_ and each of XMACRO's names, its first
#    fields, as a header (generated ones too) defines it.
# What these miss - a name pasted some other way, in C this build didn't
# compile or in Rust - shows as unused, and the change fails the build at the
# reference.

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

# Each #[c_export] record's text is the fn as written, the body dropped:
# #[c_export], its other attributes - docs, #[cfg] - then the signature.
grep -aozP '#\[c_export\](?:\s*#\[(?:[^\]"]|"(?:[^"\\]|\\.)*")*\])*\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:extern\s+"C"\s+)?fn\s+\K\w+' \
        "$records" | tr '\0' '\n' | sort -u > "$tmp/exports"
[ -s "$tmp/exports" ] || die "no #[c_export] records in $records"

# Each c_extern! record is the block as written: its fns' names.
perl -0777 -ne 'while (/c_extern!\s*\{(.*?)\}/sg) { print "$1\n" for $1 =~ /\bfn\s+(\w+)/g }' \
        "$records" | sort -u > "$tmp/externs"

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
    grep -ohE '\b[A-Za-z_][A-Za-z0-9_]*\b' > "$tmp/c_names" || true

# Names the build's C objects reference. if, not &&: a .c with no object -
# kernel-only C - mustn't be the loop's status, or pipefail ends the script
git ls-files '*.c' | sed 's|^|build/|; s|\.c$|.o|' |
    while read -r o; do if [ -f "$o" ]; then echo "$o"; fi; done |
    xargs -r nm -u | awk '$1 == "U" { print $2 }' | sort -u > "$tmp/c_refs"

# Rust's extern "C" declarations: literal names, and pasted ones as
# "PREFIX MACRO" - fn [<PREFIX $x>] in macro_rules! MACRO - with each
# x-macro MACRO is invoked over, "XMACRO MACRO", from XMACRO!(MACRO).
git ls-files -z '*.rs' | grep -zv '^fs/vendor/' | xargs -0 perl -0777 -ne '
    my $src = $_;
    my @macros;
    push @macros, [pos($src), $1] while $src =~ /macro_rules!\s*(\w+)/g;
    while ($src =~ /extern\s+"C"\s*\{/g) {
        my ($start, $depth, $i) = (pos($src), 1, pos($src));
        while ($depth && $i < length $src) {
            my $ch = substr($src, $i++, 1);
            $depth++ if $ch eq "{";
            $depth-- if $ch eq "}";
        }
        my $block = substr($src, $start, $i - $start);
        print "lit $1\n" while $block =~ /\bfn\s+(\w+)/g;
        my $in = (map { $_->[1] } grep { $_->[0] < $start } @macros)[-1];
        while ($block =~ /\bfn\s+\[<(\w+)\s+\$\w+>\]/g) {
            print "paste $1 $in\n" if defined $in;
        }
    }
    print "invoke $1 $2\n" while $src =~ /\b(\w+)!\(\s*(\w+)\s*\)/g;
' > "$tmp/rust_decls" || true

awk '$1 == "lit" { print $2 }' "$tmp/rust_decls" > "$tmp/rust_refs"

# Pasted: each x-macro's first fields, from its #define in any header
awk '$1 == "paste" { print $2, $3 }' "$tmp/rust_decls" | sort -u |
    while read -r prefix macro; do
        awk -v m="$macro" '$1 == "invoke" && $3 == m { print $2 }' "$tmp/rust_decls" | sort -u |
            while read -r xmacro; do
                find . -name '*.h' -not -path './fs/vendor/*' -print0 |
                    xargs -0 perl -0777 -ne '
                        s/\\\n//g;
                        while (/^#define\s+'"$xmacro"'\s*\([^)]*\)(.*)$/mg) {
                            my $body = $1;
                            print "'"$prefix"'$1\n" while $body =~ /\bx\(\s*(\w+)/g;
                        }' 2>/dev/null
            done
    done >> "$tmp/rust_refs"

sort -u "$tmp/rust_refs" -o "$tmp/rust_refs"
sort -u "$tmp/c_names" -o "$tmp/c_names_u"
sort -u "$tmp/c_names_u" "$tmp/c_refs" "$tmp/rust_refs" > "$tmp/used"

found=0

where() {
    git grep -n -E "fn $1\b" -- '*.rs' | head -n1 | cut -d: -f1,2
}

for name in $(comm -23 "$tmp/exports" "$tmp/used"); do
    echo "$(where "$name" || true): $name: exported to C, and nothing calls it by its C name"
    found=1
done

# A c_extern! fn no Rust calls - the name anywhere but a declaration of it.
# Its declaration is also C's prototype, in the generated header: if C calls
# it, and has no prototype of its own, it's C's header now, and stays.
for name in $(cat "$tmp/externs"); do
    # -c, not -q: grep -q's early exit is git grep's SIGPIPE, and pipefail's
    # failure
    uses=$(git grep -hwE "$name" -- '*.rs' ':!fs/vendor' | grep -vcE "\bfn\s+$name\s*\(" || true)
    [ "$uses" -gt 0 ] && continue

    # C's calls: a reference from an object, or the name more than once in C
    # source - once is its definition
    if grep -qx "$name" "$tmp/c_refs" ||
       [ "$(grep -cx "$name" "$tmp/c_names")" -gt 1 ]; then
        git grep -qE "\b$name\s*\(" -- '*.h' ':!*_gen.h' || continue
        why="C calls it, and has its own prototype: the declaration's redundant"
    else
        why="nor does C call it: it's dead"
    fi
    echo "$(git grep -n -E "fn $name\s*\(" -- '*.rs' | head -n1 | cut -d: -f1,2): $name: declared to Rust, and no Rust calls it - $why"
    found=1
done

# A pub fn no one names. Our Rust, comments stripped - a doc naming a fn
# isn't a call - as words, and each name's definitions, to subtract.
git ls-files -z '*.rs' | grep -zv '^fs/vendor/' | xargs -0 perl -0777 -ne '
    s{("(?:[^"\\]|\\.)*")|//[^\n]*|/\*.*?\*/}{defined $1 ? $1 : ""}gse;
    print "$_\n" for /\b([A-Za-z_]\w*)\b/g;
    print "def $_\n" for /\bfn\s+([A-Za-z_]\w*)/g;
' > "$tmp/rust_words"
grep -v '^def ' "$tmp/rust_words" | sort | uniq -c | awk '{ print $2, $1 }' > "$tmp/rust_word_counts"
awk '$1 == "def" { print $2 }' "$tmp/rust_words" | sort | uniq -c | awk '{ print $2, $1 }' > "$tmp/rust_def_counts"

# Not those something outside our Rust reaches by name: C, through an
# export or a symbol name, or the compiler, as a proc macro. An attribute
# runs until its brackets close. Not a name that runs into {, # or $ - a
# template's, a fn a macro or a format string pastes together.
git ls-files -z '*.rs' | grep -zv '^fs/vendor/' | xargs -0 perl -ne '
    if ($open || /^\s*#\[/) {
        $attrs .= $_;
        $open += tr/[// - tr/]//;
        next;
    }
    next if m{^\s*//};
    print "$ARGV:$.:$1\n"
        if /^\s*pub(?:\([^)]*\))?\s+(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)(?![\w{#\$])/ &&
           $attrs !~ /\b(?:proc_macro\w*|c_export|no_mangle|export_name)\b/;
    $attrs = "";
} continue { if (eof) { close ARGV; $attrs = ""; $open = 0 }' > "$tmp/pub_fns"

cat "$tmp/c_names_u" "$tmp/c_refs" > "$tmp/c_all"
awk -F: '
    FILENAME == ARGV[1] { split($0, w, " "); words[w[1]] = w[2]; next }
    FILENAME == ARGV[2] { split($0, d, " "); defs[d[1]] = d[2];  next }
    FILENAME == ARGV[3] { c[$0] = 1; next }
    words[$3] + 0 <= defs[$3] + 0 && !($3 in c) {
        print $1 ":" $2 ": " $3 ": pub, and nothing names it"
    }' "$tmp/rust_word_counts" "$tmp/rust_def_counts" "$tmp/c_all" "$tmp/pub_fns" > "$tmp/unused_pub"
cat "$tmp/unused_pub"
[ -s "$tmp/unused_pub" ] && found=1

[ "$found" = 0 ]
