/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_OS_LINUX_KERNEL_SHIM_H
#define _BCACHEFS_OS_LINUX_KERNEL_SHIM_H

/* shim.c's: the kernel's macros and inlines, for the kernel port's Rust */

#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>
#include <linux/rwsem.h>
#include <linux/spinlock.h>

struct lockdep_map;
void rust_mutex_assert_held(struct mutex *lock);
void rust_spin_lock_assert_held(spinlock_t *lock);
void rust_raw_spin_lock_assert_held(raw_spinlock_t *lock);
void rust_rwsem_assert_held(struct rw_semaphore *sem);
void rust_rwsem_assert_held_write(struct rw_semaphore *sem);
void rust_percpu_rwsem_assert_held(struct percpu_rw_semaphore *sem);
void rust_percpu_rwsem_assert_held_write(struct percpu_rw_semaphore *sem);
void rust_lockdep_set_notrack_class(struct lockdep_map *map);

#ifdef CONFIG_DEBUG_LOCK_ALLOC
void rust_lockdep_init_map(void *map, const char *name, void *key);
void rust_lock_acquire(void *map, int trylock, int read, unsigned long ip);
void rust_lock_release(void *map, unsigned long ip);
void rust_lock_contended(void *map, unsigned long ip);
void rust_lock_acquired(void *map, unsigned long ip);
void rust_debug_check_no_locks_freed(const void *p, unsigned long len);
#endif

void rust_spin_lock_init(spinlock_t *lock, const char *name, void *key);
unsigned long rust_spin_lock_irqsave(spinlock_t *lock);
unsigned long rust_spin_lock_irqsave_nested(spinlock_t *lock, unsigned subclass);
void rust_spin_unlock_irqrestore(spinlock_t *lock, unsigned long flags);

void rust_raw_spin_lock_init(raw_spinlock_t *lock, const char *name, void *key);
void rust_raw_spin_lock(raw_spinlock_t *lock);
void rust_raw_spin_unlock(raw_spinlock_t *lock);

void rust_init_rwsem(struct rw_semaphore *sem, const char *name, void *key);

void rust_mutex_init(struct mutex *lock, const char *name, void *key);

#endif /* _BCACHEFS_OS_LINUX_KERNEL_SHIM_H */
