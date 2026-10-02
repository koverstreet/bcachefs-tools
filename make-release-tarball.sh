#!/usr/bin/env bash

set -o errexit

trap 'rc=$?; echo >&2 "make-release-tarball.sh: FAILED at line $LINENO: \`$BASH_COMMAND\` (exit $rc)"; exit $rc' ERR

version=$1

git checkout v$version
git clean -xfd
make generate_version

cargo license > COPYING.rust-dependencies

git ls-files|
    { cat; echo "version.h"; }|
    tar --create --file bcachefs-tools-$version.tar -T -	\
	--transform="s_^_bcachefs-tools-$version/_"

tar --append --file bcachefs-tools-$version.tar			\
    --transform="s_^_bcachefs-tools-$version/_"			\
    COPYING.rust-dependencies

zstd -z --ultra			bcachefs-tools-$version.tar

gpg --armor --detach-sign	bcachefs-tools-$version.tar
mv bcachefs-tools-$version.tar.asc bcachefs-tools-$version.tar.sign

gpg --armor --sign		bcachefs-tools-$version.tar

scp bcachefs-tools-$version.tar.zst	evilpiepirate.org:/var/www/htdocs/bcachefs-tools/
scp bcachefs-tools-$version.tar.asc	evilpiepirate.org:/var/www/htdocs/bcachefs-tools/
scp bcachefs-tools-$version.tar.sign	evilpiepirate.org:/var/www/htdocs/bcachefs-tools/

# The source replacement config it prints covers every vendored source -
# crates.io and any git dependencies - so it's never out of date:
mkdir .cargo
cargo-vendor-filterer > .cargo/config.toml

cp bcachefs-tools-$version.tar bcachefs-tools-vendored-$version.tar
tar --append --file bcachefs-tools-vendored-$version.tar	\
    --transform="s_^_bcachefs-tools-$version/_"			\
    .cargo vendor

zstd -z --ultra			bcachefs-tools-vendored-$version.tar

gpg --armor --detach-sign	bcachefs-tools-vendored-$version.tar
mv bcachefs-tools-vendored-$version.tar.asc bcachefs-tools-vendored-$version.tar.sign

gpg --armor --sign		bcachefs-tools-vendored-$version.tar

scp bcachefs-tools-vendored-$version.tar.zst	evilpiepirate.org:/var/www/htdocs/bcachefs-tools/
scp bcachefs-tools-vendored-$version.tar.asc	evilpiepirate.org:/var/www/htdocs/bcachefs-tools/
scp bcachefs-tools-vendored-$version.tar.sign	evilpiepirate.org:/var/www/htdocs/bcachefs-tools/
