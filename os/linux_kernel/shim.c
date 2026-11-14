// SPDX-License-Identifier: GPL-2.0

/*
 * The C that the kernel port's Rust calls: what C has as a macro - lockdep's
 * classes and asserts, spin_lock_irqsave() - or as a static inline.
 *
 * A lock initialized from Rust has a lockdep class per call site, as C's
 * init macros give theirs: Rust passes its own, @key.
 */

#include <linux/blkdev.h>
#include <linux/jump_label.h>
#include <linux/mutex.h>
#include <linux/percpu.h>
#include <linux/percpu-rwsem.h>
#include <linux/preempt.h>
#include <linux/rwsem.h>
#include <linux/sched/signal.h>
#include <linux/spinlock.h>
#include <linux/srcu.h>
#include <linux/version.h>
#include <linux/workqueue.h>

#include "shim.h"

/* os/linux_kernel/lockdep.rs: the C locks' asserts */

void rust_mutex_assert_held(struct mutex *lock)
{
	lockdep_assert_held(lock);
}

void rust_spin_lock_assert_held(spinlock_t *lock)
{
	lockdep_assert_held(lock);
}

void rust_raw_spin_lock_assert_held(raw_spinlock_t *lock)
{
	lockdep_assert_held(lock);
}

void rust_rwsem_assert_held(struct rw_semaphore *sem)
{
	lockdep_assert_held(sem);
}

void rust_rwsem_assert_held_write(struct rw_semaphore *sem)
{
	lockdep_assert_held_write(sem);
}

void rust_percpu_rwsem_assert_held(struct percpu_rw_semaphore *sem)
{
	percpu_rwsem_assert_held(sem);
}

void rust_percpu_rwsem_assert_held_write(struct percpu_rw_semaphore *sem)
{
	lockdep_assert_held_write(sem);
}

/* a lock whose state is Rust's - six - by its map: the macro wants the lock */
void rust_lockdep_set_notrack_class(struct lockdep_map *map)
{
	lockdep_set_notrack_class((struct { struct lockdep_map dep_map; } *) map);
}

/* os/linux_kernel/lockdep.rs: lockdep's hooks, for a lock whose state is ours */

#ifdef CONFIG_DEBUG_LOCK_ALLOC
void rust_lockdep_init_map(void *map, const char *name, void *key)
{
	lockdep_init_map(map, name, key, 0);
}

void rust_lock_acquire(void *map, int trylock, int read, unsigned long ip)
{
	lock_acquire(map, 0, trylock, read, 1, NULL, ip);
}

void rust_lock_release(void *map, unsigned long ip)
{
	lock_release(map, ip);
}

void rust_lock_contended(void *map, unsigned long ip)
{
	lock_contended(map, ip);
}

void rust_lock_acquired(void *map, unsigned long ip)
{
	lock_acquired(map, ip);
}

void rust_debug_check_no_locks_freed(const void *p, unsigned long len)
{
	debug_check_no_locks_freed(p, len);
}
#endif

/* os/linux_kernel/spinlock.rs: spinlock_t, as the kernel's Rust helper initializes it */

void rust_spin_lock_init(spinlock_t *lock, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_SPINLOCK
# ifdef CONFIG_PREEMPT_RT
	__spin_lock_init(lock, name, key, false);
# else
	__raw_spin_lock_init(spinlock_check(lock), name, key, LD_WAIT_CONFIG);
# endif
#else
	spin_lock_init(lock);
#endif
}

unsigned long rust_spin_lock_irqsave(spinlock_t *lock)
{
	unsigned long flags;

	spin_lock_irqsave(lock, flags);
	return flags;
}

unsigned long rust_spin_lock_irqsave_nested(spinlock_t *lock, unsigned subclass)
{
	unsigned long flags;

	spin_lock_irqsave_nested(lock, flags, subclass);
	return flags;
}

void rust_spin_unlock_irqrestore(spinlock_t *lock, unsigned long flags)
{
	spin_unlock_irqrestore(lock, flags);
}

/* os/linux_kernel/spinlock.rs: raw_spinlock_t */

void rust_raw_spin_lock_init(raw_spinlock_t *lock, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_SPINLOCK
	__raw_spin_lock_init(lock, name, key, LD_WAIT_SPIN);
#else
	raw_spin_lock_init(lock);
#endif
}

void rust_raw_spin_lock(raw_spinlock_t *lock)
{
	raw_spin_lock(lock);
}

void rust_raw_spin_unlock(raw_spinlock_t *lock)
{
	raw_spin_unlock(lock);
}

/* os/linux_kernel/rwsem.rs: rw_semaphore */

void rust_init_rwsem(struct rw_semaphore *sem, const char *name, void *key)
{
	__init_rwsem(sem, name, key);
}

/* os/linux_kernel/mutex.rs: struct mutex */

void rust_mutex_init(struct mutex *lock, const char *name, void *key)
{
	__mutex_init(lock, name, key);
}

/*
 * 7.1 introduced bdev_rot() and removed bdev_nonrot(). Shim bdev_rot for
 * older kernels so call sites can always use bdev_rot.
 */
#if LINUX_VERSION_CODE < KERNEL_VERSION(7,0,0)
static inline bool bdev_rot(struct block_device *bdev)
{
	return !bdev_nonrot(bdev);
}
#endif

/*
 * this_cpu_add() and _cmpxchg(), for Rust where it has no asm of its own
 * (util/percpu.rs). @pcp is a _t __percpu *: the prototype Rust generates
 * can't say so, and on x86 __percpu can be an address space of its own.
 */
#define BCH2_THIS_CPU_OPS(_t)							\
void bch2_this_cpu_add_##_t(_t *pcp, _t val)					\
{										\
	this_cpu_add(*(_t __percpu *) (unsigned long) pcp, val);		\
}										\
										\
_t bch2_this_cpu_cmpxchg_##_t(_t *pcp, _t old, _t new)				\
{										\
	return this_cpu_cmpxchg(*(_t __percpu *) (unsigned long) pcp, old, new);\
}

BCH2_THIS_CPU_OPS(u32)
BCH2_THIS_CPU_OPS(u64)
#undef BCH2_THIS_CPU_OPS

/* ...per_cpu_ptr(), and the possible CPUs: the one after @cpu, -1 at the end */
void *bch2_per_cpu_ptr(void *pcp, unsigned cpu)
{
	return per_cpu_ptr((void __percpu *) (unsigned long) pcp, cpu);
}

int bch2_cpu_possible_next(int cpu)
{
	cpu = cpumask_next(cpu, cpu_possible_mask);
	return cpu < nr_cpu_ids ? cpu : -1;
}

/* ...this CPU's copy */
void *bch2_this_cpu_ptr(void *pcp)
{
	return this_cpu_ptr((void __percpu *) (unsigned long) pcp);
}

/* ...this CPU's: preemption and migration, and its id */
void bch2_preempt_disable(void)
{
	preempt_disable();
}

void bch2_preempt_enable(void)
{
	preempt_enable();
}

unsigned bch2_raw_smp_processor_id(void)
{
	return raw_smp_processor_id();
}

void bch2_migrate_disable(void)
{
	migrate_disable();
}

void bch2_migrate_enable(void)
{
	migrate_enable();
}

/* ...and a task's: its pid, and the CPU it's to be woken on */
int bch2_task_pid(struct task_struct *task)
{
	return task->pid;
}

/*
 * Compiled out under CONFIG_SCHED_ALT (BMQ/PDS): those schedulers don't have
 * select_task_rq_fair() and drop task_struct.wake_cpu.
 */
void bch2_set_current_wake_cpu(int cpu)
{
#ifndef CONFIG_SCHED_ALT
	WRITE_ONCE(current->wake_cpu, cpu);
#endif
}

bool bch2_fatal_signal_pending(void)
{
	return fatal_signal_pending(current);
}

unsigned bch2_num_online_cpus(void)
{
	return num_online_cpus();
}

/* bdev_rot() is the shim's above, before 7.0 */
bool bch2_bdev_rot(struct block_device *bdev)
{
	return bdev_rot(bdev);
}

/* For os/linux_kernel/workqueue.rs: alloc_workqueue() is a macro, its name a format */
struct workqueue_struct *bch2_alloc_workqueue(const char *name, unsigned flags, int max_active)
{
	return alloc_workqueue("%s", flags, max_active, name);
}

#include <linux/kthread.h>

/* For os/linux_kernel/kthread.rs: kthread_create() is a macro */
struct task_struct *bch2_kthread_create(int (*fn)(void *), void *data, const char *name)
{
	return kthread_create(fn, data, "%s", name);
}

bool bch2_kthread_should_stop(void)
{
	return (current->flags & PF_KTHREAD) && kthread_should_stop();
}

/*
 * os/linux_kernel/sched.rs: current, signal_pending_state() and need_resched()
 * are macros, or inlines over current
 */
struct task_struct *bch2_current(void)
{
	return current;
}

bool bch2_signal_pending_state(unsigned int state)
{
	return signal_pending_state(state, current);
}

bool bch2_need_resched(void)
{
	return need_resched();
}

#ifdef CONFIG_SMP
#include <linux/sched/rt.h>

/*
 * For locking/six.rs's optimistic spin: inlines over current and its owner -
 * sched.h's, which codegen can't unblocklist: its structs don't bind
 */
bool bch2_owner_on_cpu(struct task_struct *owner)
{
	return owner_on_cpu(owner);
}

bool bch2_rt_or_dl_task_current(void)
{
	return rt_or_dl_task(current);
}
#endif

/* For os/linux_kernel/workqueue.rs: set_worker_desc() takes a format */
void bch2_set_worker_desc(const char *desc)
{
	set_worker_desc("%s", desc);
}

#include <linux/debugfs.h>

/* For os/linux_kernel/debugfs.rs: debugfs_create_file() is a _Generic() over its fops' type */
struct dentry *bch2_debugfs_create_file(const char *name, umode_t mode, struct dentry *parent,
					void *data, const struct file_operations *fops)
{
	return debugfs_create_file(name, mode, parent, data, fops);
}

#include <linux/cpumask.h>

/* nr_cpu_ids is a variable - or a macro, without SMP */
unsigned bch2_nr_cpu_ids(void)
{
	return nr_cpu_ids;
}

/*
 * For os/linux_kernel/printk.rs's warn_at(), warn_on!()'s: the kernel's own
 * warn_on!() pulls the arch's WARN asm from a file generated in the kernel's
 * build tree (rust/kernel/generated_arch_warn_asm.rs), which distro headers
 * packages don't ship, so a DKMS build against them fails:
 */
void bch2_rust_warn(const char *file, unsigned line)
{
	WARN(1, "WARNING at %s:%u\n", file, line);
}

/* os/linux_kernel/kernel.rs: cond_resched() is a macro, or an inline over current */
void bch2_cond_resched(void)
{
	cond_resched();
}

/* ...and capable(), a function: whether the current task has @cap */
bool bch2_capable(int cap)
{
	return capable(cap);
}

/*
 * static_branch_unlikely(), for Rust where it has no asm of its own
 * (jump_label.rs): out of line, @key isn't a constant the asm can name -
 * the key's count, as static_branch_unlikely() reads it without
 * CONFIG_JUMP_LABEL.
 */
bool bch2_static_branch_unlikely(struct static_key_false *key)
{
	return static_key_enabled(&key->key);
}

/* os/linux_kernel/sched.rs: set_current_state() is a macro */
void bch2_set_current_state(unsigned int state)
{
	set_current_state(state);
}

void bch2_set_current_state_running(void)
{
	__set_current_state(TASK_RUNNING);
}

/* os/linux_kernel/percpu_rwsem.rs: percpu_rw_semaphore */

int rust_percpu_init_rwsem(struct percpu_rw_semaphore *sem, const char *name, void *key)
{
	return __percpu_init_rwsem(sem, name, key);
}

void rust_percpu_free_rwsem(struct percpu_rw_semaphore *sem)
{
	percpu_free_rwsem(sem);
}

void rust_percpu_down_read(struct percpu_rw_semaphore *sem)
{
	percpu_down_read(sem);
}

void rust_percpu_up_read(struct percpu_rw_semaphore *sem)
{
	percpu_up_read(sem);
}

void rust_percpu_down_write(struct percpu_rw_semaphore *sem)
{
	percpu_down_write(sem);
}

void rust_percpu_up_write(struct percpu_rw_semaphore *sem)
{
	percpu_up_write(sem);
}

/* os/linux_kernel/rcu.rs: init_srcu_struct() is a macro under lockdep */
int rust_init_srcu_struct(struct srcu_struct *ssp, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_LOCK_ALLOC
	return __init_srcu_struct(ssp, name, key);
#else
	return init_srcu_struct(ssp);
#endif
}
