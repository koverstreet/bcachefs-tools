/*
 * How the libraries' headers are configured: liburcu's memb flavour, its
 * functions inlined (see below), and zstd's static-linking API - the
 * ZSTD_initStatic*Ctx() compress.rs runs in memory of its own.
 */
#define RCU_MEMBARRIER			1
#define _LGPL_SOURCE			1
#define ZSTD_STATIC_LINKING_ONLY	1

#include <urcu.h>

#include <lz4.h>
#include <lz4hc.h>
#include <zlib.h>
#include <zstd.h>
#include <zstd_errors.h>

#include <errno.h>

/*
 * liburcu's functions are macros, naming its flavour's - functions, or with
 * _LGPL_SOURCE static inlines, or no-ops: by the macros' names, wrappers
 * bindgen compiles, as it does the shim's inlines - build.rs strips the
 * prefix. So Rust calls the flavour C does, as C calls it.
 */
#define RUST_FN_HELPER(ret, name, params, args)				\
static inline ret RUST_FN_HELPER_##name params { return name args; }

RUST_FN_HELPER(void, rcu_init,			(void), ())
RUST_FN_HELPER(void, rcu_register_thread,	(void), ())
RUST_FN_HELPER(void, rcu_unregister_thread,	(void), ())
RUST_FN_HELPER(void, rcu_quiescent_state,	(void), ())
RUST_FN_HELPER(void, rcu_read_lock,		(void), ())
RUST_FN_HELPER(void, rcu_read_unlock,		(void), ())
RUST_FN_HELPER(void, synchronize_rcu,		(void), ())
RUST_FN_HELPER(void, rcu_barrier,		(void), ())
RUST_FN_HELPER(void, call_rcu,
	       (struct rcu_head *head, void (*func)(struct rcu_head *)), (head, func))
RUST_FN_HELPER(struct urcu_gp_poll_state, start_poll_synchronize_rcu, (void), ())
RUST_FN_HELPER(bool, poll_state_synchronize_rcu,
	       (struct urcu_gp_poll_state state), (state))
