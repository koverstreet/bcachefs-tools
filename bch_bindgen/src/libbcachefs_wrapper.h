#include "bcachefs_format.h"
#include "errcode.h"
#include "opts.h"

#include "btree/cache.h"
#include "btree/iter.h"
#include "data/checksum.h"
#include "debug/debug.h"
#include "init/dev.h"
#include "init/error.h"
#include "init/fs.h"
#include "init/passes.h"
#include "fs/check.h"
#include "fs/dirent.h"
#include "fs/namei.h"
#include "fs/inode.h"
#include "alloc/accounting.h"
#include "alloc/buckets.h"
#include "data/read.h"
#include "data/write.h"
#include "journal/init.h"
#include "journal/read.h"
#include "journal/seq_blacklist.h"
#include "sb/io.h"

#include "alloc/disk_groups.h"
#include "cmds.h"
#include "crypto.h"
#include "libbcachefs.h"
#include "raid/raid.h"
#include "sb/members.h"
#include "src/rust_to_c.h"
#include "rust_shims.h"

#include "include/linux/bio.h"
#include "include/linux/blkdev.h"

#include "c_src/fuse_shims.h"

/* Fix753 is a workaround for https://github.com/rust-lang/rust-bindgen/issues/753
 * Functional macro are not expanded with bindgen, e.g. ioctl are automatically ignored
 * from the generation
 *
 * To avoid this, use `MARK_FIX_753` to force the synthesis of your macro constant.
 * It will appear in Rust with its proper name and not Fix753_{name}.
 */

/* MARK_FIX_753: force generate a macro constant in Rust
 *
 * @type_name   - a type for this constant
 * @req_name    - a name for this constant which will be used inside of Rust
 */
#define MARK_FIX_753(type_name, req_name) const type_name Fix753_##req_name = req_name;

MARK_FIX_753(blk_mode_t, BLK_OPEN_READ);
MARK_FIX_753(blk_mode_t, BLK_OPEN_WRITE);
MARK_FIX_753(blk_mode_t, BLK_OPEN_EXCL);

