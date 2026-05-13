/*
 * Userspace-only glue: register libbcachefs percpu variables that need
 * per-thread setup with the bch_percpu callback registry.
 *
 * In the kernel these get initialized at module init via for_each_possible_cpu()
 * walks (since possible cpus are fixed at boot). In userspace, threads come
 * and go dynamically, so we register init_one/exit_one via bch_percpu_register()
 * and let the registry call them per-thread at thread-create / module-exit.
 *
 * Lives outside fs/ so it doesn't get clobbered on next sync.
 */
#include <linux/percpu.h>

#include "fs/btree/locking.h"

__attribute__((constructor(115)))
static void bch2_percpu_register(void)
{
	bch_percpu_register(
		(void (*)(void *)) bch2_lock_graph_init_one,
		(void (*)(void *)) bch2_lock_graph_exit_one,
		&bch2_lock_graph);
}
