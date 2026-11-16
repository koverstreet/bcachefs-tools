#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# The kernel's Rust support tree, vendored:
#
#   os/linux_kernel/vendor-update.sh LINUX_TREE REF
#   os/linux_kernel/vendor-update.sh --continue LINUX_TREE REF
#   os/linux_kernel/vendor-update.sh --amend PATCH LINUX_TREE REF
#
# replaces os/linux_kernel/vendor with the kernel's rust/ at REF, less what
# vendor-patches/exclude lists, plus vendor-patches/*.patch applied in order,
# and records REF in vendor/UPSTREAM. scripts/generate_rust_target.rs comes
# from REF too: distro kernel headers don't all ship it.
#
# vendor/ is only ever this script's output: a change to it is a patch in
# vendor-patches, then this.
#
# A patch that doesn't apply to REF is merged, file by file, against what it
# did to the tree HEAD has - HEAD's vendor/UPSTREAM, exclude and patches -
# and rewritten from the result, or deleted if REF already has it. A merge
# that conflicts stops: vendor/ is left with the conflict markers in it, and
# vendor/CONFLICT naming the patch. Resolve them there, then --continue,
# which rewrites that patch from vendor/ and carries on.
#
# To change a patch, edit vendor/ and --amend it: what vendor/ has that
# REF plus the patches doesn't goes into PATCH, and the tree is made again
# from the patches.

set -euo pipefail

die() {
    echo "vendor-update.sh: $*" >&2
    exit 1
}

cont=
amend=
case "${1:-}" in
--continue)	cont=1; shift ;;
--amend)	amend=${2:-}; shift 2 ;;
esac
[ $# -eq 2 ] || die "usage: os/linux_kernel/vendor-update.sh [--continue | --amend PATCH] LINUX_TREE REF"

linux=$1
ref=$2
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
top=$(git -C "$here" rev-parse --show-toplevel)
rel=${here#"$top"/}
patches=$here/vendor-patches
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

commit=$(git -C "$linux" rev-parse --verify -q "$ref^{commit}") ||
    die "$linux has no $ref"

# $1: a commit of $linux; $2: where; $3: exclude list. Missing paths are fine
# with $4 set: HEAD's list is for HEAD's ref, and only has to be close enough
# for merging against.
export_tree() {
    mkdir -p "$2/x"
    git -C "$linux" archive "$1" rust/ scripts/generate_rust_target.rs | tar -x -C "$2/x"
    mv "$2/x/rust" "$2/t"
    mkdir "$2/t/scripts"
    mv "$2/x/scripts/generate_rust_target.rs" "$2/t/scripts/"
    rm -r "$2/x"
    while read -r path; do
        [[ -z $path || $path == \#* ]] && continue
        if [ -e "$2/t/$path" ]; then
            rm -r "${2:?}/t/$path"
        elif [ -z "${4:-}" ]; then
            die "exclude: $1 has no rust/$path"
        fi
    done < "$3"
}

# Whether trees $1 and $2 differ; the diff, in patch form, in $3
trees_differ() {
    local r=0
    rm -rf "$tmp/rw"
    mkdir "$tmp/rw"
    cp -a "$1" "$tmp/rw/a"
    cp -a "$2" "$tmp/rw/b"
    (cd "$tmp/rw" && git diff --no-index --no-prefix a b > "$3") || r=$?
    rm -rf "$tmp/rw"
    [ $r -le 1 ] || die "diffing $1 against $2 failed"
    [ $r -eq 1 ]
}

# The patch's message, then the diff between trees $2 and $3, as patch $1 -
# or no patch, if there's no difference left.
rewrite_patch() {
    local p=$1
    if ! trees_differ "$2" "$3" "$tmp/diff"; then
        rm "$p"
        echo "$(basename "$p"): $ref has it - dropped"
        return
    fi
    { sed '/^---$/q' "$p"; echo; cat "$tmp/diff"; } > "$tmp/new.patch"
    mv "$tmp/new.patch" "$p"
    echo "$(basename "$p"): rewritten for $ref"
}

# Patches before $1 have been rewritten against REF and apply to it as is.
if [ -n "$cont" ]; then
    stopped=$(cat "$here/vendor/CONFLICT" 2>/dev/null) || die "no vendor/CONFLICT to continue from"
    grep -rlq '^<<<<<<< ' "$here/vendor" && die "vendor/ still has conflict markers"
    export_tree "$commit" "$tmp/c" "$patches/exclude"
    for p in "$patches"/*.patch; do
        [ "$(basename "$p")" = "$stopped" ] && break
        git -C "$tmp/c/t" apply --whitespace=nowarn "$p"
    done
    cp -a "$here/vendor" "$tmp/resolved"
    rm "$tmp/resolved/CONFLICT"
    rewrite_patch "$patches/$stopped" "$tmp/c/t" "$tmp/resolved"
    rm -rf "$tmp/c" "$tmp/resolved"
fi

# vendor/'s edits, as a diff against REF plus every patch, go into $amend
if [ -n "$amend" ]; then
    [ -f "$patches/$amend" ] || die "no vendor-patches/$amend"
    [ -e "$here/vendor/CONFLICT" ] && die "vendor/ is mid-conflict: --continue first"
    export_tree "$commit" "$tmp/a" "$patches/exclude"
    for p in "$patches"/*.patch; do
        git -C "$tmp/a/t" apply --whitespace=nowarn "$p"
    done
    cp -a "$here/vendor" "$tmp/edited"
    rm -f "$tmp/edited/UPSTREAM"
    trees_differ "$tmp/a/t" "$tmp/edited" "$tmp/edit" ||
        die "vendor/ has no edits to amend $amend with"
    rm -rf "$tmp/a" "$tmp/edited"

    export_tree "$commit" "$tmp/a" "$patches/exclude"
    for p in "$patches"/*.patch; do
        [ "$(basename "$p")" = "$amend" ] && break
        git -C "$tmp/a/t" apply --whitespace=nowarn "$p"
    done
    cp -a "$tmp/a/t" "$tmp/amend-before"
    git -C "$tmp/a/t" apply --whitespace=nowarn "$patches/$amend"
    git -C "$tmp/a/t" apply --whitespace=nowarn "$tmp/edit" ||
        die "vendor/'s edits don't apply on top of $amend: they need a later patch's changes"
    rewrite_patch "$patches/$amend" "$tmp/amend-before" "$tmp/a/t"
    rm -rf "$tmp/a" "$tmp/amend-before"
fi

# HEAD's tree, to merge against: its ref, its exclude list, its patches
old_ref=$(git -C "$top" show "HEAD:$rel/vendor/UPSTREAM" | cut -d' ' -f2)
mkdir "$tmp/head-patches"
git -C "$top" archive HEAD "$rel/vendor-patches" |
    tar -x -C "$tmp/head-patches" --strip-components=$(($(tr -cd / <<< "$rel" | wc -c) + 2))
git -C "$linux" cat-file -e "$old_ref^{commit}" ||
    die "$linux doesn't have HEAD's upstream, $old_ref"
export_tree "$old_ref" "$tmp/old" "$tmp/head-patches/exclude" missing-ok
export_tree "$commit" "$tmp/new" "$patches/exclude"
old=$tmp/old/t
new=$tmp/new/t

for p in "$patches"/*.patch; do
    name=$(basename "$p")
    head_p=$tmp/head-patches/$name

    if git -C "$new" apply --whitespace=nowarn --check "$p" 2>/dev/null; then
        [ -f "$head_p" ] && git -C "$old" apply --whitespace=nowarn "$head_p"
        git -C "$new" apply --whitespace=nowarn "$p"
        continue
    fi
    [ -f "$head_p" ] || die "$name doesn't apply to $ref, and HEAD has no $name to merge against"

    rm -rf "$tmp/before" "$tmp/new-before"
    cp -a "$old" "$tmp/before"
    cp -a "$new" "$tmp/new-before"
    git -C "$old" apply --whitespace=nowarn "$head_p" ||
        die "HEAD's $name doesn't apply to HEAD's upstream, $old_ref"

    conflicts=
    while read -r _ _ path; do
        [ -f "$new/$path" ] || die "$name: $ref has no rust/$path"
        git merge-file -q -L "$ref" -L "$old_ref" -L "$name" \
            "$new/$path" "$tmp/before/$path" "$old/$path" ||
            conflicts+=" $path"
    done < <(git apply --numstat "$head_p")

    if [ -n "$conflicts" ]; then
        rm -rf "${here:?}/vendor"
        mv "$new" "$here/vendor"
        echo "$name" > "$here/vendor/CONFLICT"
        die "$name conflicts with $ref in:$conflicts
resolve them in $rel/vendor, then: $rel/vendor-update.sh --continue $linux $ref"
    fi
    rewrite_patch "$p" "$tmp/new-before" "$new"
done

echo "$ref $commit" > "$new/UPSTREAM"
rm -rf "${here:?}/vendor"
mv "$new" "$here/vendor"
echo "$rel/vendor: $ref ($commit) + $(ls "$patches"/*.patch | wc -l) patches"
