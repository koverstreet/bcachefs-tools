#ifndef _LINUX_RCULIST_H
#define _LINUX_RCULIST_H

#include <urcu/rculist.h>


#include <urcu/rcuhlist.h>

#define hlist_add_head_rcu		cds_hlist_add_head_rcu
#define hlist_del_rcu			cds_hlist_del_rcu

#define hlist_for_each_rcu		cds_hlist_for_each_rcu
#define hlist_for_each_entry_rcu	cds_hlist_for_each_entry_rcu_2


#endif
