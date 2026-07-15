#ifndef __LINUX_BIT_SPINLOCK_H
#define __LINUX_BIT_SPINLOCK_H

#include <linux/kernel.h>
#include <linux/preempt.h>
#include <linux/futex.h>
#include <urcu/futex.h>

/*
 * The futex wait op wants an explicit 32-bit address and value. If the bitmap
 * used for the spinlock is 64-bit, cast down and pass the right 32-bit region
 * for the in-kernel checks. The value is the copy that has already been read
 * from the atomic op.
 *
 * The futex wake op interprets the value as the number of waiters to wake (up
 * to INT_MAX), so pass that along directly.
 */
static inline void do_futex(int nr, unsigned long *addr, unsigned long v, int futex_flags)
{
	u32 *addr32 = (u32 *) addr;
	u32 *v32 = (u32 *) &v;
	int shift = 0;

	futex_flags |= FUTEX_PRIVATE_FLAG;

#if BITS_PER_LONG == 64
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
	shift = (nr >= 32) ? 1 : 0;
#else
	shift = (nr < 32) ? 1 : 0;
#endif
#endif
	if (shift) {
		addr32 += shift;
		v32 += shift;
	}
	/*
	 * The shift to determine the futex address may have cast away a
	 * literal wake count value. The value is capped to INT_MAX and thus
	 * always in the low bytes of v regardless of bit nr. Copy in the wake
	 * count to whatever 32-bit range was selected.
	 */
	if (futex_flags == FUTEX_WAKE_PRIVATE)
		*v32 = (u32) v;
	futex(addr32, futex_flags, *v32, NULL, NULL, 0);
}

static inline void bit_spin_lock(int nr, unsigned long *_addr)
{
	unsigned long mask;
	unsigned long *addr = _addr + (nr / BITS_PER_LONG);
	unsigned long v;

	nr &= BITS_PER_LONG - 1;
	mask = 1UL << nr;

	while (1) {
		v = __atomic_fetch_or(addr, mask, __ATOMIC_ACQUIRE);
		if (!(v & mask))
			break;

		do_futex(nr, addr, v, FUTEX_WAIT);
	}
}

static inline void bit_spin_wake(int nr, unsigned long *_addr)
{
	do_futex(nr, _addr, INT_MAX, FUTEX_WAKE);
}

static inline void bit_spin_unlock(int nr, unsigned long *_addr)
{
	unsigned long mask;
	unsigned long *addr = _addr + (nr / BITS_PER_LONG);

	nr &= BITS_PER_LONG - 1;
	mask = 1UL << nr;

	__atomic_and_fetch(addr, ~mask, __ATOMIC_RELEASE);
	do_futex(nr, addr, INT_MAX, FUTEX_WAKE);
}

#endif /* __LINUX_BIT_SPINLOCK_H */

