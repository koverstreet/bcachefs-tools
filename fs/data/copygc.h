/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_COPYGC_H
#define _BCACHEFS_COPYGC_H

s64 bch2_copygc_dev_wait_amount(struct bch_dev *);
void bch2_copygc_wait_to_text(struct printbuf *, struct bch_fs *);

bool bch2_copygc_can_make_progress(struct bch_dev *);

static inline void bch2_copygc_wakeup(struct bch_fs *c)
{
	c->copygc.kick_count++;
	guard(rcu)();
	struct task_struct *p = rcu_dereference(c->copygc.thread);
	if (p)
		wake_up_process(p);
}

static inline void bch2_copygc_wakeup_for_pressure(struct bch_fs *c)
{
	WRITE_ONCE(c->copygc.pressure_pending, true);
	bch2_copygc_wakeup(c);
}

void bch2_copygc_stop(struct bch_fs *);
int bch2_copygc_start(struct bch_fs *);

void bch2_fs_copygc_exit(struct bch_fs *);
void bch2_fs_copygc_init(struct bch_fs *);

#endif /* _BCACHEFS_COPYGC_H */
