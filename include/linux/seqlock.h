#ifndef __LINUX_SEQLOCK_H
#define __LINUX_SEQLOCK_H

#include <linux/compiler.h>

typedef struct seqcount {
	unsigned sequence;
} seqcount_t;

static inline void seqcount_init(seqcount_t *s)
{
	s->sequence = 0;
}

static inline unsigned read_seqcount_begin(const seqcount_t *s)
{
	unsigned ret;

repeat:
	ret = READ_ONCE(s->sequence);
	if (unlikely(ret & 1)) {
		cpu_relax();
		goto repeat;
	}
	smp_rmb();
	return ret;
}

static inline int read_seqcount_retry(const seqcount_t *s, unsigned start)
{
	smp_rmb();
	return unlikely(s->sequence != start);
}

static inline void write_seqcount_begin(seqcount_t *s)
{
	s->sequence++;
	smp_wmb();
}

static inline void write_seqcount_end(seqcount_t *s)
{
	smp_wmb();
	s->sequence++;
}

#endif /* __LINUX_SEQLOCK_H */
