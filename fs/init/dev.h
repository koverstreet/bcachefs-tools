/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_DEV_H
#define _BCACHEFS_INIT_DEV_H

#include <linux/wait.h>

#include "sb/members.h"

/*
 * #918 per-device freeze/thaw (Design B: per-device write gate + steer/hold).
 *
 * freeze_state is a single word that doubles as the member's freeze lifecycle
 * and its WRITE GATE.  Derivation: the TLA model (DesignB.tla) couples the gate
 * to fstate by GateVsFstate ("gate closed <=> fstate in {freezing,frozen,ro}"),
 * and the generated state machine (ExecFreeze.h: enum fzlife, enum gate) uses
 * the same five lifecycle values.  The gate is CLOSED (writes park) only while
 * _freezing or _frozen; _thawing reopens it (candidate (c)); _ro relies on the
 * existing member-ro get_ioref(WRITE)=NULL path, so the parking gate is open
 * there and a released writer relocates or fails BLK_STS_REMOVED.
 */
enum bch_dev_freeze_state {
	BCH_DEV_FREEZE_thawed	= 0,
	BCH_DEV_FREEZE_freezing,
	BCH_DEV_FREEZE_frozen,
	BCH_DEV_FREEZE_thawing,
	BCH_DEV_FREEZE_ro,
};

enum bch_dev_freeze_mode {
	BCH_DEV_FREEZE_MODE_none	= 0,
	BCH_DEV_FREEZE_MODE_steer,
	BCH_DEV_FREEZE_MODE_hold,
	BCH_DEV_FREEZE_MODE_ro,
};

/* The per-device write gate.  Open unless the member is mid-freeze (_freezing
 * or _frozen); read lock-free on the write-submit path. */
static inline bool bch2_dev_freeze_gate_open(int st)
{
	return st != BCH_DEV_FREEZE_freezing && st != BCH_DEV_FREEZE_frozen;
}

static inline bool bch2_dev_idx_freeze_gate_open(struct bch_fs *c, unsigned dev)
{
	guard(rcu)();
	struct bch_dev *ca = bch2_dev_rcu_noerror(c, dev);
	/* a gone/removed member is not gated; its write resolves per BLK_STS_REMOVED */
	return !ca || bch2_dev_freeze_gate_open(atomic_read(&ca->freeze_state));
}

/*
 * Park a write that targets a HOLD-frozen member until the gate reopens.  The
 * writer holds NO per-device ref while parked (the condition re-looks-up under
 * rcu each check): candidate (c)'s lock-free thaw or an escape-to-ro's io_ref
 * drain must be able to make progress against the parked writer, which it
 * cannot if the writer pins an io_ref.  (Contract clause 3: a no-peer write
 * BLOCKS waiting for thaw; the block is reversible and never auto-released.)
 */
static inline void bch2_dev_idx_write_gate_wait(struct bch_fs *c, unsigned dev)
{
	if (likely(bch2_dev_idx_freeze_gate_open(c, dev)))
		return;

	/*
	 * #918 G9 fix: the gate is non-blocking.  The F*-generated C
	 * (step_issue_write) never blocks a gate-closed write — it parks
	 * (parked++, return) or steers.  A blocking wait_event here holds
	 * VFS sb_writers: bch2_fs_bdev_freeze (from dmsetup suspend) closes
	 * the gate while a user writer is mid-transaction, the writer blocks,
	 * fsfreeze blocks on sb_writers, and the gate never reopens.
	 *
	 * With the gate non-blocking, writes to frozen devices fall through
	 * to bch2_dev_get_ioref() — which returns NULL when the WRITE ref is
	 * stopped (STEER freeze) — and take the clean-completion path in
	 * bch2_submit_wbio_replicas.  For HOLD freeze (WRITE ref re-armed),
	 * the bio is completed cleanly without submit_bio (the underlying
	 * block device may be dm-suspended).  Journal replay reconciles.
	 */
}

void bch2_devs_list_to_text(struct printbuf *, struct bch_fs *, struct bch_devs_list *);

struct bch_fs *bch2_dev_to_fs(dev_t);

void bch2_dev_io_ref_stop(struct bch_dev *, int);
void bch2_dev_unlink(struct bch_dev *);
void bch2_dev_free(struct bch_dev *);
void __bch2_dev_offline(struct bch_fs *, struct bch_dev *);
int bch2_dev_sysfs_online(struct bch_fs *, struct bch_dev *);
void bch2_dev_read_identity(struct block_device *bdev,
			    char *name, size_t name_size,
			    char *model, size_t model_size,
			    char *serial, size_t serial_size);
int bch2_dev_alloc(struct bch_fs *, unsigned);
int bch2_dev_attach_bdev(struct bch_fs *, struct bch_sb_handle *, struct printbuf *);

bool bch2_dev_state_allowed(struct bch_fs *, struct bch_dev *,
			    enum bch_member_state, int,
			    struct printbuf *);
int __bch2_dev_set_state(struct bch_fs *, struct bch_dev *,
			 enum bch_member_state, int,
			 struct printbuf *);
int bch2_dev_set_state(struct bch_fs *, struct bch_dev *,
		       enum bch_member_state, int,
		       struct printbuf *);

int bch2_dev_add_initialize(struct bch_fs *, struct bch_dev *);

int bch2_dev_remove(struct bch_fs *, struct bch_dev *, int, struct printbuf *);
int bch2_dev_online(struct bch_fs *, const char *, struct printbuf *);
int bch2_dev_offline(struct bch_fs *, struct bch_dev *, int, struct printbuf *);

int __bch2_dev_resize_alloc(struct bch_dev *, u64, u64);

struct bch_dev *bch2_dev_lookup(struct bch_fs *, const char *);

extern const struct blk_holder_ops bch2_sb_handle_bdev_ops;

#endif /* _BCACHEFS_INIT_DEV_H */
