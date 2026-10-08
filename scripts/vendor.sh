#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Crates vendored from a git repository, at a commit:
#
#   scripts/vendor.sh NAME [COMMIT]
#
# exports NAME from its repository again - at COMMIT, if given, which is
# recorded. fs/vendor/git-vendored lists them, a line each: NAME URL COMMIT.
#
# fs/vendor/NAME is exactly what `git archive COMMIT` gives - nothing added,
# removed or edited. A change to a vendored crate is a commit in its
# repository, then this - never an edit under fs/vendor.

set -euo pipefail

top=$(git rev-parse --show-toplevel)
manifest=$top/fs/vendor/git-vendored
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

die() {
    echo "vendor.sh: $*" >&2
    exit 1
}

[ $# -ge 1 ] && [ $# -le 2 ] && [[ $1 != -* ]] ||
    die "usage: scripts/vendor.sh NAME [COMMIT]"

name=$1
read -r url commit < <(awk -v n="$name" '$1 == n { print $2, $3; found = 1 } END { exit !found }' "$manifest") ||
    die "$name isn't in ${manifest#$top/}"
commit=${2:-$commit}

repo=$tmp/repo
git init -q --bare "$repo"
# By hash if the server allows it, else everything it has
git -C "$repo" fetch -q "$url" "$commit" 2>/dev/null ||
    git -C "$repo" fetch -q "$url" '+refs/*:refs/*' ||
    die "fetching $url failed"
git -C "$repo" cat-file -e "$commit^{commit}" 2>/dev/null ||
    die "$url has no commit $commit"

mkdir "$tmp/$name"
git -C "$repo" archive "$commit" | tar -x -C "$tmp/$name"
rm -rf "${top:?}/fs/vendor/$name"
mv "$tmp/$name" "$top/fs/vendor/$name"

awk -v n="$name" -v c="$commit" '$1 == n { $3 = c } { print }' "$manifest" > "$tmp/manifest"
cat "$tmp/manifest" > "$manifest"
echo "fs/vendor/$name: $commit of $url"
