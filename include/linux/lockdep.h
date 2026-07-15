#ifndef __TOOLS_LINUX_LOCKDEP_H
#define __TOOLS_LINUX_LOCKDEP_H

struct lock_class_key {};
struct task_struct;

# define lock_acquire(l, s, t, r, c, n, i)	do { } while (0)
# define lock_acquire_exclusive(...)		do { } while (0)
# define lockdep_set_notrack_class(...)		do { } while (0)
# define lock_release(l, i)			do { } while (0)
# define lock_set_class(l, n, k, s, i)		do { } while (0)
# define lock_set_subclass(l, s, i)		do { } while (0)
# define lockdep_set_current_reclaim_state(g)	do { } while (0)
# define lockdep_clear_current_reclaim_state()	do { } while (0)
# define lockdep_trace_alloc(g)			do { } while (0)
# define lockdep_info()				do { } while (0)
# define lockdep_init_map(lock, name, key, sub) \
		do { (void)(name); (void)(key); } while (0)
# define lockdep_set_class(lock, key)		do { (void)(key); } while (0)
# define lockdep_set_class_and_name(lock, key, name) \
		do { (void)(key); (void)(name); } while (0)
#define lockdep_set_class_and_subclass(lock, key, sub) \
		do { (void)(key); } while (0)
#define lockdep_set_subclass(lock, sub)		do { } while (0)

#define lockdep_set_novalidate_class(lock) do { } while (0)

#define lockdep_assert_held(l)			do { (void)(l); } while (0)
#define lockdep_assert_held_once(l)		do { (void)(l); } while (0)

#define lock_acquire_shared(l, s, t, n, i)

#define lockdep_acquire_shared(lock)

#define lock_contended(lockdep_map, ip) do {} while (0)
#define lock_acquired(lockdep_map, ip) do {} while (0)

#define lock_set_cmp_fn(lock, ...)	do { } while (0)

static inline void debug_show_all_locks(void)
{
}

static inline void debug_show_held_locks(struct task_struct *task)
{
}

static inline void
debug_check_no_locks_freed(const void *from, unsigned long len)
{
}

static inline void
debug_check_no_locks_held(void)
{
}

static inline int lock_class_is_held(struct lock_class_key *k)
{
	return 0;
}

#endif /* __TOOLS_LINUX_LOCKDEP_H */

