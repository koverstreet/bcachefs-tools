#!/bin/bash
# One-time setup for the package CI build system on evilpiepirate.org
#
# Run as root, once, on a fresh host. Sets up only what a deploy cannot:
# - Required Debian packages
# - aptbcachefsorg user configuration for rootless podman
# - CI directory structure and its persistent state
#
# The binary, the scripts, the post-receive hook and the systemd unit are NOT
# installed here - they come from the nix closure, together, and activation
# restarts the daemon. See ../doc/deploy.md. Installing any of them from two
# places is how they ended up at four different vintages.

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

echo "=== Enabling lingering for rootless podman ==="
# Without this, /run/user/<uid> only exists while a login session does - and
# the unit points XDG_RUNTIME_DIR at it.
loginctl enable-linger aptbcachefsorg

echo ""
echo "=== Setup complete ==="
echo ""
echo "The unit, hook, scripts and binary come from the deploy, not from here."
echo ""
echo "Next steps:"
echo "  1. GPG:    sudo -u aptbcachefsorg gpg --full-generate-key"
echo "  2. Config: write $CI_DIR/config (GPG_SIGNING_SUBKEY_FINGERPRINT, APTLY_ROOT)"
echo "  3. Deploy: cd package-ci && nix run github:serokell/deploy-rs -- .#angband.package-ci"
echo "  4. Test:   echo \$(git rev-parse HEAD) > $CI_DIR/desired"
