#include <urcu.h>

#include "include/linux/bio.h"
#include "include/linux/blkdev.h"
#include "include/linux/crc32c.h"
#include "include/linux/crc64.h"
#include "include/linux/debugfs.h"
#include "include/linux/device.h"
#include "include/linux/generic-radix-tree.h"
#include "include/linux/idr.h"
#include "include/linux/kernel.h"
#include "include/linux/llist.h"
#include "include/linux/local_lock.h"
#include "include/linux/mm.h"
#include "include/linux/percpu-refcount.h"
#include "include/linux/percpu-rwsem.h"
#include "include/linux/posix_acl.h"
#include "include/linux/ratelimit.h"
#include "include/linux/refcount.h"
#include "include/linux/rcupdate.h"
#include "include/linux/rhashtable.h"
#include "include/linux/sched.h"
#include "include/linux/semaphore.h"
#include "include/linux/seqlock.h"
#include "include/linux/srcu.h"
#include "include/linux/string.h"
#include "include/linux/uuid.h"
#include "include/linux/unicode.h"
#include "include/linux/wait_bit.h"
#include "include/linux/xattr.h"
#include "include/crypto/sha2.h"	/* after linux/: it isn't self-contained */
#include "fs/vendor/closure.h"

#include <errno.h>

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
const blk_opf_t RUST_CONST_HELPER_REQ_SYNC		= REQ_SYNC;
const blk_opf_t RUST_CONST_HELPER_REQ_META		= REQ_META;
const blk_opf_t RUST_CONST_HELPER_REQ_IDLE		= REQ_IDLE;
const blk_opf_t RUST_CONST_HELPER_REQ_FUA		= REQ_FUA;
#endif
