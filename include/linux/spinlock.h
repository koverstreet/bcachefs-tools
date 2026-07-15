/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __LINUX_SPINLOCK_H
#define __LINUX_SPINLOCK_H

#include <linux/cleanup.h>
#include "linux/spinlock_types.h"

#if 0
DEFINE_LOCK_GUARD_1(raw_spinlock, raw_spinlock_t,
		    raw_spin_lock(_T->lock),
		    raw_spin_unlock(_T->lock))

DEFINE_LOCK_GUARD_1_COND(raw_spinlock, _try, raw_spin_trylock(_T->lock))

DEFINE_LOCK_GUARD_1(raw_spinlock_nested, raw_spinlock_t,
		    raw_spin_lock_nested(_T->lock, SINGLE_DEPTH_NESTING),
		    raw_spin_unlock(_T->lock))

DEFINE_LOCK_GUARD_1(raw_spinlock_irq, raw_spinlock_t,
		    raw_spin_lock_irq(_T->lock),
		    raw_spin_unlock_irq(_T->lock))

DEFINE_LOCK_GUARD_1_COND(raw_spinlock_irq, _try, raw_spin_trylock_irq(_T->lock))

DEFINE_LOCK_GUARD_1(raw_spinlock_bh, raw_spinlock_t,
		    raw_spin_lock_bh(_T->lock),
		    raw_spin_unlock_bh(_T->lock))

DEFINE_LOCK_GUARD_1_COND(raw_spinlock_bh, _try, raw_spin_trylock_bh(_T->lock))

DEFINE_LOCK_GUARD_1(raw_spinlock_irqsave, raw_spinlock_t,
		    raw_spin_lock_irqsave(_T->lock, _T->flags),
		    raw_spin_unlock_irqrestore(_T->lock, _T->flags),
		    unsigned long flags)

DEFINE_LOCK_GUARD_1_COND(raw_spinlock_irqsave, _try,
			 raw_spin_trylock_irqsave(_T->lock, _T->flags))
#endif

DEFINE_LOCK_GUARD_1(spinlock, spinlock_t,
		    spin_lock(_T->lock),
		    spin_unlock(_T->lock))

DEFINE_LOCK_GUARD_1(spinlock_irqsave, spinlock_t,
		    spin_lock(_T->lock),
		    spin_unlock(_T->lock))

DEFINE_LOCK_GUARD_1_COND(spinlock, _try, spin_trylock(_T->lock))
DEFINE_LOCK_GUARD_1(spinlock_irq, spinlock_t,
		    spin_lock_irq(_T->lock),
		    spin_unlock_irq(_T->lock))

DEFINE_LOCK_GUARD_1(raw_spinlock, spinlock_t,
		    spin_lock(_T->lock),
		    spin_unlock(_T->lock))

#if 0
DEFINE_LOCK_GUARD_1_COND(spinlock_irq, _try,
			 spin_trylock_irq(_T->lock))

DEFINE_LOCK_GUARD_1(spinlock_bh, spinlock_t,
		    spin_lock_bh(_T->lock),
		    spin_unlock_bh(_T->lock))

DEFINE_LOCK_GUARD_1_COND(spinlock_bh, _try,
			 spin_trylock_bh(_T->lock))

DEFINE_LOCK_GUARD_1(spinlock_irqsave, spinlock_t,
		    spin_lock_irqsave(_T->lock, _T->flags),
		    spin_unlock_irqrestore(_T->lock, _T->flags),
		    unsigned long flags)

DEFINE_LOCK_GUARD_1_COND(spinlock_irqsave, _try,
			 spin_trylock_irqsave(_T->lock, _T->flags))

DEFINE_LOCK_GUARD_1(read_lock, rwlock_t,
		    read_lock(_T->lock),
		    read_unlock(_T->lock))

DEFINE_LOCK_GUARD_1(read_lock_irq, rwlock_t,
		    read_lock_irq(_T->lock),
		    read_unlock_irq(_T->lock))

DEFINE_LOCK_GUARD_1(read_lock_irqsave, rwlock_t,
		    read_lock_irqsave(_T->lock, _T->flags),
		    read_unlock_irqrestore(_T->lock, _T->flags),
		    unsigned long flags)

DEFINE_LOCK_GUARD_1(write_lock, rwlock_t,
		    write_lock(_T->lock),
		    write_unlock(_T->lock))

DEFINE_LOCK_GUARD_1(write_lock_irq, rwlock_t,
		    write_lock_irq(_T->lock),
		    write_unlock_irq(_T->lock))

DEFINE_LOCK_GUARD_1(write_lock_irqsave, rwlock_t,
		    write_lock_irqsave(_T->lock, _T->flags),
		    write_unlock_irqrestore(_T->lock, _T->flags),
		    unsigned long flags)
#endif

#endif /* __LINUX_SPINLOCK_H */
