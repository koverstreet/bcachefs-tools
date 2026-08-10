#!/bin/bash
# Build binary .deb on a remote host (arm64 on farm1)
#
# Handles: scp source artifacts over, run build, scp results back.
#
# Usage: build-binary-remote.sh HOST DISTRO ARCH COMMIT SOURCE_DIR RESULT_DIR RUST_VERSION

set -euo pipefail

HOST="$1"
DISTRO="$2"
ARCH="$3"
COMMIT="$4"
SOURCE_DIR="$5"
RESULT_DIR="$6"
RUST_VERSION="$7"

REMOTE_WORK="/tmp/bcachefs-ci/${COMMIT}/${DISTRO}-${ARCH}"

# We rm -rf this over ssh, so prove it's the path we think it is first. set -u
# rejects *missing* arguments; an empty one would collapse the path a level and
# still look plausible. Ask the question directly rather than by glob - a
# pattern like /tmp/bcachefs-ci/*/*-* accepts the empty case, because `*`
# matches the empty string.
for v in COMMIT DISTRO ARCH; do
    if [ -z "${!v}" ]; then
        echo "refusing to build: $v is empty (remote work dir would be '$REMOTE_WORK')" >&2
        exit 1
    fi
done

SSH_OPTS=(
    -o BatchMode=yes
    -o ConnectTimeout=30
    -o ServerAliveInterval=30
    -o ServerAliveCountMax=4
)

ssh_remote() {
    echo "+ ssh $HOST $*"
    ssh "${SSH_OPTS[@]}" "$HOST" "$@"
}

scp_to_remote() {
    local dest="$1"
    shift

    echo "+ scp $* $HOST:$dest"
    timeout --foreground 300 scp "${SSH_OPTS[@]}" "$@" "$HOST:$dest"
}

scp_from_remote_dir() {
    local src_dir="$1"
    local dest="$2"

    echo "+ scp -r $HOST:$src_dir/. $dest/"
    timeout --foreground 300 scp -r "${SSH_OPTS[@]}" "$HOST:$src_dir/." "$dest/"
}

echo "=== Remote build: $DISTRO $ARCH on $HOST ==="

# Set up the remote work directory, from scratch.
#
# The rm is load-bearing, not hygiene. The scripts now ship from the nix store,
# where they are mode r-xr-xr-x, and scp preserves that mode on the copy. A
# second attempt into a surviving directory therefore dies with
#
#   scp: dest open ".../build-binary.sh": Permission denied
#
# because it cannot overwrite the read-only copy the first attempt left behind.
# That breaks every retry of a remote build - after an orchestrator restart, a
# build timeout, or any transient error - and it only became reachable when
# deployment moved to a nix closure; a git checkout left these writable.
#
# Starting clean is right regardless: a build that inherits artifacts from a
# previous attempt is a build whose output nobody can account for.
ssh_remote "rm -rf $REMOTE_WORK && mkdir -p $REMOTE_WORK/source $REMOTE_WORK/result"

# Ship source artifacts
scp_to_remote "$REMOTE_WORK/source/" "$SOURCE_DIR"/*

# Ship the build script
SCRIPT_DIR="$(dirname "$0")"
scp_to_remote "$REMOTE_WORK/" "$SCRIPT_DIR/build-binary.sh"

# Run the build
ssh_remote "bash $REMOTE_WORK/build-binary.sh \
    $DISTRO $ARCH $COMMIT \
    $REMOTE_WORK/source $REMOTE_WORK/result \
    $RUST_VERSION"

# Ship results back
mkdir -p "$RESULT_DIR"
scp_from_remote_dir "$REMOTE_WORK/result" "$RESULT_DIR"

# Clean up remote
ssh_remote "rm -rf $REMOTE_WORK"

echo "=== Remote build complete: $DISTRO $ARCH ==="
ls -la "$RESULT_DIR/"
