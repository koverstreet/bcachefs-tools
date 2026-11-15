#ifndef _LINUX_LIST_H
#define _LINUX_LIST_H

#include <urcu/list.h>

#define list_head			cds_list_head
#define INIT_LIST_HEAD(l)		CDS_INIT_LIST_HEAD(l)

/* hlists: */

#include <urcu/hlist.h>

#define hlist_head			cds_hlist_head
#define hlist_node			cds_hlist_node

#endif /* _LIST_LIST_H */
