#!/bin/bash
# Build and deploy the Principles of Operation PDF
# Can be run directly or as a git pre-push hook (symlink .git/hooks/pre-push here)
set -e

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
cd "$SCRIPT_DIR/.."

# Consume stdin if run as hook (git passes ref info on stdin)
if [ -p /dev/stdin ]; then
    cat > /dev/null
fi

echo "Building PDF..."
make bcachefs-principles-of-operation.pdf

echo "Deploying to bcachefs.org..."
scp bcachefs-principles-of-operation.pdf root@evilpiepirate.org:/home/bcachefs/doc/
ssh root@evilpiepirate.org "chown bcachefs:bcachefs /home/bcachefs/doc/bcachefs-principles-of-operation.pdf"

echo "Done! PDF deployed to https://bcachefs.org/bcachefs-principles-of-operation.pdf"
