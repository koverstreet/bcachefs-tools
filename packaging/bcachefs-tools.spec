Name:           bcachefs-tools
Version:        2020.01.07
Release:        1%{?dist}
Summary:        Userspace tools for bcachefs

License:        GPLv2
URL:            https://github.com/koverstreet/bcachefs-tools
Source0:        %{name}-%{version}.tar.bz2

BuildRequires:  epel-release
BuildRequires:  bzip2
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  keyutils-libs-devel
BuildRequires:  libaio-devel
BuildRequires:  libattr-devel
BuildRequires:  libblkid-devel
BuildRequires:  libscrypt-devel
BuildRequires:  libsodium-devel
BuildRequires:  libtool-ltdl-devel
BuildRequires:  libuuid-devel
BuildRequires:  libvmmalloc-devel
BuildRequires:  libzstd-devel
BuildRequires:  lz4-devel
BuildRequires:  userspace-rcu-devel
BuildRequires:  valgrind-devel
BuildRequires:  zlib-devel

Requires:   epel-release
Requires:   bzip2
Requires:   keyutils-libs
Requires:   libaio
Requires:   libattr
Requires:   libblkid
Requires:   libscrypt
Requires:   libsodium
Requires:   libtool-ltdl
Requires:   libuuid
Requires:   libvmmalloc
Requires:   libzstd
Requires:   lz4
Requires:   userspace-rcu
Requires:   valgrind
Requires:   zlib

%description
The bcachefs tool, which has a number of subcommands for formatting and managing bcachefs filesystems. Run bcachefs --help for full list of commands.

%prep
%setup -q

%build
make

%install
rm -rf $RPM_BUILD_ROOT
mkdir -p $RPM_BUILD_ROOT/usr/local/sbin
mkdir -p $RPM_BUILD_ROOT/usr/local/share/man/man8
%make_install

%files
/usr/local/sbin/bcachefs
/usr/local/sbin/fsck.bcachefs
/usr/local/sbin/mkfs.bcachefs
/usr/local/share/man/man8/bcachefs.8
/etc/initramfs-tools/hooks/bcachefs
/etc/initramfs-tools/scripts/local-premount/bcachefs

%changelog
* Tue Jan 07 2020 Michael Adams <unquietwiki@gmail.com> - 2020.01.07-1
- Initial RPM package definition
- Makefile needs further work to accomodate RPM macros.
