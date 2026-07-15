/*
 * Userspace shim for kernel-style percpu — both static (DEFINE_PER_CPU)
 * and dynamic (alloc_percpu).
 *
 * Per-thread chunk layout:
 *
 *   [ static section ][ dynamic arena ]
 *   |                |
 *   0                static_size       static_size + BCH_PERCPU_DYNAMIC_SIZE
 *
 * Static section is sized at link time by the linker auto-generated
 * symbols __start_bch_percpu / __stop_bch_percpu. DEFINE_PER_CPU vars
 * land there; their address-within-section is their offset within the
 * chunk (the resolve macro subtracts __start_bch_percpu).
 *
 * Dynamic arena is fixed at BCH_PERCPU_DYNAMIC_SIZE bytes per chunk.
 * alloc_percpu() returns offsets into [static_size, static_size +
 * BCH_PERCPU_DYNAMIC_SIZE), cast as a void *. The resolve macro adds
 * the offset directly.
 *
 * Distinguishing static-section addresses from dynamic offsets at
 * runtime: section addresses are real VAs (typically megabytes); dynamic
 * offsets are < static_size + dynamic_size. A single < check decides.
 *
 * Per-thread setup runs through bch_percpu_thread_init() (called from
 * kthread_start_fn(), linux_shrinkers_init(), rust_fuse_rcu_register(),
 * and a constructor here that bootstraps slot 0 before any module_init
 * runs). Subsystems that need per-instance setup register init_one /
 * exit_one callbacks via bch_percpu_register(); the registry runs them
 * for every live chunk plus future ones.
 *
 * The dynamic allocator is bump + freelist. Allocations return zeroed
 * memory across all live chunks; new threads get zeroed chunks via
 * calloc, which preserves the contract on subsequent allocations.
 *
 * Caller contract for alloc_percpu(): zero-init must be a valid initial
 * state. Things that need real per-instance setup (semaphores etc.)
 * should use DEFINE_PER_CPU + the registry instead.
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <linux/percpu.h>

#include "fs/util/darray.h"
#include "fs/util/util.h"

extern char __start_bch_percpu[], __stop_bch_percpu[];

__thread void *bch_percpu_my_chunk;
__thread int   bch_percpu_my_id = -1;

void   *bch_percpu_chunks[BCH_PERCPU_MAX_CPUS];
int     bch_percpu_nr_cpus;
size_t  bch_percpu_static_size;

#define BCH_PERCPU_GRAIN	8

#define BCH_PERCPU_MAX_CALLBACKS 32

struct bch_percpu_callback {
	void (*init_one)(void *);
	void (*exit_one)(void *);
	void *pcv;
};

static struct bch_percpu_callback callbacks[BCH_PERCPU_MAX_CALLBACKS];
static int		nr_callbacks;

struct bch_percpu_free_run {
	size_t	off;
	size_t	size;
};

static DARRAY(struct bch_percpu_free_run) free_runs;

/*
 * Per-thread init callbacks for dynamically-allocated percpu vars
 * (alloc_percpu()). Registered via bch2_alloc_percpu_init(); called for
 * every existing thread chunk at registration time and for every future
 * thread chunk at thread-create time.
 */
struct bch_percpu_dynamic_init {
	void	*pcv;
	void	(*init)(void *p, void *ctx, unsigned cpu);
	void	*ctx;
};

static DARRAY(struct bch_percpu_dynamic_init) dynamic_inits;
static size_t		dynamic_used;
/*
 * Map from grain index to allocation size in grains, so free_percpu()
 * doesn't need a size argument.
 */
static u16		size_at_grain[BCH_PERCPU_DYNAMIC_SIZE / BCH_PERCPU_GRAIN];

static pthread_mutex_t	bch_percpu_lock = PTHREAD_MUTEX_INITIALIZER;

void bch_percpu_register(void (*init_one)(void *),
			 void (*exit_one)(void *),
			 void *pcv)
{
	pthread_mutex_lock(&bch_percpu_lock);

	if (nr_callbacks == BCH_PERCPU_MAX_CALLBACKS) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "bch_percpu_register: callback table full\n");
		abort();
	}

	int idx = nr_callbacks++;
	callbacks[idx] = (struct bch_percpu_callback){ init_one, exit_one, pcv };

	for (int cpu = 0; cpu < bch_percpu_nr_cpus; cpu++)
		if (bch_percpu_chunks[cpu] && init_one)
			init_one(__bch_percpu_resolve(pcv, bch_percpu_chunks[cpu]));

	pthread_mutex_unlock(&bch_percpu_lock);
}

void bch_percpu_thread_init(void)
{
	if (bch_percpu_my_chunk)
		return;

	pthread_mutex_lock(&bch_percpu_lock);

	if (!bch_percpu_static_size)
		bch_percpu_static_size = __stop_bch_percpu - __start_bch_percpu;

	size_t chunk_size = bch_percpu_static_size + BCH_PERCPU_DYNAMIC_SIZE;
	void *chunk = calloc(1, chunk_size);
	if (!chunk) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "bch_percpu_thread_init: out of memory\n");
		abort();
	}

	int my_id = bch_percpu_nr_cpus++;
	if (my_id >= BCH_PERCPU_MAX_CPUS) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "bch_percpu_thread_init: too many threads (max %d)\n",
			BCH_PERCPU_MAX_CPUS);
		abort();
	}

	bch_percpu_my_chunk = chunk;
	bch_percpu_my_id    = my_id;
	bch_percpu_chunks[my_id] = chunk;

	for (int i = 0; i < nr_callbacks; i++)
		if (callbacks[i].init_one)
			callbacks[i].init_one(__bch_percpu_resolve(callbacks[i].pcv, chunk));

	darray_for_each(dynamic_inits, di)
		di->init(__bch_percpu_resolve(di->pcv, chunk), di->ctx, my_id);

	pthread_mutex_unlock(&bch_percpu_lock);
}

void __bch2_alloc_percpu_init(void *pcv,
			      void (*init)(void *p, void *ctx, unsigned cpu),
			      void *ctx)
{
	pthread_mutex_lock(&bch_percpu_lock);

	for (int cpu = 0; cpu < bch_percpu_nr_cpus; cpu++)
		if (bch_percpu_chunks[cpu])
			init(__bch_percpu_resolve(pcv, bch_percpu_chunks[cpu]), ctx, cpu);

	if (darray_push(&dynamic_inits,
			((struct bch_percpu_dynamic_init){pcv, init, ctx}))) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "bch2_alloc_percpu_init: out of memory registering init\n");
		abort();
	}

	pthread_mutex_unlock(&bch_percpu_lock);
}

/* Caller must hold bch_percpu_lock. Returns offset within dynamic arena (in
 * bytes), or SIZE_MAX on no space. */
static size_t bch_percpu_dynamic_alloc(size_t size)
{
	size_t off = SIZE_MAX;

	darray_for_each(free_runs, run)
		if (run->size >= size) {
			off = run->off;
			if (run->size > size) {
				run->off  += size;
				run->size -= size;
			} else {
				darray_remove_item(&free_runs, run);
			}
			return off;
		}

	if (dynamic_used + size > BCH_PERCPU_DYNAMIC_SIZE)
		return SIZE_MAX;

	off = dynamic_used;
	dynamic_used += size;
	return off;
}

void *__alloc_percpu_gfp(size_t size, size_t align, gfp_t gfp)
{
	/* Round to grain; align is honored implicitly because all offsets
	 * are grain-aligned and BCH_PERCPU_GRAIN is 8 (covers any alignof
	 * request bcachefs makes). */
	size = (size + BCH_PERCPU_GRAIN - 1) & ~(BCH_PERCPU_GRAIN - 1);

	pthread_mutex_lock(&bch_percpu_lock);

	size_t off = bch_percpu_dynamic_alloc(size);
	if (off == SIZE_MAX) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "alloc_percpu: dynamic arena exhausted "
			"(used %zu, requested %zu, max %d)\n",
			dynamic_used, size, BCH_PERCPU_DYNAMIC_SIZE);
		return NULL;
	}

	size_at_grain[off / BCH_PERCPU_GRAIN] = size / BCH_PERCPU_GRAIN;

	/* Zero across all live chunks (covers reuse from free list; new
	 * threads get calloc'd chunks so the slot is already zero in chunks
	 * created later). */
	size_t chunk_off = bch_percpu_static_size + off;
	for (int cpu = 0; cpu < bch_percpu_nr_cpus; cpu++)
		if (bch_percpu_chunks[cpu])
			memset((char *)bch_percpu_chunks[cpu] + chunk_off, 0, size);

	pthread_mutex_unlock(&bch_percpu_lock);

	return (void *)(uintptr_t)chunk_off;
}

void *__alloc_percpu(size_t size, size_t align)
{
	return __alloc_percpu_gfp(size, align, 0);
}

void free_percpu(void *p)
{
	if (!p)
		return;

	uintptr_t chunk_off = (uintptr_t)p;
	size_t off = chunk_off - bch_percpu_static_size;

	pthread_mutex_lock(&bch_percpu_lock);

	size_t grain = off / BCH_PERCPU_GRAIN;
	size_t size  = size_at_grain[grain] * (size_t)BCH_PERCPU_GRAIN;
	size_at_grain[grain] = 0;

	if (darray_push(&free_runs, ((struct bch_percpu_free_run){off, size}))) {
		/* OOM appending to free list: leak the slot rather than crash.
		 * This shouldn't happen in practice — free_runs is bounded by
		 * the number of live allocations, which fits in 64KB / 8B. */
		fprintf(stderr, "free_percpu: free list push failed; leaking slot\n");
	}

	pthread_mutex_unlock(&bch_percpu_lock);
}

/*
 * Run before any module_init() (priority 120): module_init constructors
 * are kernel-mirror code that may iterate for_each_possible_cpu() over
 * DEFINE_PER_CPU storage; that needs slot 0 to exist with a real chunk
 * before they run. Allocates slot 0 in the calling thread's TLS, which
 * is the main thread (constructors run on it).
 */
__attribute__((constructor(110)))
static void bch_percpu_module_init(void)
{
	bch_percpu_thread_init();
}

__attribute__((destructor))
static void bch_percpu_module_exit(void)
{
	pthread_mutex_lock(&bch_percpu_lock);
	for (int cpu = 0; cpu < bch_percpu_nr_cpus; cpu++) {
		void *chunk = bch_percpu_chunks[cpu];
		if (!chunk)
			continue;

		for (int i = nr_callbacks - 1; i >= 0; i--)
			if (callbacks[i].exit_one)
				callbacks[i].exit_one(__bch_percpu_resolve(callbacks[i].pcv, chunk));

		free(chunk);
		bch_percpu_chunks[cpu] = NULL;
	}
	darray_exit(&free_runs);
	darray_exit(&dynamic_inits);
	pthread_mutex_unlock(&bch_percpu_lock);
}
