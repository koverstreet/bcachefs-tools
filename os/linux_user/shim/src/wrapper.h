#include <urcu.h>

#include "include/linux/blkdev.h"
#include "include/linux/delay.h"
#include "include/linux/device.h"
#include "include/linux/idr.h"
#include "include/linux/kernel.h"
#include "include/linux/llist.h"
#include "include/linux/lz4.h"
#include "include/linux/mm.h"
#include "include/linux/percpu-refcount.h"
#include "include/linux/posix_acl.h"
#include "include/linux/random.h"
#include "include/linux/refcount.h"
#include "include/linux/rcupdate.h"
#include "include/linux/sched.h"
#include "include/linux/seqlock.h"
#include "include/linux/string.h"
#include "include/linux/uuid.h"
#include "include/linux/unicode.h"
#include "include/linux/xxhash.h"
#include "include/linux/xattr.h"
#include "include/linux/zlib.h"
#include "include/linux/zstd.h"
#include "include/crypto/sha2.h"	/* after linux/: it isn't self-contained */
#include "include/crypto/chacha.h"
#include "include/crypto/poly1305.h"
#include "fs/vendor/closure.h"
#include "raid/raid.h"		/* raid_gen(), raid_rec(): the kernel's raid6/xor, in userspace */

#include <errno.h>
#include <keyutils.h>

/*
 * liburcu's functions are macros, naming its flavour's - functions, or with
 * _LGPL_SOURCE static inlines, or no-ops: by the macros' names, wrappers
 * bindgen compiles, as it does the shim's inlines - build.rs strips the
 * prefix. So Rust calls the flavour C does, as C calls it.
 */
#define RUST_FN_HELPER(ret, name, params, args)				\
static inline ret RUST_FN_HELPER_##name params { return name args; }

RUST_FN_HELPER(void, rcu_init,			(void), ())
RUST_FN_HELPER(void, rcu_register_thread,	(void), ())
RUST_FN_HELPER(void, rcu_unregister_thread,	(void), ())
RUST_FN_HELPER(void, rcu_quiescent_state,	(void), ())
RUST_FN_HELPER(void, rcu_read_lock,		(void), ())
RUST_FN_HELPER(void, rcu_read_unlock,		(void), ())
RUST_FN_HELPER(void, synchronize_rcu,		(void), ())
RUST_FN_HELPER(void, rcu_barrier,		(void), ())
RUST_FN_HELPER(void, call_rcu,
	       (struct rcu_head *head, void (*func)(struct rcu_head *)), (head, func))
RUST_FN_HELPER(struct urcu_gp_poll_state, start_poll_synchronize_rcu, (void), ())
RUST_FN_HELPER(bool, poll_state_synchronize_rcu,
	       (struct urcu_gp_poll_state state), (state))
