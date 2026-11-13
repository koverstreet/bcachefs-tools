/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_SHIM_H
#define _BCACHEFS_UTIL_SHIM_H

#include <linux/gfp.h>
#include <linux/srcu.h>
#include <linux/types.h>
#include <linux/workqueue.h>

struct block_device;
struct dentry;
struct file_operations;
struct ratelimit_state;
struct static_key_false;
struct task_struct;

/*
 * util.c's: the kernel's macros and inlines, and what differs between the
 * kernel and userspace, for Rust
 */

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

bool bch2_static_branch_unlikely(struct static_key_false *key);

void *mempool_kvmalloc(gfp_t gfp_mask, void *pool_data);
void mempool_kvfree(void *element, void *pool_data);
void *rust_kzalloc_nofail(size_t size);
void *rust_kvzalloc_nofail(size_t size);
void rust_kfree(const void *p);
void rust_kvfree(void *p);
void rust_kvfree_rcu(void *p, struct rcu_head *head);
void *rust_bio_pages_alloc(size_t size, gfp_t gfp);
void rust_bio_pages_free(void *p, size_t size);
void rust_kfree_rcu_mightsleep(void *p);

void bch2_cond_resched(void);
bool bch2_fatal_signal_pending(void);
unsigned bch2_num_online_cpus(void);
bool bch2_bdev_rot(struct block_device *bdev);
void bch2_init_delayed_work(struct delayed_work *dwork, work_func_t func);
struct workqueue_struct *bch2_alloc_workqueue(const char *name, unsigned flags, int max_active);
struct task_struct *bch2_kthread_create(int (*fn)(void *), void *data, const char *name);
bool bch2_kthread_should_stop(void);
struct task_struct *bch2_current(void);
void bch2_set_current_state(unsigned int state);
void bch2_set_current_state_running(void);
bool bch2_signal_pending_state(unsigned int state);
bool bch2_need_resched(void);
#ifdef CONFIG_SMP
bool bch2_owner_on_cpu(struct task_struct *owner);
bool bch2_rt_or_dl_task_current(void);
#endif
void bch2_init_work(struct work_struct *work, work_func_t func);
void bch2_set_worker_desc(const char *desc);
bool bch2_capable(int cap);
struct dentry *bch2_debugfs_create_file(const char *name, umode_t mode, struct dentry *parent,
					void *data, const struct file_operations *fops);
#ifdef __KERNEL__
unsigned bch2_nr_cpu_ids(void);
#endif

void bch2_ratelimit_state_init(struct ratelimit_state *rs);
void bch2_rust_warn(const char *file, unsigned line);

/* rcu.c's: init_srcu_struct() is a macro under lockdep */
int rust_init_srcu_struct(struct srcu_struct *ssp, const char *name, void *key);

#ifndef __KERNEL__
/* the userspace shim's: linux/stacktrace.c's, and linux/percpu.c's */
void bch2_symbolize(char *out, size_t size, unsigned long addr);
void __bch2_alloc_percpu_init(void *pcv, void (*init)(void *, void *, unsigned), void *ctx);
#endif

#endif /* _BCACHEFS_UTIL_SHIM_H */
