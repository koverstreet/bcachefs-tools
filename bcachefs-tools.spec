%global kmodname bcachefs

# Ensure that the build script shell is bash
%global _buildshell /bin/bash

%global dkmsname dkms-%{kmodname}

# SUSE Linux does not define the dist tag, so we must define it manually
%if "%{_vendor}" == "suse"
%global dist .suse%{?suse_version}
%endif


# Disable LTO for now until more testing can be done.
%global _lto_cflags %{nil}

%global make_opts VERSION="%{version}" BUILD_VERBOSE=1 PREFIX=%{_prefix} ROOT_SBINDIR=%{_sbindir}

%global MSRV 1.77
%global MINIMAL_KERNEL_VERSION_FOR_TOOLS 6.11.3
%global MINIMAL_KERNEL_VERSION_FOR_MODULE 6.16

%global _KMP_TMPDIR "%{_tmppath}/%{name}-%{version}-kmp"

%if %{undefined _with_kmp}
%if 0%{?suse_version} >= 1690
%global _with_kmp 1
%endif
%endif

%if %{undefined _version}
%global _OBS 1
%else
%global _OBS 0
%endif

Name:           bcachefs-tools
# define with i.e. --define '_version 1.0'
Version:        0%{?_version}
Release:        0%{?dist}
Summary:        Userspace tools for bcachefs

# --- rust ---
# Apache-2.0
# Apache-2.0 OR MIT
# Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
# MIT
# MIT OR Apache-2.0
# MPL-2.0
# Unlicense OR MIT
# --- misc ---
# GPL-2.0-only
# GPL-2.0-or-later
# LGPL-2.1-only
# BSD-3-Clause
License:        GPL-2.0-only AND GPL-2.0-or-later AND LGPL-2.1-only AND BSD-3-Clause AND (Apache-2.0 AND (Apache-2.0 OR MIT) AND (Apache-2.0 with LLVM-exception OR Apache-2.0 OR MIT) AND MIT AND MPL-2.0 AND (Unlicense OR MIT))
URL:            https://bcachefs.org/
%if %_OBS
Source:         bcachefs-tools_%{version}.tar.xz
Source1:        bcachefs-tools_%{version}.tar.xz.sig
Source2:        apt.bcachefs.org.keyring
Source3:        cargo.config
Source4:        bcachefs-kmp.spec-preambule
Source99:       %{dkmsname}.rpmlintrc
%else
Source:         https://evilpiepirate.org/%{name}/%{name}-vendored-%{version}.tar.zst
%endif

BuildRequires:  findutils
BuildRequires:  gcc
BuildRequires:  jq
BuildRequires:  make
BuildRequires:  tar
%if %_OBS
BuildRequires:  xz
%else
BuildRequires:  zstd
%endif

BuildRequires:  cargo >= %{MSRV}

%if 0%{?suse_version}
BuildRequires:  rust >= %{MSRV}
%else
BuildRequires:  rustc >= %{MSRV}
%endif

BuildRequires:  kernel-headers >= %{MINIMAL_KERNEL_VERSION_FOR_TOOLS}
BuildRequires:  libaio-devel >= 0.3.111
BuildRequires:  libattr-devel
BuildRequires:  pkgconfig(blkid)
BuildRequires:  pkgconfig(libkeyutils)
BuildRequires:  pkgconfig(liblz4)
BuildRequires:  pkgconfig(libsodium)
BuildRequires:  pkgconfig(libudev)
BuildRequires:  pkgconfig(liburcu) >= 0.15
BuildRequires:  pkgconfig(libzstd)
BuildRequires:  pkgconfig(udev)
BuildRequires:  pkgconfig(uuid)
BuildRequires:  pkgconfig(zlib)

BuildRequires:  clang-devel
BuildRequires:  llvm-devel
BuildRequires:  pkgconfig

BuildRequires:  systemd-rpm-macros

%if 0%{?_with_kmp} != 0
BuildRequires:  %kernel_module_package_buildreqs
BuildRequires:  kernel-devel >= %{MINIMAL_KERNEL_VERSION_FOR_MODULE}
BuildRequires:  kernel-syms  >= %{MINIMAL_KERNEL_VERSION_FOR_MODULE}
%kernel_module_package -n %{kmodname} -x xen -p %_sourcedir/bcachefs-kmp.spec-preambule
%endif

# Rust parts FTBFS on 32-bit arches
ExcludeArch:    %{ix86} %{arm32}

%description
The bcachefs-tools package provides all the userspace programs needed to create,
check, modify and correct any inconsistencies in the bcachefs filesystem.

%files
%license COPYING
%doc doc/bcachefs-principles-of-operation.tex
%doc doc/bcachefs.5.rst.tmpl
%{_sbindir}/bcachefs
%{_sbindir}/mount.bcachefs
%{_sbindir}/fsck.bcachefs
%{_sbindir}/mkfs.bcachefs
%{_mandir}/man8/bcachefs.8*
%{_udevrulesdir}/64-bcachefs.rules
%{_datadir}/bash-completion/completions/bcachefs
%{_sbindir}/mount.fuse.bcachefs
%{_sbindir}/fsck.fuse.bcachefs
%{_sbindir}/mkfs.fuse.bcachefs

%package -n %{dkmsname}
Summary:        Bcachefs kernel module managed by DKMS
Requires:       diffutils
Requires:       dkms >= 3.2.1
Requires:       kernel-devel >= %{MINIMAL_KERNEL_VERSION_FOR_MODULE}
Requires:       gcc
Requires:       make
Requires:       perl
Requires:       python3

Obsoletes:      fuse-bcachefs < %{version}-%{release}

Requires:       %{name} = %{version}-%{release}

# For Fedora/RHEL systems
%if 0%{?fedora} || 0%{?rhel}
Supplements:    (bcachefs-tools and kernel-core)
%endif
# For SUSE systems
%if 0%{?suse_version}
Supplements:    (bcachefs-tools and kernel-default)
%endif

BuildArch:      noarch

%description -n %{dkmsname}
This package is an implementation of bcachefs built using DKMS to offer the kernel
module to mount, create, check, modify and correct any inconsistencies in the bcachefs
filesystem.

%preun -n %{dkmsname}
if [ "$1" -eq 0 ]; then
   if [  "$(dkms status -m %{kmodname} -v %{version})" ]; then
      dkms remove -m %{kmodname} -v %{version} --all
      exit $?
   fi
fi

%post -n %{dkmsname}
if [ "$1" -ge "1" ]; then
%if "%{_vendor}" == "suse"
   if [ -f %{_libexecdir}/dkms/common.postinst ]; then
      %{_libexecdir}/dkms/common.postinst %{kmodname} %{version}
      exit $?
   fi
%else
   if [ -f /usr/lib/dkms/common.postinst ]; then
      /usr/lib/dkms/common.postinst %{kmodname} %{version}
      exit $?
   fi
%endif
fi

%files -n %{dkmsname}
%license COPYING
%{_usrsrc}/%{kmodname}-%{version}/

# ----------------------------------------------------------------------------


%prep
%autosetup


%build
%if %_OBS
export CARGO_HOME=$PWD/.cargo
export CARGO_ARGS="--frozen"
rm -rf $PWD/.cargo
mkdir -p $PWD/.cargo
cp %{_sourcedir}/cargo.config $PWD/.cargo/config.toml
%endif
%set_build_flags
%make_build %{make_opts}

%if 0%{?_with_kmp} != 0
%make_build install_dkms DKMSDIR="%{_KMP_TMPDIR}/src"
for kmp_flavor in %{?flavors_to_build}; do
  rm -Rf "%{_KMP_TMPDIR}/obj-$kmp_flavor"
  cp -a "%{_KMP_TMPDIR}/src/." "%{_KMP_TMPDIR}/obj-$kmp_flavor"
  %make_build \
    -C "%{_KMP_TMPDIR}/obj-$kmp_flavor" \
    KDIR="/usr/src/linux-obj/%_target_cpu/$kmp_flavor"
done
%endif

%install
%if %_OBS
export CARGO_HOME=$PWD/.cargo
export CARGO_ARGS="--frozen"
rm -rf $PWD/.cargo
mkdir -p $PWD/.cargo
cp %{_sourcedir}/cargo.config $PWD/.cargo/config.toml
%endif
%set_build_flags
%make_install %{make_opts}

# Purge unneeded debian stuff
rm -rfv %{buildroot}/%{_datadir}/initramfs-tools

%if 0%{?_with_kmp} != 0
for kmp_flavor in %{?flavors_to_build}; do
  %make_build \
    -C "/usr/src/linux-obj/%_target_cpu/$kmp_flavor" \
    M="%{_KMP_TMPDIR}/obj-$kmp_flavor" \
    INSTALL_MOD_PATH="%buildroot" \
    modules_install
done
%endif

%changelog
* Fri Nov 21 2025 Neal Gompa <neal@gompa.dev>
- Turn off fuse-bcachefs by default
* Sun Nov 19 2025 Roman Lebedev <lebedev.ri@gmail.com>
- Implement KMP package for OpenSUSE Tumbleweed
* Sun Oct 19 2025 Roman Lebedev <lebedev.ri@gmail.com>
- Fix DKMS support on SUSE
* Sun Oct 12 2025 Roman Lebedev <lebedev.ri@gmail.com>
- OBS support
* Sat Sep 27 2025 Neal Gompa <neal@gompa.dev>
- Initial package based on Fedora package
