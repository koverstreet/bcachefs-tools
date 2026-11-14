/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOCKING_SHIM_H
#define _BCACHEFS_LOCKING_SHIM_H

/* locking.c's: the C locks' macros and inlines, for Rust (locking/raw.rs) */

#include <linux/local_lock.h>
#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>

void rust_mutex_init(struct mutex *lock, const char *name, void *key);

void rust_trace_contention_begin(const void *lock, unsigned int flags);
void rust_trace_contention_end(const void *lock, int ret);

void rust_local_lock_init(local_lock_t *lock);
unsigned long rust_local_lock_irqsave(local_lock_t *pcp);
void rust_local_unlock_irqrestore(local_lock_t *pcp, unsigned long flags);
unsigned long rust_local_irq_save(void);
void rust_local_irq_restore(unsigned long flags);

int rust_percpu_init_rwsem(struct percpu_rw_semaphore *sem, const char *name, void *key);
void rust_percpu_free_rwsem(struct percpu_rw_semaphore *sem);
void rust_percpu_down_read(struct percpu_rw_semaphore *sem);
void rust_percpu_up_read(struct percpu_rw_semaphore *sem);
void rust_percpu_down_write(struct percpu_rw_semaphore *sem);
void rust_percpu_up_write(struct percpu_rw_semaphore *sem);

#endif /* _BCACHEFS_LOCKING_SHIM_H */
