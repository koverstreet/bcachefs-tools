#!/bin/bash
# Publish built .deb packages to apt.bcachefs.org
#
# Signs .debs with debsigs, then includes them in aptly repos and publishes
# to a staging directory. After aptly finishes, rsync with --delay-updates
# copies to the live directory — each file is written to a temp name, then
# renamed into place, so the live tree is never in an inconsistent state.
#
# Usage: publish.sh COMMIT [snapshot|release]
#   SUITE defaults to "snapshot" (use "release" for tagged releases)
#
# Config read from $STATE_DIR/config:
#   GPG_SIGNING_SUBKEY_FINGERPRINT
#   APTLY_ROOT
#   PUBLISH_ROOT  (where nginx serves from; defaults to $APTLY_ROOT/public)

set -euo pipefail

COMMIT="$1"
SUITE="${2:-snapshot}"

STATE_DIR="${STATE_DIR:-/home/aptbcachefsorg/package-ci}"
BUILD_DIR="$STATE_DIR/builds/$COMMIT"
SHORT="${COMMIT:0:12}"

# Load config
# shellcheck source=/dev/null
source "$STATE_DIR/config"
: "${GPG_SIGNING_SUBKEY_FINGERPRINT:?not set in config}"
: "${APTLY_ROOT:?not set in config}"
: "${PUBLISH_ROOT:=$APTLY_ROOT/public}"

STAGING_ROOT="$APTLY_ROOT/staging"
SNAPSHOT_DATE="$(date -u +%Y%m%d%H%M%S)"

mkdir -p "$STAGING_ROOT"

# Aptly config — publish to staging directory, not directly to live.
# No -force-overwrite: that flag corrupts shared pool files by overwriting
# them in-place, leaving metadata hashes stale.  Instead we remove old
# packages before adding new ones.
APTLY_CONF="$(mktemp)"
trap "rm -f '$APTLY_CONF'" EXIT
cat > "$APTLY_CONF" << EOF
{
    "rootDir": "$APTLY_ROOT",
    "gpgDisableVerify": true,
    "skipContentsPublishing": true,
    "FileSystemPublishEndpoints": {
        "public": {
            "rootDir": "$STAGING_ROOT",
            "linkMethod": "symlink"
        }
    }
}
EOF

aptly() { command aptly -config="$APTLY_CONF" "$@"; }

echo "=== Publishing $SHORT (suite=$SUITE) ==="

SRC_DIR="$BUILD_DIR/source/result"
if [ ! -d "$SRC_DIR" ]; then
    echo "ERROR: no source result dir at $SRC_DIR"
    exit 1
fi

sign_debs() {
    local dir="$1"
    find "$dir" -maxdepth 1 \( -name "*.deb" -o -name "*.ddeb" \) | while read -r deb; do
        echo "  signing $(basename "$deb")"
        debsigs --verbose --default-key="$GPG_SIGNING_SUBKEY_FINGERPRINT" --sign=origin "$deb"
    done
}

echo "--- Signing source artifacts ---"
sign_debs "$SRC_DIR"

# Collect which distros have at least one successful arch build
declare -A DISTRO_DONE
for job_dir in "$BUILD_DIR"/*/; do
    job="$(basename "$job_dir")"
    [ "$job" = "source" ] && continue
    [ "$job" = "publish" ] && continue
    status="$(cat "$job_dir/status" 2>/dev/null || echo pending)"
    [ "$status" != "done" ] && continue
    distro="${job%-*}"
    DISTRO_DONE["$distro"]=1
    echo "--- Signing $job ---"
    sign_debs "$job_dir/result"
done

# Include, snapshot, publish per distro
for distro in "${!DISTRO_DONE[@]}"; do
    REPO_NAME="$distro-$SUITE"
    REPO_SUITE="bcachefs-tools-$SUITE"
    SNAPSHOT_NAME="$REPO_NAME-$SNAPSHOT_DATE"
    PUBLISH_PREFIX="filesystem:public:$distro"

    echo "--- $distro: including into $REPO_NAME ---"

    aptly repo show "$REPO_NAME" &>/dev/null || \
        aptly repo create \
            -distribution="$REPO_SUITE" \
            -component=main \
            "$REPO_NAME"

    # Clear old packages before adding — avoids -force-replace/-force-overwrite
    # which can corrupt pool files shared across repos
    aptly repo remove "$REPO_NAME" 'Name (% bcachefs-*)' 2>/dev/null || true

    # Build list of dirs to include: source + all arches for this distro
    INCLUDE_DIRS=("$SRC_DIR")
    for job_dir in "$BUILD_DIR/${distro}"-*/; do
        [ -d "$job_dir/result" ] && \
            [ "$(cat "$job_dir/status" 2>/dev/null)" = "done" ] && \
            INCLUDE_DIRS+=("$job_dir/result")
    done

    # repo add takes .deb/.dsc files directly (avoids needing signed .changes)
    aptly repo add "$REPO_NAME" "${INCLUDE_DIRS[@]}"

    echo "--- $distro: snapshot $SNAPSHOT_NAME ---"
    aptly snapshot create "$SNAPSHOT_NAME" from repo "$REPO_NAME"

    echo "--- $distro: publish ---"
    if aptly publish show "$REPO_SUITE" "$PUBLISH_PREFIX" &>/dev/null; then
        aptly publish switch \
            "$REPO_SUITE" "$PUBLISH_PREFIX" "$SNAPSHOT_NAME"
    else
        aptly publish snapshot \
            -acquire-by-hash \
            -origin="apt.bcachefs.org" \
            -label="apt.bcachefs.org Packages" \
            "$SNAPSHOT_NAME" \
            "$PUBLISH_PREFIX"
    fi
done

# Sync staging to live directory.  --delay-updates writes each updated file
# to a temp name first, then renames them all into place at the end — the
# live tree is never half-old half-new.
# No --delete: staging only has suites published in this run, other suites
# (e.g. snapshot when publishing release, or vice versa) must be preserved.
echo "--- Syncing staging to live ---"
rsync -rlpt --delay-updates "$STAGING_ROOT/" "$PUBLISH_ROOT/"

# Export GPG public key so users can fetch it for apt verification
echo "--- Exporting GPG public key ---"
gpg --armor --export "$GPG_SIGNING_SUBKEY_FINGERPRINT" > "$PUBLISH_ROOT/apt.bcachefs.org.asc"

# Generate landing page footer (nginx fancyindex_footer)
echo "--- Generating landing page ---"
mkdir -p "$PUBLISH_ROOT/.footer"
cat > "$PUBLISH_ROOT/.footer/README.html" << 'FOOTER'
<hr>
<h2>Adding this repository</h2>
<pre><code>wget -qO- https://apt.bcachefs.org/apt.bcachefs.org.asc | sudo tee /etc/apt/trusted.gpg.d/apt.bcachefs.org.asc
FOOTER

# Inject the real fingerprint
cat >> "$PUBLISH_ROOT/.footer/README.html" << EOF
# Fingerprint: $GPG_SIGNING_SUBKEY_FINGERPRINT
EOF

cat >> "$PUBLISH_ROOT/.footer/README.html" << 'FOOTER'

sudo tee /etc/apt/sources.list.d/apt.bcachefs.org.sources > /dev/null &lt;&lt;SOURCES
Types: deb deb-src
URIs: https://apt.bcachefs.org/unstable/
Suites: bcachefs-tools-release
Components: main
Signed-By: /etc/apt/trusted.gpg.d/apt.bcachefs.org.asc
SOURCES

sudo apt update
sudo apt install bcachefs-tools
</code></pre>
<p><strong>Note:</strong> For latest <code>git master</code> packages, replace <code>bcachefs-tools-release</code> with <code>bcachefs-tools-snapshot</code>.</p>
<p>For more information, see the <a href="https://wiki.debian.org/DebianRepository/UseThirdParty">Debian third-party repository guide</a>.</p>
<p>Binary <code>.deb</code> packages are signed with <a href="https://manpages.debian.org/debsigs">debsigs</a>. Source artifacts can be verified using <a href="https://github.com/sigstore/rekor">Rekor</a>.</p>
FOOTER

echo "=== Publish complete: $SHORT ==="
