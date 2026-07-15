#!/bin/bash
# Show package-ci build status
#
# Usage: status.sh [COMMIT]
# Without COMMIT, shows the desired (latest) commit.

STATE_DIR="${STATE_DIR:-/home/aptbcachefsorg/package-ci}"

COMMIT="${1:-}"
if [ -z "$COMMIT" ]; then
    DESIRED="$STATE_DIR/desired"
    if [ ! -f "$DESIRED" ]; then
        echo "No desired commit found in $DESIRED"
        exit 1
    fi
    COMMIT="$(cat "$DESIRED")"
fi

BUILD_DIR="$STATE_DIR/builds/$COMMIT"
if [ ! -d "$BUILD_DIR" ]; then
    echo "No build directory for $COMMIT"
    exit 1
fi

echo "Commit: ${COMMIT:0:12}"
echo ""

# Print a padded row
row() {
    printf "  %-24s %s\n" "$1" "$2"
}

done=0; failed=0; pending=0; building=0

for job_dir in "$BUILD_DIR"/*/; do
    job="$(basename "$job_dir")"
    status="$(cat "$job_dir/status" 2>/dev/null || echo pending)"
    case "$status" in
        done)     sym="✓"; ((done++)) ;;
        failed)   sym="✗"; ((failed++)) ;;
        building) sym="…"; ((building++)) ;;
        *)        sym="·"; ((pending++)) ;;
    esac
    row "$job" "$sym $status"
done

echo ""
echo "  done=$done  failed=$failed  building=$building  pending=$pending"
