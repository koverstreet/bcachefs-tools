# rpmbuild with QA_RPATHS=$[0x0001]

Name:           userspace-rcu
Version:        0.11.1
Release:        1%{?dist}
Summary:        liburcu is a LGPLv2.1 userspace RCU (read-copy-update) library.

License:        LGPLv2.1
URL:            https://liburcu.org/
Source0:        https://lttng.org/files/urcu/%{name}-%{version}.tar.bz2
Source1:        https://lttng.org/files/urcu/%{name}-%{version}.tar.bz2.asc

# "devel" files are installed with this package, also.
Conflicts:	userspace-rcu-devel < 0.11.1
Provides:	userspace-rcu-devel

BuildRequires:  bzip2
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  m4

%description
liburcu provides efficient data structures based on RCU and lock-free algorithms. Those structures include hash tables, queues, stacks, and doubly-linked lists.

%prep
%setup -q

%configure

%build
make

%install
rm -rf $RPM_BUILD_ROOT
%make_install

%files
%{_datadir}/doc/userspace-rcu/cds-api.md
%{_datadir}/doc/userspace-rcu/examples/hlist/cds_hlist_add_head_rcu.c
%{_datadir}/doc/userspace-rcu/examples/hlist/cds_hlist_del_rcu.c
%{_datadir}/doc/userspace-rcu/examples/hlist/cds_hlist_for_each_entry_rcu.c
%{_datadir}/doc/userspace-rcu/examples/hlist/cds_hlist_for_each_rcu.c
%{_datadir}/doc/userspace-rcu/examples/hlist/Makefile
%{_datadir}/doc/userspace-rcu/examples/hlist/Makefile.cds_hlist_add_head_rcu
%{_datadir}/doc/userspace-rcu/examples/hlist/Makefile.cds_hlist_del_rcu
%{_datadir}/doc/userspace-rcu/examples/hlist/Makefile.cds_hlist_for_each_entry_rcu
%{_datadir}/doc/userspace-rcu/examples/hlist/Makefile.cds_hlist_for_each_rcu
%{_datadir}/doc/userspace-rcu/examples/lfstack/cds_lfs_pop_all_blocking.c
%{_datadir}/doc/userspace-rcu/examples/lfstack/cds_lfs_pop_blocking.c
%{_datadir}/doc/userspace-rcu/examples/lfstack/cds_lfs_push.c
%{_datadir}/doc/userspace-rcu/examples/lfstack/Makefile
%{_datadir}/doc/userspace-rcu/examples/lfstack/Makefile.cds_lfs_pop_all_blocking
%{_datadir}/doc/userspace-rcu/examples/lfstack/Makefile.cds_lfs_pop_blocking
%{_datadir}/doc/userspace-rcu/examples/lfstack/Makefile.cds_lfs_push
%{_datadir}/doc/userspace-rcu/examples/list/cds_list_add_rcu.c
%{_datadir}/doc/userspace-rcu/examples/list/cds_list_add_tail_rcu.c
%{_datadir}/doc/userspace-rcu/examples/list/cds_list_del_rcu.c
%{_datadir}/doc/userspace-rcu/examples/list/cds_list_for_each_entry_rcu.c
%{_datadir}/doc/userspace-rcu/examples/list/cds_list_for_each_rcu.c
%{_datadir}/doc/userspace-rcu/examples/list/cds_list_replace_rcu.c
%{_datadir}/doc/userspace-rcu/examples/list/Makefile
%{_datadir}/doc/userspace-rcu/examples/list/Makefile.cds_list_add_rcu
%{_datadir}/doc/userspace-rcu/examples/list/Makefile.cds_list_add_tail_rcu
%{_datadir}/doc/userspace-rcu/examples/list/Makefile.cds_list_del_rcu
%{_datadir}/doc/userspace-rcu/examples/list/Makefile.cds_list_for_each_entry_rcu
%{_datadir}/doc/userspace-rcu/examples/list/Makefile.cds_list_for_each_rcu
%{_datadir}/doc/userspace-rcu/examples/list/Makefile.cds_list_replace_rcu
%{_datadir}/doc/userspace-rcu/examples/Makefile
%{_datadir}/doc/userspace-rcu/examples/Makefile.examples.template
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_add_replace.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_add_unique.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_add.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_del.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_destroy.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_for_each_entry_duplicate.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/cds_lfht_lookup.c
%{_datadir}/doc/userspace-rcu/examples/rculfhash/jhash.h
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_add
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_add_replace
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_add_unique
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_del
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_destroy
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_for_each_entry_duplicate
%{_datadir}/doc/userspace-rcu/examples/rculfhash/Makefile.cds_lfht_lookup
%{_datadir}/doc/userspace-rcu/examples/rculfqueue/cds_lfq_dequeue.c
%{_datadir}/doc/userspace-rcu/examples/rculfqueue/cds_lfq_enqueue.c
%{_datadir}/doc/userspace-rcu/examples/rculfqueue/Makefile
%{_datadir}/doc/userspace-rcu/examples/rculfqueue/Makefile.cds_lfq_dequeue
%{_datadir}/doc/userspace-rcu/examples/rculfqueue/Makefile.cds_lfq_enqueue
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/bp.c
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/Makefile
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/Makefile.bp
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/Makefile.mb
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/Makefile.membarrier
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/Makefile.qsbr
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/Makefile.signal
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/mb.c
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/membarrier.c
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/qsbr.c
%{_datadir}/doc/userspace-rcu/examples/urcu-flavors/signal.c
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/cds_wfcq_dequeue.c
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/cds_wfcq_enqueue.c
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/cds_wfcq_splice.c
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/Makefile
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/Makefile.cds_wfcq_dequeue
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/Makefile.cds_wfcq_enqueue
%{_datadir}/doc/userspace-rcu/examples/wfcqueue/Makefile.cds_wfcq_splice
%{_datadir}/doc/userspace-rcu/examples/wfstack/cds_wfs_pop_all_blocking.c
%{_datadir}/doc/userspace-rcu/examples/wfstack/cds_wfs_pop.c
%{_datadir}/doc/userspace-rcu/examples/wfstack/cds_wfs_push.c
%{_datadir}/doc/userspace-rcu/examples/wfstack/Makefile
%{_datadir}/doc/userspace-rcu/examples/wfstack/Makefile.cds_wfs_pop
%{_datadir}/doc/userspace-rcu/examples/wfstack/Makefile.cds_wfs_pop_all_blocking
%{_datadir}/doc/userspace-rcu/examples/wfstack/Makefile.cds_wfs_push
%{_datadir}/doc/userspace-rcu/LICENSE
%{_datadir}/doc/userspace-rcu/rcu-api.md
%{_datadir}/doc/userspace-rcu/README.md
%{_datadir}/doc/userspace-rcu/solaris-build.md
%{_datadir}/doc/userspace-rcu/uatomic-api.md
%{_includedir}/urcu-bp.h
%{_includedir}/urcu-call-rcu.h
%{_includedir}/urcu-defer.h
%{_includedir}/urcu-flavor.h
%{_includedir}/urcu-pointer.h
%{_includedir}/urcu-qsbr.h
%{_includedir}/urcu.h
%{_includedir}/urcu/arch.h
%{_includedir}/urcu/arch/generic.h
%{_includedir}/urcu/call-rcu.h
%{_includedir}/urcu/cds.h
%{_includedir}/urcu/compiler.h
%{_includedir}/urcu/config.h
%{_includedir}/urcu/debug.h
%{_includedir}/urcu/defer.h
%{_includedir}/urcu/flavor.h
%{_includedir}/urcu/futex.h
%{_includedir}/urcu/hlist.h
%{_includedir}/urcu/lfstack.h
%{_includedir}/urcu/list.h
%{_includedir}/urcu/map/clear.h
%{_includedir}/urcu/map/urcu-bp.h
%{_includedir}/urcu/map/urcu-mb.h
%{_includedir}/urcu/map/urcu-memb.h
%{_includedir}/urcu/map/urcu-qsbr.h
%{_includedir}/urcu/map/urcu-signal.h
%{_includedir}/urcu/map/urcu.h
%{_includedir}/urcu/pointer.h
%{_includedir}/urcu/rcuhlist.h
%{_includedir}/urcu/rculfhash.h
%{_includedir}/urcu/rculfqueue.h
%{_includedir}/urcu/rculfstack.h
%{_includedir}/urcu/rculist.h
%{_includedir}/urcu/ref.h
%{_includedir}/urcu/static/lfstack.h
%{_includedir}/urcu/static/pointer.h
%{_includedir}/urcu/static/rculfqueue.h
%{_includedir}/urcu/static/rculfstack.h
%{_includedir}/urcu/static/urcu-bp.h
%{_includedir}/urcu/static/urcu-common.h
%{_includedir}/urcu/static/urcu-mb.h
%{_includedir}/urcu/static/urcu-memb.h
%{_includedir}/urcu/static/urcu-qsbr.h
%{_includedir}/urcu/static/urcu-signal.h
%{_includedir}/urcu/static/urcu.h
%{_includedir}/urcu/static/wfcqueue.h
%{_includedir}/urcu/static/wfqueue.h
%{_includedir}/urcu/static/wfstack.h
%{_includedir}/urcu/syscall-compat.h
%{_includedir}/urcu/system.h
%{_includedir}/urcu/tls-compat.h
%{_includedir}/urcu/uatomic_arch.h
%{_includedir}/urcu/uatomic.h
%{_includedir}/urcu/uatomic/generic.h
%{_includedir}/urcu/urcu_ref.h
%{_includedir}/urcu/urcu-bp.h
%{_includedir}/urcu/urcu-futex.h
%{_includedir}/urcu/urcu-mb.h
%{_includedir}/urcu/urcu-memb.h
%{_includedir}/urcu/urcu-qsbr.h
%{_includedir}/urcu/urcu-signal.h
%{_includedir}/urcu/urcu.h
%{_includedir}/urcu/wfcqueue.h
%{_includedir}/urcu/wfqueue.h
%{_includedir}/urcu/wfstack.h
%{_libdir}/liburcu-bp.a
%{_libdir}/liburcu-bp.la
%{_libdir}/liburcu-bp.so
%{_libdir}/liburcu-bp.so.6
%{_libdir}/liburcu-bp.so.6.1.0
%{_libdir}/liburcu-cds.a
%{_libdir}/liburcu-cds.la
%{_libdir}/liburcu-cds.so
%{_libdir}/liburcu-cds.so.6
%{_libdir}/liburcu-cds.so.6.1.0
%{_libdir}/liburcu-common.a
%{_libdir}/liburcu-common.la
%{_libdir}/liburcu-common.so
%{_libdir}/liburcu-common.so.6
%{_libdir}/liburcu-common.so.6.1.0
%{_libdir}/liburcu-mb.a
%{_libdir}/liburcu-mb.la
%{_libdir}/liburcu-mb.so
%{_libdir}/liburcu-mb.so.6
%{_libdir}/liburcu-mb.so.6.1.0
%{_libdir}/liburcu-memb.a
%{_libdir}/liburcu-memb.la
%{_libdir}/liburcu-memb.so
%{_libdir}/liburcu-memb.so.6
%{_libdir}/liburcu-memb.so.6.1.0
%{_libdir}/liburcu-qsbr.a
%{_libdir}/liburcu-qsbr.la
%{_libdir}/liburcu-qsbr.so
%{_libdir}/liburcu-qsbr.so.6
%{_libdir}/liburcu-qsbr.so.6.1.0
%{_libdir}/liburcu-signal.a
%{_libdir}/liburcu-signal.la
%{_libdir}/liburcu-signal.so
%{_libdir}/liburcu-signal.so.6
%{_libdir}/liburcu-signal.so.6.1.0
%{_libdir}/liburcu.a
%{_libdir}/liburcu.la
%{_libdir}/liburcu.so
%{_libdir}/liburcu.so.6
%{_libdir}/liburcu.so.6.1.0
%{_libdir}/pkgconfig/liburcu-bp.pc
%{_libdir}/pkgconfig/liburcu-cds.pc
%{_libdir}/pkgconfig/liburcu-mb.pc
%{_libdir}/pkgconfig/liburcu-qsbr.pc
%{_libdir}/pkgconfig/liburcu-signal.pc
%{_libdir}/pkgconfig/liburcu.pc

%changelog
* Tue Jan 07 2020 Michael Adams <unquietwiki@gmail.com> - 0.11-1
- Initial RPM package
