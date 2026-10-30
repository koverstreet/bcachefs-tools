/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_CLOCK_H
#define _BCACHEFS_CLOCK_H

static inline void bch2_increment_clock(struct bch_fs *c, u64 sectors,
					int rw)
{
	struct io_clock *clock = &c->io_clock[rw];

	/* Past the batch: it's a u16, which a whole bucket would wrap */
	if (sectors >= IO_CLOCK_PCPU_SECTORS)
		__bch2_increment_clock(clock, sectors);
	else if (unlikely(this_cpu_add_return(*clock->pcpu_buf, sectors) >=
			  IO_CLOCK_PCPU_SECTORS))
		__bch2_increment_clock(clock, this_cpu_xchg(*clock->pcpu_buf, 0));
}

#endif /* _BCACHEFS_CLOCK_H */
