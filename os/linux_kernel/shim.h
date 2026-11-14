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

#endif /* _BCACHEFS_OS_LINUX_KERNEL_SHIM_H */
