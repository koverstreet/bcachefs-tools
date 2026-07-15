#!/bin/bash
# One-time setup for the package CI build system on evilpiepirate.org
#
# Run as root. Sets up:
# - Required packages
# - aptbcachefsorg user configuration for rootless podman
# - CI directory structure
# - Post-receive hook
# - Systemd service
#
# After running this:
# 1. Build the orchestrator: cd package-ci && cargo build --release
# 2. Copy binary: cp target/release/bcachefs-package-ci /home/aptbcachefsorg/package-ci/
# 3. Copy scripts: cp scripts/*.sh /home/aptbcachefsorg/package-ci/scripts/
# 4. Generate GPG key: sudo -u aptbcachefsorg gpg --full-generate-key
# 5. Start: systemctl start bcachefs-package-ci
# 6. Test: git push to trigger a build

set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root"
    exit 1
fi

echo "=== Installing packages ==="
apt-get update
apt-get install -y \
    podman \
    sbuild \
    mmdebstrap \
    aptly \
    gnupg \
    devscripts \
    git-buildpackage \
    qemu-user-static \
    uidmap

echo "=== Configuring aptbcachefsorg for rootless podman ==="
# subuids/subgids for rootless podman
if ! grep -q aptbcachefsorg /etc/subuid; then
    usermod --add-subuids 100000-165535 aptbcachefsorg
fi
if ! grep -q aptbcachefsorg /etc/subgid; then
    usermod --add-subgids 100000-165535 aptbcachefsorg
fi

echo "=== Creating CI directory structure ==="
CI_DIR="/home/aptbcachefsorg/package-ci"
mkdir -p "$CI_DIR/scripts" "$CI_DIR/cache/rustup" "$CI_DIR/cache/cargo" "$CI_DIR/cache/apt"
chown -R aptbcachefsorg:aptbcachefsorg "$CI_DIR"

echo "=== Installing post-receive hook ==="
HOOK="/var/www/git/bcachefs-tools.git/hooks/post-receive"
if [ -f "$HOOK" ]; then
    echo "WARNING: post-receive hook already exists at $HOOK"
    echo "Please review and install manually"
else
    cp "$(dirname "$0")/post-receive" "$HOOK"
    chmod +x "$HOOK"
    echo "Installed post-receive hook"
fi

echo "=== Installing systemd service ==="
cp "$(dirname "$0")/../bcachefs-package-ci.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable bcachefs-package-ci

echo ""
echo "=== Setup complete ==="
echo ""
echo "Next steps:"
echo "  1. Build:  cd package-ci && cargo build --release"
echo "  2. Deploy: cp target/release/bcachefs-package-ci /home/aptbcachefsorg/package-ci/"
echo "  3. Deploy: cp scripts/*.sh /home/aptbcachefsorg/package-ci/scripts/"
echo "  4. GPG:    sudo -u aptbcachefsorg gpg --full-generate-key"
echo "  5. Start:  systemctl start bcachefs-package-ci"
echo "  6. Test:   echo \$(git rev-parse HEAD) > /home/aptbcachefsorg/package-ci/desired"
