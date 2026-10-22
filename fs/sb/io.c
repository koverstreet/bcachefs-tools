// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "alloc/disk_groups.h"
#include "alloc/replicas.h"

#include "data/checksum.h"
#include "data/extents_sb_gen.h"

#include "journal/journal.h"
#include "journal/sb.h"
#include "journal/seq_blacklist.h"

#include "fs/inode.h"
#include "fs/quota.h"

#include "init/dev.h"
#include "init/error.h"
#include "init/fs.h"
#include "init/passes.h"

#include "sb/clean.h"
#include "sb/counters.h"
#include "sb/downgrade.h"
#include "sb/errors.h"
#include "sb/members.h"
#include "sb/io.h"

#include "util/vstructs.h"

/* DOC_LATEX(superblock)
 * \subsubsection{Layout and redundancy}
 *
 * The primary superblock is located at sector 8 (4\,KB from the start of the
 * device). A \texttt{bch\_sb\_layout} structure at sector 7 records the locations
 * of all superblock copies---typically three: the primary, one immediately
 * following it, and one at the end of the device. Up to 61 backup locations can
 * be recorded. The layout structure has its own magic number so that it can be
 * found independently.
 *
 * Only the primary superblock and the layout structure are at fixed,
 * known-without-context offsets. Backup superblock locations are recorded only
 * inside the layout (and inside each superblock's own embedded layout copy);
 * recovery from a lost layout therefore requires either falling back to a
 * second known offset, or scanning the device. The end-of-device backup is at
 * an offset computable from the device size, which gives a second escape valve
 * when sector 7 itself is unreadable.
 *
 * The layout structure is intentionally minimal---a magic number, a small
 * offset table, no checksum---and lives in its own 512-byte sector so that it
 * shares a physical block with as little else as possible. All checksumming
 * and validation logic lives in the superblock itself. The embedded
 * \texttt{bch\_sb\_layout} inside each superblock is what's used in the
 * common case (the standalone copy at sector 7 is consulted only when the
 * primary superblock cannot be read).
 *
 * The superblock is written with a monotonically increasing sequence number
 * (\texttt{seq}); on read, the copy with the highest valid sequence number is
 * authoritative. The \texttt{bcachefs recover-super} command can reconstruct a
 * device's superblock from backup copies or from another device in the same
 * filesystem.
 *
 * \subsubsection{Threat model}
 *
 * Redundancy exists to survive several distinct failure modes, in roughly
 * descending order of how often they're observed in practice:
 *
 * \begin{itemize}
 * \item \textbf{Torn writes during power loss}. The actual superblock size
 *   varies---a few kilobytes on a single-device laptop filesystem, larger on
 *   big multi-device filesystems---but is generally well beyond a single
 *   sector or physical block, and the on-disk reservation can grow to 32\,MB.
 *   That's beyond any device's atomic write granularity, so power loss
 *   mid-write can leave a torn superblock; the checksum catches it and a
 *   backup is consulted. This was the original driver for redundant
 *   superblocks.
 * \item \textbf{Media errors and bit rot}. Bad sectors, single-sector unrecoverable
 *   reads, slow long-term decay on backup copies that are written infrequently.
 *   Front-of-device and end-of-device backups are physically separated on
 *   rotating media, providing some independence.
 * \item \textbf{Legacy bootloader coexistence and large physical block sizes}.
 *   Sector 7 is adjacent to sector 0 (where ancient-style bootloaders live)
 *   and the start of the primary superblock (sector 8). On disks with logical
 *   sector size 512 and physical block size $\geq$\,4\,KB---increasingly
 *   common, especially on SSDs reporting 8\,KB or 16\,KB physical blocks---a
 *   sub-physical-block write triggers a read-modify-write of the whole
 *   physical block. A torn RMW on physical block 0 can take out the boot
 *   sector, the layout, and the start of the primary superblock together.
 *   The end-of-device backup superblock is the escape valve here, since it
 *   lives in its own physical block far away.
 * \item \textbf{Whole-device failure}. A single device dropping out: covered
 *   trivially by replication of the filesystem-wide state across all member
 *   devices' superblocks.
 * \end{itemize}
 *
 * \subsubsection{Fixed fields}
 *
 * The superblock header contains:
 *
 * \begin{itemize}
 * \item \textbf{Identity}: filesystem UUID (immutable), user-visible UUID
 *   (mutable), filesystem label (up to 32 bytes)
 * \item \textbf{Geometry}: block size, btree node size, number of devices
 * \item \textbf{Versioning}: current metadata version, minimum version of any
 *   data still on disk (see the Metadata versions section)
 * \item \textbf{State}: initialized and clean flags, sequence number, write
 *   timestamp
 * \item \textbf{Options}: all persistent filesystem options are encoded as
 *   bitfields in the superblock flags---replication counts, checksum and
 *   compression types, error handling policy, targets, quotas, journal
 *   parameters, and more. Mount options override these at runtime;
 *   \texttt{bcachefs set-fs-option} persists changes.
 * \end{itemize}
 *
 * \subsubsection{Variable-length fields}
 *
 * The superblock is extensible via type-tagged variable-length fields
 * (\texttt{BCH\_SB\_FIELD\_*}). Some are per-device (journal bucket lists);
 * most are shared across all devices (members, encryption, replicas, disk
 * groups, error log, recovery state). See the On disk format section for the
 * complete field list.
 *
 * Key fields for operators:
 *
 * \begin{description}
 * \item[\texttt{members\_v2}] Per-\hyperref[sec:devices]{device} metadata: UUID,
 *   bucket count and size, state (rw/ro/evacuating/spare), durability,
 *   data-type restrictions, error counters, performance measurements, and
 *   hardware identifiers.
 * \item[\texttt{disk\_groups}] Device label hierarchy and group definitions,
 *   used for target-based allocation (see
 *   \hyperref[sec:disk-groups]{Device labels and targets}).
 * \item[\texttt{replicas}] All unique replication configurations in the
 *   filesystem (see Replicas tracking).
 * \item[\texttt{clean}] Written on clean shutdown: contains btree roots and
 *   usage counters, allowing the next mount to skip
 *   \hyperref[sec:journal]{journal} replay entirely.
 * \item[\texttt{errors}] Persistent error log recording operational errors and
 *   fsck findings across mounts.
 * \item[\texttt{ext}] Extended metadata including required recovery passes and
 *   silenced errors.
 * \end{description}
 *
 * \subsubsection{Version upgrades}
 *
 * The superblock records both the current metadata version and the minimum
 * version of any data still on disk. This two-version scheme allows the
 * filesystem to upgrade incrementally: new data is written with the current
 * version while old data retains the format it was written with. The
 * \texttt{version\_upgrade} option controls upgrade behavior at mount time:
 * \texttt{compatible} (allow new features), \texttt{incompatible} (upgrade to
 * latest), or \texttt{none} (don't upgrade). Downgrade information is stored
 * separately so that a filesystem can be safely used by older tools after an
 * upgrade if no incompatible features have been used.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * Every superblock copy is checksummed; reads validate the checksum and fall
 * back to alternative copies on failure. The sequence number provides
 * unambiguous ordering when copies disagree. The \texttt{recover-super} command
 * can reconstruct a completely overwritten superblock from the backup copies on
 * the same device or from any other device in the filesystem. Recovery passes
 * \texttt{check\_alloc\_info} and \texttt{check\_topology} verify that
 * superblock-recorded state matches the actual on-disk data.
 */

#include <linux/backing-dev.h>
#include <linux/sort.h>
#include <linux/string_choices.h>

/* write superblock: */

static void write_super_endio(struct bio *bio)
{
	struct bch_dev *ca = bio->bi_private;

	bch2_account_io_success_fail(ca, bio_data_dir(bio), !bio->bi_status);

	if (bio->bi_status)
		ca->sb_write_error =
			__bch2_err_throw(ca->fs, -blk_status_to_bch_err(bio->bi_status));

	closure_put(&ca->fs->sb_write);
}

static void read_back_super(struct bch_fs *c, struct bch_dev *ca)
{
	struct bch_sb *sb = ca->disk_sb.sb;
	struct bio *bio = ca->disk_sb.bio;

	memset(ca->sb_read_scratch, 0, BCH_SB_READ_SCRATCH_BUF_SIZE);

	bio_reset(bio, ca->disk_sb.bdev, REQ_OP_READ|REQ_SYNC|REQ_META);
	bio->bi_iter.bi_sector	= le64_to_cpu(sb->layout.sb_offset[0]);
	bio->bi_end_io		= write_super_endio;
	bio->bi_private		= ca;
	bch2_bio_map(bio, ca->sb_read_scratch, BCH_SB_READ_SCRATCH_BUF_SIZE);

	this_cpu_add(ca->io_done->sectors[READ][BCH_DATA_sb], bio_sectors(bio));

	closure_bio_submit(bio, &c->sb_write);
}

static void write_one_super(struct bch_fs *c, struct bch_dev *ca, unsigned idx)
{
	struct bch_sb *sb = ca->disk_sb.sb;
	struct bio *bio = ca->disk_sb.bio;

	sb->offset = sb->layout.sb_offset[idx];

	SET_BCH_SB_CSUM_TYPE(sb, bch2_csum_opt_to_type(c->opts.metadata_checksum, false));
	sb->csum = csum_vstruct(c, BCH_SB_CSUM_TYPE(sb),
				null_nonce(), sb);

	/*
	 * blk-wbt.c throttles all writes except those that have both REQ_SYNC
	 * and REQ_IDLE set...
	 */

	bio_reset(bio, ca->disk_sb.bdev, REQ_OP_WRITE|REQ_SYNC|REQ_IDLE|REQ_META|REQ_FUA);
	bio->bi_iter.bi_sector	= le64_to_cpu(sb->offset);
	bio->bi_end_io		= write_super_endio;
	bio->bi_private		= ca;
	bch2_bio_map(bio, sb,
		     roundup((size_t) vstruct_bytes(sb),
			     bdev_logical_block_size(ca->disk_sb.bdev)));

	this_cpu_add(ca->io_done->sectors[WRITE][BCH_DATA_sb],
		     bio_sectors(bio));

	closure_bio_submit(bio, &c->sb_write);
}

typedef struct {
	int		err;
	u64		offset;
} sb_offset_err;
DEFINE_DARRAY(sb_offset_err);

typedef struct {
	struct bch_dev		*ca;
	darray_sb_offset_err	failures;
} write_sb_dev;

static void write_sb_dev_put(write_sb_dev d)
{
	enumerated_ref_put(&d.ca->io_ref[READ], BCH_DEV_READ_REF_write_super);
	darray_exit(&d.failures);
}

DEFINE_DARRAY_FREE_ITEM(write_sb_dev, write_sb_dev_put);

/*
 * @devs: restrict the write to these devices, or NULL for every online member.
 *
 * Everything downstream iterates online_devices - the readback pass, the write
 * loop, the error report - so filtering here is the only place the restriction
 * has to be applied.
 */
static int __bch2_write_super(struct bch_fs *c, const struct bch_devs_mask *devs,
			      enum bch_sb_write_flags flags)
{
	struct closure *cl = &c->sb_write;
	unsigned degraded_flags = BCH_FORCE_IF_DEGRADED;
	CLASS(darray_write_sb_dev, online_devices)();

	if (!test_bit(BCH_FS_may_upgrade_downgrade, &c->flags))
		return 0;

	/*
	 * Before a start has begun, the in-memory superblock holds what the open
	 * decided - version upgrade/downgrade, last_mount - for a start to act
	 * on; an open that never starts mustn't persist that. So a write before
	 * start is either part of bringing the filesystem up (as recovery's own
	 * are), or from an open that said it will never start, which skips
	 * those decisions:
	 */
	if (!test_bit(BCH_FS_start_begun, &c->flags) &&
	    !c->opts.will_not_start &&
	    !(flags & BCH_SB_WRITE_bringup))
		return bch_err_throw(c, erofs_sb_write_before_start);

	event_inc_trace(c, write_super, buf);

	if (c->opts.degraded == BCH_DEGRADED_very)
		degraded_flags |= BCH_FORCE_IF_LOST;

	lockdep_assert_held(&c->sb_lock.lock);

	closure_init_stack(cl);

	if (bch2_sb_has_journal(c->disk_sb.sb))
		bch2_fs_mark_dirty(c);
	else
		bch2_fs_mark_clean(c);

	/*
	 * Note: we do writes to RO devices here, and we might want to change
	 * that in the future.
	 *
	 * For now, we expect to be able to call write_super() when we're not
	 * yet RW:
	 */
	for_each_online_member(c, ca, BCH_DEV_READ_REF_write_super) {
		if (devs && !test_bit(ca->dev_idx, devs->d))
			continue;

		int ret = darray_push(&online_devices, ((write_sb_dev) { ca }));
		if (bch2_fs_fatal_err_on(ret, c, "%s: error allocating online devices", __func__)) {
			enumerated_ref_put(&ca->io_ref[READ], BCH_DEV_READ_REF_write_super);
			return ret;
		}
		enumerated_ref_get(&ca->io_ref[READ], BCH_DEV_READ_REF_write_super);
	}

	/* Make sure we're using the new magic numbers: */
	c->disk_sb.sb->magic = BCHFS_MAGIC;
	c->disk_sb.sb->layout.magic = BCHFS_MAGIC;

	le64_add_cpu(&c->disk_sb.sb->seq, 1);

	struct bch_sb_field_members_v2 *mi = bch2_sb_field_get(c->disk_sb.sb, members_v2);
	darray_for_each(online_devices, i)
		__bch2_members_v2_get_mut(mi, i->ca->dev_idx)->seq = c->disk_sb.sb->seq;
	c->disk_sb.sb->write_time = cpu_to_le64(ktime_get_real_seconds());

	if (test_bit(BCH_FS_error, &c->flags))
		SET_BCH_SB_HAS_ERRORS(c->disk_sb.sb, 1);
	if (test_bit(BCH_FS_topology_error, &c->flags))
		SET_BCH_SB_HAS_TOPOLOGY_ERRORS(c->disk_sb.sb, 1);

	SET_BCH_SB_BIG_ENDIAN(c->disk_sb.sb, CPU_BIG_ENDIAN);

	bch2_sb_counters_from_cpu(c);
	bch2_sb_members_from_cpu(c);
	bch2_sb_members_cpy_v2_v1(&c->disk_sb);
	bch2_sb_errors_from_cpu(c);
	bch2_sb_downgrade_update(c);
	try(bch2_sb_extent_type_u64s_from_cpu(c));

	darray_for_each(online_devices, i)
		bch2_sb_from_fs(c, i->ca);

	darray_for_each(online_devices, i) {
		struct bch_opts opts = bch2_opts_empty();
		CLASS(printbuf, err)();

		int ret = bch2_sb_validate(&i->ca->disk_sb, &opts, 0, BCH_VALIDATE_write, &err);
		if (ret) {
			bch2_fs_inconsistent(c, "sb invalid before write: %s", err.buf);
			return 0;
		}
	}

	if (c->opts.nochanges)
		return 0;

	/*
	 * Defer writing the superblock until filesystem initialization is
	 * complete - don't write out a partly initialized superblock.
	 *
	 * An offline edit (will_not_start) on a filesystem that has never been
	 * started has nothing to defer to: say so, rather than let it report
	 * success having written nothing.
	 */
	if (!BCH_SB_INITIALIZED(c->disk_sb.sb)) {
		if (!c->opts.will_not_start)
			return 0;

		bch_err(c, "filesystem has never been started, not writing superblock: mount it once first");
		return bch_err_throw(c, erofs_sb_never_started);
	}

	if (le16_to_cpu(c->disk_sb.sb->version) > bcachefs_metadata_version_current) {
		CLASS(printbuf, buf)();
		prt_printf(&buf, "attempting to write superblock that wasn't version downgraded (");
		bch2_version_to_text(&buf, le16_to_cpu(c->disk_sb.sb->version));
		prt_str(&buf, " > ");
		bch2_version_to_text(&buf, bcachefs_metadata_version_current);
		prt_str(&buf, ")");
		bch2_fs_fatal_error(c, ": %s", buf.buf);
		return bch_err_throw(c, sb_not_downgraded);
	}

	struct bch_devs_mask sb_written = {};

	darray_for_each(online_devices, i)
		i->ca->sb_write_error = 0;

	darray_for_each(online_devices, i)
		read_back_super(c, i->ca);
	closure_sync(cl);

	darray_for_each(online_devices, i) {
		struct bch_dev *ca = i->ca;

		if (ca->sb_write_error)
			continue;

		if (le64_to_cpu(ca->sb_read_scratch->seq) < ca->disk_sb.seq) {
			CLASS(bch_log_msg, msg)(c);
			prt_bdevname(&msg.m, ca->disk_sb.bdev);
			prt_printf(&msg.m, ": Superblock write was silently dropped! (seq %llu expected %llu)",
				   le64_to_cpu(ca->sb_read_scratch->seq),
				   ca->disk_sb.seq);

			if (c->opts.errors != BCH_ON_ERROR_continue &&
			    c->opts.errors != BCH_ON_ERROR_fix_safe) {
				bch2_fs_emergency_read_only(c, &msg.m);
				return bch_err_throw(c, erofs_sb_err);
			}
		}

		if (le64_to_cpu(ca->sb_read_scratch->seq) > ca->disk_sb.seq) {
			CLASS(bch_log_msg, msg)(c);
			prt_bdevname(&msg.m, ca->disk_sb.bdev);
			prt_printf(&msg.m, ": Superblock modified by another process (seq %llu expected %llu)",
				   le64_to_cpu(ca->sb_read_scratch->seq),
				   ca->disk_sb.seq);
			bch2_fs_emergency_read_only(c, &msg.m);
			return bch_err_throw(c, erofs_sb_err);
		}
	}

	unsigned sb = 0;
	bool have_errors = false, wrote;
	do {
		darray_for_each(online_devices, i)
			i->ca->sb_write_error = 0;

		wrote = false;
		darray_for_each(online_devices, i) {
			struct bch_sb_layout *l = &i->ca->disk_sb.sb->layout;
			if (sb >= l->nr_superblocks)
				continue;

			write_one_super(c, i->ca, sb);
			wrote = true;
		}

		closure_sync(cl);

		darray_for_each(online_devices, i) {
			struct bch_sb_layout *l = &i->ca->disk_sb.sb->layout;
			if (sb >= l->nr_superblocks)
				continue;

			if (i->ca->sb_write_error) {
				darray_push(&i->failures, ((sb_offset_err) {
							   i->ca->sb_write_error,
							   le64_to_cpu(l->sb_offset[sb])
				}));
				have_errors = true;
			} else {
				__set_bit(i->ca->dev_idx, sb_written.d);
			}
		}
		sb++;
	} while (wrote);

	darray_for_each(online_devices, i)
		if (test_bit(i->ca->dev_idx, sb_written.d))
			i->ca->disk_sb.seq = le64_to_cpu(i->ca->disk_sb.sb->seq);

	unsigned nr_wrote =	dev_mask_nr(&sb_written);
	unsigned nr_members =	devs
		? online_devices.nr
		: bch2_sb_nr_devices(c->disk_sb.sb);

	/*
	 * A restricted write is not trying to be readable on its own - by
	 * construction it skips devices that hold data. Asking
	 * bch2_can_read_fs_with_devs() about it would say no every time and
	 * take us read only. What matters is that the write landed somewhere:
	 * the previous superblock is still on every device, so the failure
	 * here is a missed update, not a lost one.
	 */
	bool fatal = !nr_wrote ||
		(!devs && !bch2_can_read_fs_with_devs(c, &sb_written, degraded_flags, NULL));

	if (!have_errors && !fatal)
		return 0;

	CLASS(bch_log_msg, msg)(c);

	/*
	 * A degraded write (wrote to fewer devices, but still enough to mount)
	 * repeats on every superblock write while a device is failing, so
	 * ratelimit it. A fatal failure goes emergency read-only below and
	 * always prints.
	 */
	if (!fatal)
		msg.m.suppress = bch2_ratelimit(c);

	prt_printf(&msg.m, "Error writing superblock, wrote to %u/%u devices:\n",
		   nr_wrote, nr_members);

	struct bch_devs_mask sb_unwritten;
	memset(sb_unwritten.d, 0xFF, sizeof(sb_unwritten));

	darray_for_each(online_devices, i) {
		__clear_bit(i->ca->dev_idx, sb_unwritten.d);
		bch2_member_to_text_short_locked(&msg.m, c, i->ca);

		if (i->failures.nr) {
			darray_for_each(i->failures, j)
				prt_printf(&msg.m, " %llu=%s", j->offset, bch2_err_str(j->err));

			if (!test_bit(i->ca->dev_idx, sb_written.d))
				prt_str(&msg.m, " (all failed)");
			else
				prt_str(&msg.m, " (partial success)");
		} else {
			prt_str(&msg.m, " (success)");
		}
		prt_newline(&msg.m);
	}

	/*
	 * For a restricted write the leftovers are the devices we deliberately
	 * skipped, not devices that went away - don't call them offline.
	 */
	prt_printf(&msg.m, devs ? "Not written:\n" : "Offline devices:\n");
	scoped_guard(printbuf_indent, &msg.m)
		bch2_devs_mask_to_text_locked(&msg.m, c, &sb_unwritten);

	if (fatal) {
		prt_printf(&msg.m, "Unable to write superblock to sufficient devices (from %ps)\n",
			   (void *) _RET_IP_);
		prt_printf(&msg.m, "Would not be able to mount with written devices\n");
		bch2_can_read_fs_with_devs(c, &sb_written, degraded_flags, &msg.m);
		bch2_fs_emergency_read_only(c, &msg.m);
		return bch_err_throw(c, erofs_sb_err);
	}

	/*
	 * Not fatal: we wrote to fewer devices than we wanted, but enough that
	 * the filesystem still mounts. The superblock is on disk, so the caller
	 * got what it asked for - the message above is a warning, not a failure.
	 */
	return 0;
}

static int bch2_write_super_devs(struct bch_fs *c, const struct bch_devs_mask *devs,
				 enum bch_sb_write_flags flags)
{
	u64 start_time = local_clock();

	int ret = __bch2_write_super(c, devs, flags);
	/* Make new options visible after they're persistent: */
	bch2_sb_update(c);

	bch2_time_stats_update(&c->times[BCH_TIME_sb_write], start_time);
	return ret;
}

int bch2_write_super_flags(struct bch_fs *c, enum bch_sb_write_flags flags)
{
	return bch2_write_super_devs(c, NULL, flags);
}

int bch2_write_super(struct bch_fs *c)
{
	return bch2_write_super_flags(c, 0);
}

/*
 * How much of the filesystem's metadata we're willing to leave on devices a
 * replicas superblock write skips. Tunable: lower is more conservative.
 */
/*
 * How much of the filesystem's metadata we're willing to leave on devices a
 * replicas superblock write skips. Tunable: lower is more conservative.
 */
#define BCH_SB_SKIP_META_PCT		10

/*
 * Btree only.
 *
 * Not journal: that's buckets allocated to the journal, not the live entries
 * in them, so it's both the wrong quantity and a constantly moving one.
 *
 * Not sb either: every device has one and they're all the same size, so it
 * only dilutes the ranking - and on a filesystem with little btree it would
 * dominate, leaving every device tied. Without it that case totals zero, and
 * we write to everyone, which is the right answer.
 */
static u64 dev_meta_sectors(struct bch_dev *ca)
{
	struct bch_dev_usage_full u = bch2_dev_usage_full_read(ca);

	return u.d[BCH_DATA_btree].sectors;
}

typedef struct {
	u8	dev_idx;
	u64	sectors;
} dev_meta;
DEFINE_DARRAY(dev_meta);

static int dev_meta_cmp(const void *_l, const void *_r)
{
	const dev_meta *l = _l, *r = _r;

	/* Least metadata first: */
	return cmp_int(l->sectors, r->sectors);
}

/*
 * Superblock write for a new replicas entry - the one that happens constantly
 * on a big multi device filesystem, because the journal wanders and every
 * device set it lands on is a new entry, and every entry costs a barrier
 * against the slowest disk in the fleet.
 *
 * Such a write may skip the devices that hold little metadata, because an
 * entry that doesn't reach them isn't lost: the previous superblock is still
 * on every device, the next fleet wide write catches them up, and a mount
 * that has only them rebuilds the replicas section from accounting - see
 * accounting_read_mem_fixups(), which does that on every mount already.
 *
 * Start from every online device and drop rotating disks while the metadata
 * left behind stays under BCH_SB_SKIP_META_PCT. Dropping rather than picking
 * keeps the default on the safe side, and only a rotating disk is ever
 * dropped - an all-SSD filesystem keeps its whole fleet however the metadata
 * is spread.
 *
 * Cheapest first, because the number of devices dropped is the whole point:
 * with a budget of 10%, one disk holding 10% and ten holding 1% each gets us
 * one device unsorted and ten sorted. Sorted also lets us stop at the first
 * disk that doesn't fit - everything after it holds at least as much.
 *
 * This is the *only* caller allowed to write to a subset, which is why it's
 * named for the caller rather than for the mechanism: every other superblock
 * section stays fleet wide, so nothing else can go stale behind us.
 */
int bch2_write_super_replicas(struct bch_fs *c)
{
	CLASS(darray_dev_meta, m)();
	struct bch_devs_mask devs;
	u64 total = 0, skipped = 0;
	unsigned nr_skipped = 0;

	memset(&devs, 0, sizeof(devs));

	for_each_online_member(c, ca, BCH_DEV_READ_REF_write_super) {
		u64 sectors = dev_meta_sectors(ca);

		if (darray_push(&m, ((dev_meta) { ca->dev_idx, sectors }))) {
			enumerated_ref_put(&ca->io_ref[READ], BCH_DEV_READ_REF_write_super);
			return bch2_write_super(c);
		}

		__set_bit(ca->dev_idx, devs.d);
		total += sectors;
	}

	if (!total)
		return bch2_write_super(c);

	darray_sort(m, dev_meta_cmp);

	u64 budget = total / 100 * BCH_SB_SKIP_META_PCT;

	darray_for_each(m, i) {
		if (!test_bit(i->dev_idx, c->devs_rotational.d))
			continue;

		if (skipped + i->sectors > budget)
			break;

		skipped += i->sectors;
		__clear_bit(i->dev_idx, devs.d);
		nr_skipped++;
	}

	return nr_skipped
		? bch2_write_super_devs(c, &devs, 0)
		: bch2_write_super(c);
}
