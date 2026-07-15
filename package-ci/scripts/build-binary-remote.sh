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

echo "=== Remote build: $DISTRO $ARCH on $HOST ==="

# Set up remote work directory
ssh "$HOST" "mkdir -p $REMOTE_WORK/source $REMOTE_WORK/result"

# Ship source artifacts
scp "$SOURCE_DIR"/* "$HOST:$REMOTE_WORK/source/"

# Ship the build script
SCRIPT_DIR="$(dirname "$0")"
scp "$SCRIPT_DIR/build-binary.sh" "$HOST:$REMOTE_WORK/"

# Run the build
ssh "$HOST" "bash $REMOTE_WORK/build-binary.sh \
    $DISTRO $ARCH $COMMIT \
    $REMOTE_WORK/source $REMOTE_WORK/result \
    $RUST_VERSION"

# Ship results back
mkdir -p "$RESULT_DIR"
scp "$HOST:$REMOTE_WORK/result/"* "$RESULT_DIR/"

# Clean up remote
ssh "$HOST" "rm -rf $REMOTE_WORK"

echo "=== Remote build complete: $DISTRO $ARCH ==="
ls -la "$RESULT_DIR/"
