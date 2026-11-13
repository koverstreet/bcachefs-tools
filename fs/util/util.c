// SPDX-License-Identifier: GPL-2.0
/*
 * random utility code, for bcache but in theory not specific to bcache
 *
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/debugfs.h>
#include <linux/kthread.h>
#include <linux/percpu.h>
#include <linux/preempt.h>
#include <linux/rcupdate.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/sched/debug.h>
#include <linux/sched/signal.h>
#include <linux/static_key.h>
#include <linux/version.h>

#include "util.h"

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

/*
 * static_branch_unlikely(), for Rust where it has no asm of its own
 * (util/jump_label.rs): out of line, @key isn't a constant the asm can name -
 * the key's count, as static_branch_unlikely() reads it without
 * CONFIG_JUMP_LABEL.
 */
bool bch2_static_branch_unlikely(struct static_key_false *key)
{
	return static_key_enabled(&key->key);
}

/* ...per_cpu_ptr(), and the possible CPUs: the one after @cpu, -1 at the end */
void *bch2_per_cpu_ptr(void *pcp, unsigned cpu)
{
	return per_cpu_ptr((void __percpu *) (unsigned long) pcp, cpu);
}

int bch2_cpu_possible_next(int cpu)
{
#ifdef __KERNEL__
	cpu = cpumask_next(cpu, cpu_possible_mask);
	return cpu < nr_cpu_ids ? cpu : -1;
#else
	return cpu + 1 < bch_percpu_nr_cpus ? cpu + 1 : -1;
#endif
}

/* ...this CPU's: its copy, preemption and migration, and its id */
void *bch2_this_cpu_ptr(void *pcp)
{
	return this_cpu_ptr((void __percpu *) (unsigned long) pcp);
}

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
#if defined(__KERNEL__) && !defined(CONFIG_SCHED_ALT)
	WRITE_ONCE(current->wake_cpu, cpu);
#endif
}

/*
 * For Rust: the kernel's Rust allocator API has no __GFP_NOFAIL. Out of line,
 * as locking/locking.c's shims are, for the same reason.
 */
void *rust_kzalloc_nofail(size_t size)
{
	return kzalloc(size, GFP_KERNEL|__GFP_NOFAIL);
}

void *rust_kvzalloc_nofail(size_t size)
{
	return kvzalloc(size, GFP_KERNEL|__GFP_NOFAIL);
}

void rust_kfree(const void *p)
{
	kfree(p);
}

/* For util/rcu.rs: kvfree_rcu() is a macro, of a struct's rcu_head field */
void rust_kvfree(void *p)
{
	kvfree(p);
}

/*
 * kvfree_rcu(p, field), for a head the macro can't name: anywhere in @p. As a
 * void *, as the macro casts it: newer kernels take a struct kvfree_rcu_head,
 * no bigger than an rcu_head.
 */
void rust_kvfree_rcu(void *p, struct rcu_head *head)
{
#ifdef __KERNEL__
	kvfree_call_rcu((void *) head, p);
#else
	kvfree_rcu(p, head);
#endif
}

/* For util/bio/pages.rs: __get_free_pages() is a macro */
void *rust_bio_pages_alloc(size_t size, gfp_t gfp)
{
#ifdef __KERNEL__
	/* page aligned: we don't know the device's dma alignment */
	return (void *) __get_free_pages(gfp, get_order(size));
#else
	return kmalloc(size, gfp);
#endif
}

void rust_bio_pages_free(void *p, size_t size)
{
	free_pages((unsigned long) p, get_order(size));
}

void rust_kfree_rcu_mightsleep(void *p)
{
	kfree_rcu_mightsleep(p);
}

/*
 * For Rust, what it can't call itself: macros, and in places which of a
 * macro or a function differs between the kernel and userspace.
 */
void bch2_cond_resched(void)
{
	cond_resched();
}

bool bch2_fatal_signal_pending(void)
{
	return fatal_signal_pending(current);
}

#ifdef __KERNEL__
unsigned bch2_num_online_cpus(void)
{
	return num_online_cpus();
}
#endif

/* bdev_rot() is util.h's, before 7.0 and in userspace */
bool bch2_bdev_rot(struct block_device *bdev)
{
	return bdev_rot(bdev);
}

#ifdef __KERNEL__
/* For os/linux_kernel/workqueue.rs: alloc_workqueue() is a macro, its name a format */
struct workqueue_struct *bch2_alloc_workqueue(const char *name, unsigned flags, int max_active)
{
	return alloc_workqueue("%s", flags, max_active, name);
}

#endif

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
 * For util/sleep.rs: current, set_current_state() and
 * signal_pending_state() are macros, or inlines over current.
 */
struct task_struct *bch2_current(void)
{
	return current;
}

void bch2_set_current_state(unsigned int state)
{
	set_current_state(state);
}

void bch2_set_current_state_running(void)
{
	__set_current_state(TASK_RUNNING);
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

#ifdef __KERNEL__
/* For os/linux_kernel/workqueue.rs: set_worker_desc() takes a format */
void bch2_set_worker_desc(const char *desc)
{
	set_worker_desc("%s", desc);
}
#endif

/* capable() is a function in the kernel, a macro (true) in userspace's blkdev.h */
bool bch2_capable(int cap)
{
	return capable(cap);
}

/* For util/debugfs.rs: debugfs_create_file() is, in the kernel, a _Generic() over its fops' type */
struct dentry *bch2_debugfs_create_file(const char *name, umode_t mode, struct dentry *parent,
					void *data, const struct file_operations *fops)
{
	return debugfs_create_file(name, mode, parent, data, fops);
}

#ifdef __KERNEL__
#include <linux/cpumask.h>

/* nr_cpu_ids is a variable - or a macro, without SMP */
unsigned bch2_nr_cpu_ids(void)
{
	return nr_cpu_ids;
}
#endif

/*
 * For Rust's ratelimited log macros, which keep a ratelimit_state per call
 * site as bch2_ratelimit() does, but have to initialize it at runtime:
 */
void bch2_ratelimit_state_init(struct ratelimit_state *rs)
{
	ratelimit_default_init(rs);
}

/*
 * For Rust's warn_on!() in the kernel build: the kernel's own warn_on!() pulls
 * the arch's WARN asm from a file generated in the kernel's build tree
 * (rust/kernel/generated_arch_warn_asm.rs), which distro headers packages
 * don't ship, so a DKMS build against them fails:
 */
void bch2_rust_warn(const char *file, unsigned line)
{
	WARN(1, "WARNING at %s:%u\n", file, line);
}
