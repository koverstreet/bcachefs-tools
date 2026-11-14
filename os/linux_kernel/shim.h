/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_OS_LINUX_KERNEL_SHIM_H
#define _BCACHEFS_OS_LINUX_KERNEL_SHIM_H

/* shim.c's: the kernel's macros and inlines, for the kernel port's Rust */

#include <linux/blkdev.h>
#include <linux/debugfs.h>
#include <linux/gfp.h>
#include <linux/local_lock.h>
#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>
#include <linux/rwsem.h>
#include <linux/spinlock.h>
#include <linux/srcu.h>
#include <linux/workqueue.h>

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

int rust_percpu_init_rwsem(struct percpu_rw_semaphore *sem, const char *name, void *key);
void rust_percpu_free_rwsem(struct percpu_rw_semaphore *sem);
void rust_percpu_down_read(struct percpu_rw_semaphore *sem);
void rust_percpu_up_read(struct percpu_rw_semaphore *sem);
void rust_percpu_down_write(struct percpu_rw_semaphore *sem);
void rust_percpu_up_write(struct percpu_rw_semaphore *sem);

void rust_trace_contention_begin(const void *lock, unsigned int flags);
void rust_trace_contention_end(const void *lock, int ret);

void rust_local_lock_init(local_lock_t *lock);
unsigned long rust_local_lock_irqsave(local_lock_t *pcp);
void rust_local_unlock_irqrestore(local_lock_t *pcp, unsigned long flags);
unsigned long rust_local_irq_save(void);
void rust_local_irq_restore(unsigned long flags);

void bch2_this_cpu_add_u32(u32 *pcp, u32 val);
void bch2_this_cpu_add_u64(u64 *pcp, u64 val);
u32 bch2_this_cpu_cmpxchg_u32(u32 *pcp, u32 old, u32 new);
u64 bch2_this_cpu_cmpxchg_u64(u64 *pcp, u64 old, u64 new);
void *bch2_this_cpu_ptr(void *pcp);
void *bch2_per_cpu_ptr(void *pcp, unsigned cpu);
int bch2_cpu_possible_next(int cpu);
void bch2_preempt_disable(void);
void bch2_preempt_enable(void);
void bch2_migrate_disable(void);
void bch2_migrate_enable(void);
unsigned bch2_raw_smp_processor_id(void);
int bch2_task_pid(struct task_struct *task);
void bch2_set_current_wake_cpu(int cpu);
unsigned bch2_num_online_cpus(void);
bool bch2_fatal_signal_pending(void);
bool bch2_bdev_rot(struct block_device *bdev);
struct workqueue_struct *bch2_alloc_workqueue(const char *name, unsigned flags, int max_active);
void bch2_set_worker_desc(const char *desc);
struct task_struct *bch2_kthread_create(int (*fn)(void *), void *data, const char *name);
bool bch2_kthread_should_stop(void);
struct task_struct *bch2_current(void);
bool bch2_signal_pending_state(unsigned int state);
void bch2_set_current_state(unsigned int state);
void bch2_set_current_state_running(void);

int rust_init_srcu_struct(struct srcu_struct *ssp, const char *name, void *key);

void *rust_kzalloc_nofail(size_t size);
void *rust_kvzalloc_nofail(size_t size);
void rust_kfree(const void *p);
void rust_kvfree(void *p);
void rust_kvfree_rcu(void *p, struct rcu_head *head);
void rust_kfree_rcu_mightsleep(void *p);
void *rust_bio_pages_alloc(size_t size, gfp_t gfp);
void rust_bio_pages_free(void *p, size_t size);
bool bch2_need_resched(void);
#ifdef CONFIG_SMP
bool bch2_owner_on_cpu(struct task_struct *owner);
bool bch2_rt_or_dl_task_current(void);
#endif
struct dentry *bch2_debugfs_create_file(const char *name, umode_t mode, struct dentry *parent,
					void *data, const struct file_operations *fops);
unsigned bch2_nr_cpu_ids(void);

void bch2_rust_warn(const char *file, unsigned line);
void bch2_cond_resched(void);
bool bch2_capable(int cap);

struct static_key_false;
bool bch2_static_branch_unlikely(struct static_key_false *key);

#endif /* _BCACHEFS_OS_LINUX_KERNEL_SHIM_H */
