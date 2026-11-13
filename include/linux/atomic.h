#ifndef __TOOLS_LINUX_ATOMIC_H
#define __TOOLS_LINUX_ATOMIC_H

#include <linux/compiler.h>
#include <linux/kernel.h>

typedef struct {
	int		counter;
} atomic_t;

typedef struct {
	long		counter;
} atomic_long_t;

typedef struct {
	u64		counter;
} atomic64_t;

#define __ATOMIC_READ(p)		__atomic_load_n(p,	__ATOMIC_RELAXED)
#define __ATOMIC_SET(p, v)		__atomic_store_n(p, v,	__ATOMIC_RELAXED)
#define __ATOMIC_SET_RELEASE(p, v)	__atomic_store_n(p, v,	__ATOMIC_RELEASE)
#define __ATOMIC_ADD_RETURN(v, p)	__atomic_add_fetch(p, v, __ATOMIC_RELAXED)
#define __ATOMIC_ADD_RETURN_RELEASE(v, p)				\
					__atomic_add_fetch(p, v, __ATOMIC_RELEASE)
#define __ATOMIC_SUB_RETURN(v, p)	__atomic_sub_fetch(p, v, __ATOMIC_RELAXED)
#define __ATOMIC_FETCH_SUB_RELEASE(v, p)	__atomic_fetch_sub(p, v, __ATOMIC_RELEASE)
#define __ATOMIC_SUB_RETURN_RELEASE(v, p)				\
					__atomic_sub_fetch(p, v, __ATOMIC_RELEASE)
#define __ATOMIC_AND(v, p)		__atomic_and_fetch(&(p)->counter, v, __ATOMIC_RELAXED)
#define __ATOMIC_OR(v, p)		__atomic_or_fetch(&(p)->counter, v, __ATOMIC_RELAXED)

#define xchg(p, v)			__atomic_exchange_n(p, v, __ATOMIC_SEQ_CST)
#define xchg_acquire(p, v)		__atomic_exchange_n(p, v, __ATOMIC_ACQUIRE)

#define try_cmpxchg(p, old, new)				\
	__atomic_compare_exchange_n((p), old, new, false,	\
				    __ATOMIC_SEQ_CST,		\
				    __ATOMIC_SEQ_CST)

#define try_cmpxchg_acquire(p, old, new)			\
	__atomic_compare_exchange_n((p), old, new, false,	\
				    __ATOMIC_ACQUIRE,		\
				    __ATOMIC_RELAXED)

#define cmpxchg(p, old, new)					\
({								\
	typeof(*(p)) __old = (old);				\
								\
	__atomic_compare_exchange_n((p), &__old, new, false,	\
				    __ATOMIC_SEQ_CST,		\
				    __ATOMIC_SEQ_CST);		\
	__old;							\
})

#define cmpxchg_acquire(p, old, new)				\
({								\
	typeof(*(p)) __old = (old);				\
								\
	__atomic_compare_exchange_n((p), &__old, new, false,	\
				    __ATOMIC_ACQUIRE,		\
				    __ATOMIC_ACQUIRE);		\
	__old;							\
})

#define cmpxchg_release(p, old, new)				\
({								\
	typeof(*(p)) __old = (old);				\
								\
	__atomic_compare_exchange_n((p), &__old, new, false,	\
				    __ATOMIC_RELEASE,		\
				    __ATOMIC_RELAXED);		\
	__old;							\
})

#define smp_mb__before_atomic()	__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define smp_mb__after_atomic()	__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define smp_wmb()		__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define smp_rmb()		__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define smp_mb()		__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define smp_read_barrier_depends()
#define smp_acquire__after_ctrl_dep()	__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define smp_store_mb(var, value)  do { WRITE_ONCE(var, value); smp_mb(); } while (0)

#define smp_load_acquire(p)						\
({									\
	typeof(*p) ___p1 = READ_ONCE(*p);				\
	smp_mb();							\
	___p1;								\
})

#define smp_store_release(p, v)						\
do {									\
	smp_mb();							\
	WRITE_ONCE(*p, v);						\
} while (0)

/* atomic interface: */

#ifndef __ATOMIC_ADD
#define __ATOMIC_ADD(i, v) __ATOMIC_ADD_RETURN(i, v)
#endif

#ifndef __ATOMIC_ADD_RETURN_RELEASE
#define __ATOMIC_ADD_RETURN_RELEASE(i, v)				\
	({ smp_mb__before_atomic(); __ATOMIC_ADD_RETURN(i, v); })
#endif

#ifndef __ATOMIC_SUB_RETURN_RELEASE
#define __ATOMIC_SUB_RETURN_RELEASE(i, v)				\
	({ smp_mb__before_atomic(); __ATOMIC_SUB_RETURN(i, v); })
#endif

#ifndef __ATOMIC_FETCH_SUB_RELEASE
#define __ATOMIC_FETCH_SUB_RELEASE(i, v)				\
	({ smp_mb__before_atomic(); __ATOMIC_SUB_RETURN(i, v) + (i); })
#endif

#ifndef __ATOMIC_SUB
#define __ATOMIC_SUB(i, v) __ATOMIC_SUB_RETURN(i, v)
#endif

#ifndef __ATOMIC_INC_RETURN
#define __ATOMIC_INC_RETURN(v) __ATOMIC_ADD_RETURN(1, v)
#endif

#ifndef __ATOMIC_DEC_RETURN
#define __ATOMIC_DEC_RETURN(v) __ATOMIC_SUB_RETURN(1, v)
#endif

#ifndef __ATOMIC_INC
#define __ATOMIC_INC(v) __ATOMIC_ADD(1, v)
#endif

#ifndef __ATOMIC_DEC
#define __ATOMIC_DEC(v) __ATOMIC_SUB(1, v)
#endif

#define DEF_ATOMIC_OPS(a_type, i_type)					\
static inline i_type a_type##_read(const a_type##_t *v)			\
{									\
	return __ATOMIC_READ(&v->counter);				\
}									\
									\
static inline i_type a_type##_read_acquire(const a_type##_t *v)		\
{									\
	i_type ret = __ATOMIC_READ(&v->counter);			\
	smp_mb__after_atomic();						\
	return ret;							\
}									\
									\
static inline void a_type##_set(a_type##_t *v, i_type i)		\
{									\
	__ATOMIC_SET(&v->counter, i);					\
}									\
									\
static inline void a_type##_set_release(a_type##_t *v, i_type i)	\
{									\
	__ATOMIC_SET_RELEASE(&v->counter, i);				\
}									\
									\
static inline i_type a_type##_add_return(i_type i, a_type##_t *v)	\
{									\
	return __ATOMIC_ADD_RETURN(i, &v->counter);			\
}									\
									\
static inline i_type a_type##_add_return_release(i_type i, a_type##_t *v)\
{									\
	return __ATOMIC_ADD_RETURN_RELEASE(i, &v->counter);		\
}									\
									\
static inline i_type a_type##_sub_return_release(i_type i, a_type##_t *v)\
{									\
	return __ATOMIC_SUB_RETURN_RELEASE(i, &v->counter);		\
}									\
									\
static inline i_type a_type##_fetch_sub_release(i_type i, a_type##_t *v)\
{									\
	return __ATOMIC_FETCH_SUB_RELEASE(i, &v->counter);		\
}									\
									\
static inline i_type a_type##_sub_return(i_type i, a_type##_t *v)	\
{									\
	return __ATOMIC_SUB_RETURN(i, &v->counter);			\
}									\
									\
static inline void a_type##_add(i_type i, a_type##_t *v)		\
{									\
	__ATOMIC_ADD(i, &v->counter);					\
}									\
									\
static inline void a_type##_sub(i_type i, a_type##_t *v)		\
{									\
	__ATOMIC_SUB(i, &v->counter);					\
}									\
									\
static inline i_type a_type##_inc_return(a_type##_t *v)			\
{									\
	return __ATOMIC_INC_RETURN(&v->counter);			\
}									\
									\
static inline i_type a_type##_dec_return(a_type##_t *v)			\
{									\
	return __ATOMIC_DEC_RETURN(&v->counter);			\
}									\
									\
static inline i_type a_type##_dec_return_release(a_type##_t *v)		\
{									\
	return __ATOMIC_SUB_RETURN_RELEASE(1, &v->counter);		\
}									\
									\
static inline void a_type##_inc(a_type##_t *v)				\
{									\
	__ATOMIC_INC(&v->counter);					\
}									\
									\
static inline void a_type##_dec(a_type##_t *v)				\
{									\
	__ATOMIC_DEC(&v->counter);					\
}									\
									\
static inline bool a_type##_add_negative(i_type i, a_type##_t *v)	\
{									\
	return __ATOMIC_ADD_RETURN(i, &v->counter) < 0;			\
}									\
									\
static inline bool a_type##_sub_and_test(i_type i, a_type##_t *v)	\
{									\
	return __ATOMIC_SUB_RETURN(i, &v->counter) == 0;		\
}									\
									\
static inline bool a_type##_inc_and_test(a_type##_t *v)			\
{									\
	return __ATOMIC_INC_RETURN(&v->counter) == 0;			\
}									\
									\
static inline bool a_type##_dec_and_test(a_type##_t *v)			\
{									\
	return __ATOMIC_DEC_RETURN(&v->counter) == 0;			\
}									\
									\
static inline i_type a_type##_add_unless(a_type##_t *v, i_type a, i_type u)\
{									\
	i_type old, c = __ATOMIC_READ(&v->counter);			\
	while (c != u && (old = cmpxchg(&v->counter, c, c + a)) != c)	\
		c = old;						\
	return c;							\
}									\
									\
static inline bool a_type##_inc_not_zero(a_type##_t *v)			\
{									\
	return a_type##_add_unless(v, 1, 0);				\
}									\
									\
static inline void a_type##_and(i_type a, a_type##_t *v)		\
{									\
	__ATOMIC_AND(a, v);						\
}									\
									\
static inline void a_type##_or(i_type a, a_type##_t *v)			\
{									\
	__ATOMIC_OR(a, v);						\
}									\
									\
static inline i_type a_type##_xchg(a_type##_t *v, i_type i)		\
{									\
	return xchg(&v->counter, i);					\
}									\
									\
static inline i_type a_type##_cmpxchg(a_type##_t *v, i_type old, i_type new)\
{									\
	return cmpxchg(&v->counter, old, new);				\
}									\
									\
static inline bool a_type##_try_cmpxchg(a_type##_t *v, i_type *old, i_type new)\
{									\
	return try_cmpxchg(&v->counter, old, new);			\
}									\
									\
static inline i_type a_type##_cmpxchg_acquire(a_type##_t *v, i_type old, i_type new)\
{									\
	return cmpxchg_acquire(&v->counter, old, new);			\
}									\
									\
static inline bool a_type##_try_cmpxchg_acquire(a_type##_t *v, i_type *old, i_type new)\
{									\
	i_type prev = *old;						\
	*old = cmpxchg_acquire(&v->counter, *old, new);			\
	return prev == *old;						\
}									\
									\
static inline bool a_type##_try_cmpxchg_release(a_type##_t *v, i_type *old, i_type new)\
{									\
	i_type prev = *old;						\
	*old = cmpxchg_release(&v->counter, *old, new);			\
	return prev == *old;						\
}

DEF_ATOMIC_OPS(atomic,		int)
DEF_ATOMIC_OPS(atomic_long,	long)

DEF_ATOMIC_OPS(atomic64,	s64)

#endif /* __TOOLS_LINUX_ATOMIC_H */
