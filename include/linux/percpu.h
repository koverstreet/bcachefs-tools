#ifndef __TOOLS_LINUX_PERCPU_H
#define __TOOLS_LINUX_PERCPU_H

#include <stddef.h>
#include <stdint.h>
#include <linux/cpumask.h>
#include <linux/kernel.h>
#include <linux/types.h>

#define __percpu

void *__alloc_percpu_gfp(size_t size, size_t align, gfp_t gfp);
void *__alloc_percpu(size_t size, size_t align);
void  free_percpu(void *p);

#define alloc_percpu_gfp(type, gfp)					\
	(typeof(type) __percpu *)__alloc_percpu_gfp(sizeof(type),	\
						__alignof__(type), gfp)
#define alloc_percpu(type)						\
	(typeof(type) __percpu *)__alloc_percpu(sizeof(type),		\
						__alignof__(type))

#define __verify_pcpu_ptr(ptr)

/*
 * Static per-CPU variables: DEFINE_PER_CPU() places the variable in a custom
 * linker section "bch_percpu". Each thread gets a private chunk of size
 * (__stop_bch_percpu - __start_bch_percpu), pointed at by a TLS pointer; the
 * variable's address in the section is its offset within the chunk.
 *
 * Cross-thread access (per_cpu_ptr(p, cpu)) goes through a global registry
 * of chunk pointers. alloc_percpu()-allocated memory is a regular heap pointer
 * outside the section range, so the macros pass it through unchanged for now
 * — phase 2 will fold dynamic percpu into the same chunk model.
 */
extern char __start_bch_percpu[], __stop_bch_percpu[];

#define DEFINE_PER_CPU(type, name)					\
	__attribute__((section("bch_percpu"))) type name

#define DECLARE_PER_CPU(type, name)	extern type name

#define BCH_PERCPU_MAX_CPUS	256

/*
 * Per-thread chunk layout: [static_section][dynamic_arena].
 *
 * Static section is sized at link time (__stop_bch_percpu - __start_bch_percpu);
 * the dynamic arena is bch_percpu_dynamic_size bytes for alloc_percpu().
 *
 * Address space per chunk, not memory: chunks are NORESERVE anonymous
 * mappings, so pages materialize only as allocations touch them. Sized for the
 * largest consumer - gc's accounting table allocates a percpu counter set per
 * accounting key, and a large filesystem with many snapshots has hundreds of
 * thousands of those.
 *
 * Fixed at first thread init and never changed afterwards: chunk pointers are
 * read without bch_percpu_lock, so a chunk can't be moved or resized out from
 * under a reader. It is not a compile-time constant because a 32-bit process
 * can't afford the same reservation as a 64-bit one, and because nothing needs
 * it to be - see size_at_grain in percpu.c, which used to.
 */
extern size_t bch_percpu_dynamic_size;

extern __thread void *bch_percpu_my_chunk;
extern __thread int   bch_percpu_my_id;
extern void *bch_percpu_chunks[BCH_PERCPU_MAX_CPUS];
extern int   bch_percpu_nr_cpus;
extern size_t bch_percpu_static_size;

void bch_percpu_thread_init(void);
void bch_percpu_register(void (*init_one)(void *), void (*exit_one)(void *),
			 void *pcv);



/*
 * A percpu pointer is resolved relative to __start_bch_percpu:
 *   - static variables (DEFINE_PER_CPU) reside at &var in [__start_bch_percpu, __stop_bch_percpu)
 *   - dynamic allocations (alloc_percpu()) return (__start_bch_percpu + chunk_off)
 *
 * This allows branchless resolution for both static and dynamic percpu pointers.
 */
static inline void *__bch_percpu_resolve(void *p, void *chunk)
{
	BUG_ON(!chunk);
	return (char *)chunk + ((uintptr_t)p - (uintptr_t)__start_bch_percpu);
}

/*
 * Lazy-init: any thread that takes a percpu pointer gets a chunk on first
 * access. Avoids having to remember to call bch_percpu_thread_init() at
 * the entry point of every thread that might reach libbcachefs — Rust-
 * spawned threads (tiny_http workers, fuse workers, std::thread closures
 * we don't directly own) won't hit a NULL chunk and silently corrupt /
 * SIGSEGV. The cold path is a tail call into thread_init; the hot path
 * is the same single load + branch as before.
 */
#define this_cpu_ptr(ptr)						\
({									\
	if (unlikely(!bch_percpu_my_chunk))				\
		bch_percpu_thread_init();				\
	(typeof(ptr))__bch_percpu_resolve((void *)(ptr), bch_percpu_my_chunk); \
})

#define per_cpu_ptr(ptr, cpu)						\
	((typeof(ptr))__bch_percpu_resolve((void *)(ptr), bch_percpu_chunks[cpu]))

#define raw_cpu_ptr(ptr)	this_cpu_ptr(ptr)

#define __pcpu_size_call_return(stem, variable)				\
({									\
	typeof(variable) pscr_ret__;					\
	__verify_pcpu_ptr(&(variable));					\
	switch(sizeof(variable)) {					\
	case 1: pscr_ret__ = stem##1(variable); break;			\
	case 2: pscr_ret__ = stem##2(variable); break;			\
	case 4: pscr_ret__ = stem##4(variable); break;			\
	case 8: pscr_ret__ = stem##8(variable); break;			\
	default:							\
		__bad_size_call_parameter(); break;			\
	}								\
	pscr_ret__;							\
})

#define __pcpu_size_call_return2(stem, variable, ...)			\
({									\
	typeof(variable) pscr2_ret__;					\
	__verify_pcpu_ptr(&(variable));					\
	switch(sizeof(variable)) {					\
	case 1: pscr2_ret__ = stem##1(variable, __VA_ARGS__); break;	\
	case 2: pscr2_ret__ = stem##2(variable, __VA_ARGS__); break;	\
	case 4: pscr2_ret__ = stem##4(variable, __VA_ARGS__); break;	\
	case 8: pscr2_ret__ = stem##8(variable, __VA_ARGS__); break;	\
	default:							\
		__bad_size_call_parameter(); break;			\
	}								\
	pscr2_ret__;							\
})

/*
 * Special handling for cmpxchg_double.  cmpxchg_double is passed two
 * percpu variables.  The first has to be aligned to a double word
 * boundary and the second has to follow directly thereafter.
 * We enforce this on all architectures even if they don't support
 * a double cmpxchg instruction, since it's a cheap requirement, and it
 * avoids breaking the requirement for architectures with the instruction.
 */
#define __pcpu_double_call_return_bool(stem, pcp1, pcp2, ...)		\
({									\
	bool pdcrb_ret__;						\
	__verify_pcpu_ptr(&(pcp1));					\
	BUILD_BUG_ON(sizeof(pcp1) != sizeof(pcp2));			\
	VM_BUG_ON((unsigned long)(&(pcp1)) % (2 * sizeof(pcp1)));	\
	VM_BUG_ON((unsigned long)(&(pcp2)) !=				\
		  (unsigned long)(&(pcp1)) + sizeof(pcp1));		\
	switch(sizeof(pcp1)) {						\
	case 1: pdcrb_ret__ = stem##1(pcp1, pcp2, __VA_ARGS__); break;	\
	case 2: pdcrb_ret__ = stem##2(pcp1, pcp2, __VA_ARGS__); break;	\
	case 4: pdcrb_ret__ = stem##4(pcp1, pcp2, __VA_ARGS__); break;	\
	case 8: pdcrb_ret__ = stem##8(pcp1, pcp2, __VA_ARGS__); break;	\
	default:							\
		__bad_size_call_parameter(); break;			\
	}								\
	pdcrb_ret__;							\
})

#define __pcpu_size_call(stem, variable, ...)				\
do {									\
	__verify_pcpu_ptr(&(variable));					\
	switch(sizeof(variable)) {					\
		case 1: stem##1(variable, __VA_ARGS__);break;		\
		case 2: stem##2(variable, __VA_ARGS__);break;		\
		case 4: stem##4(variable, __VA_ARGS__);break;		\
		case 8: stem##8(variable, __VA_ARGS__);break;		\
		default: 						\
			__bad_size_call_parameter();break;		\
	}								\
} while (0)

#define raw_cpu_read(pcp)		__pcpu_size_call_return(raw_cpu_read_, pcp)
#define raw_cpu_write(pcp, val)		__pcpu_size_call(raw_cpu_write_, pcp, val)
#define raw_cpu_add(pcp, val)		__pcpu_size_call(raw_cpu_add_, pcp, val)
#define raw_cpu_and(pcp, val)		__pcpu_size_call(raw_cpu_and_, pcp, val)
#define raw_cpu_or(pcp, val)		__pcpu_size_call(raw_cpu_or_, pcp, val)
#define raw_cpu_add_return(pcp, val)	__pcpu_size_call_return2(raw_cpu_add_return_, pcp, val)
#define raw_cpu_xchg(pcp, nval)		__pcpu_size_call_return2(raw_cpu_xchg_, pcp, nval)
#define raw_cpu_cmpxchg(pcp, oval, nval) \
	__pcpu_size_call_return2(raw_cpu_cmpxchg_, pcp, oval, nval)
#define raw_cpu_cmpxchg_double(pcp1, pcp2, oval1, oval2, nval1, nval2) \
	__pcpu_double_call_return_bool(raw_cpu_cmpxchg_double_, pcp1, pcp2, oval1, oval2, nval1, nval2)

#define raw_cpu_sub(pcp, val)		raw_cpu_add(pcp, -(val))
#define raw_cpu_inc(pcp)		raw_cpu_add(pcp, 1)
#define raw_cpu_dec(pcp)		raw_cpu_sub(pcp, 1)
#define raw_cpu_sub_return(pcp, val)	raw_cpu_add_return(pcp, -(typeof(pcp))(val))
#define raw_cpu_inc_return(pcp)		raw_cpu_add_return(pcp, 1)
#define raw_cpu_dec_return(pcp)		raw_cpu_add_return(pcp, -1)

#define __this_cpu_read(pcp)						\
({									\
	raw_cpu_read(pcp);						\
})

#define __this_cpu_write(pcp, val)					\
({									\
	raw_cpu_write(pcp, val);					\
})

#define __this_cpu_add(pcp, val)					\
({									\
	raw_cpu_add(pcp, val);						\
})

#define __this_cpu_and(pcp, val)					\
({									\
	raw_cpu_and(pcp, val);						\
})

#define __this_cpu_or(pcp, val)						\
({									\
	raw_cpu_or(pcp, val);						\
})

#define __this_cpu_add_return(pcp, val)					\
({									\
	raw_cpu_add_return(pcp, val);					\
})

#define __this_cpu_xchg(pcp, nval)					\
({									\
	raw_cpu_xchg(pcp, nval);					\
})

#define __this_cpu_cmpxchg(pcp, oval, nval)				\
({									\
	raw_cpu_cmpxchg(pcp, oval, nval);				\
})

#define __this_cpu_cmpxchg_double(pcp1, pcp2, oval1, oval2, nval1, nval2) \
	raw_cpu_cmpxchg_double(pcp1, pcp2, oval1, oval2, nval1, nval2);	\
})

#define __this_cpu_sub(pcp, val)	__this_cpu_add(pcp, -(typeof(pcp))(val))
#define __this_cpu_inc(pcp)		__this_cpu_add(pcp, 1)
#define __this_cpu_dec(pcp)		__this_cpu_sub(pcp, 1)
#define __this_cpu_sub_return(pcp, val)	__this_cpu_add_return(pcp, -(typeof(pcp))(val))
#define __this_cpu_inc_return(pcp)	__this_cpu_add_return(pcp, 1)
#define __this_cpu_dec_return(pcp)	__this_cpu_add_return(pcp, -1)

/*
 * pcp is an lvalue at a percpu address (a DEFINE_PER_CPU variable, or
 * arr[i] where arr came from alloc_percpu). Take its address, resolve
 * it to a real pointer in the current thread's chunk, and operate on
 * that. Direct lvalue ops on pcp would dereference the small offset
 * for alloc_percpu()'d memory and segfault.
 */
#define this_cpu_read(pcp)		(*this_cpu_ptr(&(pcp)))
#define this_cpu_write(pcp, val)	(*this_cpu_ptr(&(pcp)) = (val))
#define this_cpu_add(pcp, val)		(*this_cpu_ptr(&(pcp)) += (val))
#define this_cpu_and(pcp, val)		(*this_cpu_ptr(&(pcp)) &= (val))
#define this_cpu_or(pcp, val)		(*this_cpu_ptr(&(pcp)) |= (val))
#define this_cpu_add_return(pcp, val)	(*this_cpu_ptr(&(pcp)) += (val))
#define this_cpu_xchg(pcp, nval)					\
({									\
	typeof(pcp) *_p = this_cpu_ptr(&(pcp));				\
	typeof(pcp) _r = *_p;						\
	*_p = (nval);							\
	_r;								\
})

#define this_cpu_cmpxchg(pcp, oval, nval) \
	__pcpu_size_call_return2(this_cpu_cmpxchg_, pcp, oval, nval)
#define this_cpu_cmpxchg_double(pcp1, pcp2, oval1, oval2, nval1, nval2) \
	__pcpu_double_call_return_bool(this_cpu_cmpxchg_double_, pcp1, pcp2, oval1, oval2, nval1, nval2)

#define this_cpu_sub(pcp, val)		this_cpu_add(pcp, -(typeof(pcp))(val))
#define this_cpu_inc(pcp)		this_cpu_add(pcp, 1)
#define this_cpu_dec(pcp)		this_cpu_sub(pcp, 1)
#define this_cpu_sub_return(pcp, val)	this_cpu_add_return(pcp, -(typeof(pcp))(val))
#define this_cpu_inc_return(pcp)	this_cpu_add_return(pcp, 1)
#define this_cpu_dec_return(pcp)	this_cpu_add_return(pcp, -1)

#endif /* __TOOLS_LINUX_PERCPU_H */
