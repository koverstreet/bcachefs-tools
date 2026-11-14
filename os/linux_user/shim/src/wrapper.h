#include <urcu.h>

#include "include/linux/bio.h"
#include "include/linux/blkdev.h"
#include "include/linux/delay.h"
#include "include/linux/device.h"
#include "include/linux/generic-radix-tree.h"
#include "include/linux/idr.h"
#include "include/linux/kernel.h"
#include "include/linux/llist.h"
#include "include/linux/lz4.h"
#include "include/linux/local_lock.h"
#include "include/linux/mm.h"
#include "include/linux/percpu-refcount.h"
#include "include/linux/percpu-rwsem.h"
#include "include/linux/posix_acl.h"
#include "include/linux/random.h"
#include "include/linux/refcount.h"
#include "include/linux/rcupdate.h"
#include "include/linux/sched.h"
#include "include/linux/seqlock.h"
#include "include/linux/srcu.h"
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
#include "fs/locking/shim.h"
#include "fs/util/shim.h"
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

#ifdef __BINDGEN__
/*
 * Macros with casts, which bindgen doesn't evaluate: as consts, which it
 * binds, by the names codegen.h's have in-kernel - build.rs strips the
 * prefix. BLK_OPEN_BUFFERED is the shim's own.
 */
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_READ	= BLK_OPEN_READ;
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_WRITE	= BLK_OPEN_WRITE;
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_EXCL	= BLK_OPEN_EXCL;
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_BUFFERED	= BLK_OPEN_BUFFERED;
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_CREAT	= BLK_OPEN_CREAT;
const blk_opf_t RUST_CONST_HELPER_REQ_SYNC		= REQ_SYNC;
const blk_opf_t RUST_CONST_HELPER_REQ_META		= REQ_META;
const blk_opf_t RUST_CONST_HELPER_REQ_IDLE		= REQ_IDLE;
const blk_opf_t RUST_CONST_HELPER_REQ_FUA		= REQ_FUA;
const blk_opf_t RUST_CONST_HELPER_REQ_PREFLUSH		= REQ_PREFLUSH;
const blk_status_t RUST_CONST_HELPER_BLK_STS_NOTSUPP		= BLK_STS_NOTSUPP;
const blk_status_t RUST_CONST_HELPER_BLK_STS_TIMEOUT		= BLK_STS_TIMEOUT;
const blk_status_t RUST_CONST_HELPER_BLK_STS_NOSPC		= BLK_STS_NOSPC;
const blk_status_t RUST_CONST_HELPER_BLK_STS_TRANSPORT		= BLK_STS_TRANSPORT;
const blk_status_t RUST_CONST_HELPER_BLK_STS_TARGET		= BLK_STS_TARGET;
const blk_status_t RUST_CONST_HELPER_BLK_STS_RESV_CONFLICT	= BLK_STS_RESV_CONFLICT;
const blk_status_t RUST_CONST_HELPER_BLK_STS_MEDIUM		= BLK_STS_MEDIUM;
const blk_status_t RUST_CONST_HELPER_BLK_STS_PROTECTION		= BLK_STS_PROTECTION;
const blk_status_t RUST_CONST_HELPER_BLK_STS_RESOURCE		= BLK_STS_RESOURCE;
const blk_status_t RUST_CONST_HELPER_BLK_STS_IOERR		= BLK_STS_IOERR;
const blk_status_t RUST_CONST_HELPER_BLK_STS_DM_REQUEUE		= BLK_STS_DM_REQUEUE;
const blk_status_t RUST_CONST_HELPER_BLK_STS_AGAIN		= BLK_STS_AGAIN;
const blk_status_t RUST_CONST_HELPER_BLK_STS_DEV_RESOURCE	= BLK_STS_DEV_RESOURCE;
const blk_status_t RUST_CONST_HELPER_BLK_STS_ZONE_OPEN_RESOURCE	= BLK_STS_ZONE_OPEN_RESOURCE;
const blk_status_t RUST_CONST_HELPER_BLK_STS_ZONE_ACTIVE_RESOURCE = BLK_STS_ZONE_ACTIVE_RESOURCE;
const blk_status_t RUST_CONST_HELPER_BLK_STS_OFFLINE		= BLK_STS_OFFLINE;
const blk_status_t RUST_CONST_HELPER_BLK_STS_DURATION_LIMIT	= BLK_STS_DURATION_LIMIT;
const blk_status_t RUST_CONST_HELPER_BLK_STS_INVAL		= BLK_STS_INVAL;
#endif
