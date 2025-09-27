# Packaging for BCacheFS for Linux for RPM and Debian distributions

This is an attempt to capture packaging BCacheFS for Linux for RPM and Debian distributions.

## How to build the package (rpm)

On a RPM system,

1. Install the `rpm-build` package
2. Create the folder tree: `mkdir -p ~/rpmbuild/{SPECS,SOURCES,BUILD,BUILDROOT,RPMS,SRPMS}`
3. Copy all the source files into `~/rpmbuild/SOURCES`
4. Download the bcachefs-tools sources referenced in the spec and put it in `~/rpmbuild/SOURCES`
5. Copy the spec to `~/rpmbuild/SPECS`
6. Run rpmbuild: `rpmbuild -ba ~/rpmbuild/SPECS/bcachefs-tools.spec`

## How to build the package (deb)

This packaging can be used to build packages using [debbuild](https://github.com/debbuild/debbuild) for Debian targets.
You can install debbuild from the openSUSE Build Service for either [Debian](https://software.opensuse.org//download.html?project=Debian%3Adebbuild&package=debbuild) or [Ubuntu](https://software.opensuse.org//download.html?project=Ubuntu%3Adebbuild&package=debbuild).

On a Debian/Ubuntu system,

1. Install `debbuild` from the openSUSE Build Service.
2. Create the folder tree: `mkdir -p ~/debbuild/{SPECS,SOURCES,BUILD,BUILDROOT,DEBS,SDEBS}`
3. Copy all the source files into `~/debbuild/SOURCES`
4. Download the bcachefs-tools sources referenced in the spec and put it in `~/debbuild/SOURCES`
5. Copy the spec to `~/debbuild/SPECS`
6. Run debbuild: `debbuild -ba ~/debbuild/SPECS/bcachefs-tools.spec`
