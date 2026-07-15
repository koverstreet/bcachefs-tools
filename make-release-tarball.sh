#!/bin/bash

set -o errexit

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

cargo-vendor-filterer

mkdir .cargo
cat > .cargo/config.toml <<-ZZ
[source.crates-io]
replace-with = "vendored-sources"

[source."git+https://evilpiepirate.org/git/rust-bindgen.git"]
git = "https://evilpiepirate.org/git/rust-bindgen.git"
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
ZZ

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
