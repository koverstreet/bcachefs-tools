To add this repository to your computer, do:
```bash
wget -qO- https://apt.bcachefs.org/apt.bcachefs.org.asc | sudo tee /etc/apt/trusted.gpg.d/apt.bcachefs.org.asc
# Fingerprint: EA483B991020C72A8A5035ADA0620B5E0E01C1DD
sudo tee /etc/apt/sources.list.d/apt.bcachefs.org.sources > /dev/null <<EOF
Types: deb deb-src
URIs: https://apt.bcachefs.org/unstable/
# Or replace unstable with your distro's release name
Suites: bcachefs-tools-release
Components: main
Signed-By: /etc/apt/trusted.gpg.d/apt.bcachefs.org.asc
EOF
sudo apt update
sudo apt install bcachefs-tools
```

> **_NOTE:_**
This will give you packages for the latest release of `bcachefs-tools`.
If you need packages for the latest `git master` commit,
replace `bcachefs-tools-release` with `bcachefs-tools-snapshot`.

Or you can use `add-apt-repository` tool. Stable channel:
```bash
sudo add-apt-repository "deb https://apt.bcachefs.org/unstable bcachefs-tools-release main"
```

If you feel like living dangerously, there's also nightly/snapshot packages:
```bash
sudo add-apt-repository "deb https://apt.bcachefs.org/unstable bcachefs-tools-snapshot main"
```

If you want to ensure that the packages from this repository are always preferred, do:
```bash
sudo mkdir -p /etc/apt/preferences.d
sudo tee /etc/apt/preferences.d/apt.bcachefs.org.pref > /dev/null <<EOF
Package: *
Pin: origin apt.bcachefs.org
Pin-Priority: 1000
EOF
```

> **_NOTE:_**
Note that yes, you should always prefer `Pin: origin <hostname>`,
over `Pin: release o=<origin>`, because pinning by origin
will *actually* pin by the full hostname of APT repository,
whereas any repository can claim anything in it's `Origin: ` field,
and thus pinning by origin label is inherently insecure!


For more information, see:
https://wiki.debian.org/DebianRepository/UseThirdParty

Source, Debian tarballs, dsc files and binary `.deb` packages can be verified using https://github.com/sigstore/rekor.

Binary `.deb` packages are signed with debsigs.
