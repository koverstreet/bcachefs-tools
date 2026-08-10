#!/bin/bash
# Build source-only Debian package for bcachefs-tools
#
# Runs inside a podman container (debian:trixie-slim).
# Produces: .dsc + .orig.tar.xz + .debian.tar.xz + .changes in $RESULT_DIR
#
# Usage: build-source.sh COMMIT GIT_REPO RESULT_DIR RUST_VERSION [TAG]
#
# TAG is what we are building this commit *as*, and it is an input, not
# something to be discovered here. Non-empty means a release: the version is
# the tag verbatim. Empty means a snapshot, definitively.
#
# This script used to ask `git describe --exact-match` itself. That question has
# a different answer depending on when you ask it: on 2026-08-09 the clone at
# 16:07:46 said "no tag" and the publish step at 16:50:46 said "v1.39.1", nine
# seconds of push latency apart. The snapshot version was already baked into the
# source package by then, and all twelve binary builds descend from it, so the
# release went out as 1:1.39.0~20260809230746 - which sorts *below* 1:1.39.0 and
# was therefore never offered to anyone. The orchestrator knows the answer
# authoritatively; it passes it in.

set -euo pipefail

COMMIT="$1"
GIT_REPO="$2"
RESULT_DIR="$3"
RUST_VERSION="$4"
TAG="${5:-}"

CACHE_DIR="${CACHE_DIR:-/home/aptbcachefsorg/package-ci/cache}"
CONTAINER="ci-source-$$"
IMAGE="debian:trixie-slim"

mkdir -p "$RESULT_DIR" "$CACHE_DIR/rustup" "$CACHE_DIR/cargo" "$CACHE_DIR/apt"

cleanup() {
    podman rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building source package for $COMMIT ==="

# Clone the repo at the target commit into a temp dir
WORK_DIR=$(mktemp -d)
trap 'cleanup; rm -rf "$WORK_DIR"' EXIT

git clone --tags "$GIT_REPO" "$WORK_DIR/bcachefs-tools"
cd "$WORK_DIR/bcachefs-tools"
git checkout "$COMMIT"

# Version comes from what we were told to build this as, never from asking git
# whether a tag happens to be visible right now. See the header.
if [ -n "$TAG" ]; then
    # Release: the tag verbatim, minus the leading 'v'.
    NEW_VERSION="${TAG#v}"
else
    # Snapshot: base version from git describe or .version + snapshot suffix
    RAW_VERSION=$(git describe --tags --abbrev=0 2>/dev/null | sed 's/^v//' || cat .version 2>/dev/null | sed 's/^v//' || echo "0.0.0")
    SHORT_COMMIT=$(echo "$COMMIT" | head -c 12)
    SNAPSHOT_DATE=$(date -u +%Y%m%d%H%M%S)
    NEW_VERSION="${RAW_VERSION}~${SNAPSHOT_DATE}.gbp${SHORT_COMMIT}"
fi

# Preserve epoch from existing debian/changelog if present
EXISTING_EPOCH=$(head -1 debian/changelog | sed -n 's/^[^ ]* (\([0-9]*\):.*/\1/p')
if [ -n "$EXISTING_EPOCH" ]; then
    DEB_VERSION="${EXISTING_EPOCH}:${NEW_VERSION}"
else
    DEB_VERSION="$NEW_VERSION"
fi

echo "=== Version: NEW_VERSION=$NEW_VERSION DEB_VERSION=$DEB_VERSION ==="

cd "$WORK_DIR"

podman run --name "$CONTAINER" \
    --detach --init \
    --volume "$WORK_DIR/bcachefs-tools:/src:rw" \
    --volume "$CACHE_DIR/rustup:/root/.rustup:rw" \
    --volume "$CACHE_DIR/cargo:/root/.cargo:rw" \
    --volume "$CACHE_DIR/apt:/var/cache/apt:rw" \
    --tmpfs /tmp:exec \
    "$IMAGE" sleep infinity

run() {
    podman exec "$CONTAINER" bash -euxc "$*"
}

# Install build dependencies
run '
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl devscripts git \
        gcc libc6-dev patch tar xz-utils gnupg
'

# Install/update rustup (cached across builds)
run "
    if [ ! -f /root/.cargo/bin/rustup ]; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
            sh -s -- --default-toolchain $RUST_VERSION --profile minimal -y
    else
        export PATH=/root/.cargo/bin:\$PATH
        rustup default $RUST_VERSION
    fi
"

# Install cargo-vendor-filterer (cached via cargo)
run '
    export PATH=/root/.cargo/bin:$PATH
    if ! command -v cargo-vendor-filterer &>/dev/null; then
        cargo install --locked cargo-vendor-filterer
    fi
'

# Build source package (dpkg-buildpackage, not sbuild — no chroot needed for source)
run "
    export PATH=/root/.cargo/bin:\$PATH
    export DEBEMAIL='kent.overstreet@linux.dev'
    export DEBFULLNAME='Kent Overstreet'
    cd /src

    # Update changelog with correct version (use dch directly — gbp dch
    # fails on detached HEAD and || true silently swallows the error)
    dch --newversion '$DEB_VERSION' --distribution unstable --urgency medium \
        'Release $NEW_VERSION'

    # Build source-only package
    dpkg-buildpackage -d -S -us -uc -nc
"

# Collect results — dpkg-buildpackage puts them in parent of source dir
podman exec "$CONTAINER" bash -c "
    mkdir -p /src/result
    cp /src/../*.dsc /src/../*.tar.* /src/../*.changes /src/../*.buildinfo /src/result/ 2>/dev/null || true
    ls -la /src/result/
"
podman cp "$CONTAINER:/src/result/." "$RESULT_DIR/"

echo "=== Source build complete ==="
ls -la "$RESULT_DIR/"
