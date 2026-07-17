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

# Set up remote work directory
ssh_remote "mkdir -p $REMOTE_WORK/source $REMOTE_WORK/result"

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
