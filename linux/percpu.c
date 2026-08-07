/*
 * Userspace shim for kernel-style percpu — both static (DEFINE_PER_CPU)
 * and dynamic (alloc_percpu).
 *
 * Per-thread chunk layout:
 *
 *   [ static section ][ dynamic arena ]
 *   |                |
 *   0                static_size       static_size + bch_percpu_dynamic_size
 *
 * Static section is sized at link time by the linker auto-generated
 * symbols __start_bch_percpu / __stop_bch_percpu. DEFINE_PER_CPU vars
 * land there; their address-within-section is their offset within the
 * chunk (the resolve macro subtracts __start_bch_percpu).
 *
 * Dynamic arena is bch_percpu_dynamic_size bytes per chunk, fixed at first
 * thread init.
 *
 * Static and dynamic percpu pointers are one representation: an address
 * relative to __start_bch_percpu. The linker gives static variables theirs;
 * alloc_percpu() returns __start_bch_percpu + chunk_off for the same effect.
 * So the resolve macro subtracts that base and adds the chunk, with nothing to
 * discriminate between the two - which is also how the kernel does it, see
 * __addr_to_pcpu_ptr() in mm/percpu.c.
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
 * anonymous mmap, which preserves the contract on subsequent allocations.
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
#include <sys/mman.h>

#include <linux/percpu.h>

#include "fs/util/darray.h"
#include "fs/util/util.h"

extern char __start_bch_percpu[], __stop_bch_percpu[];

__thread void *bch_percpu_my_chunk;
__thread int   bch_percpu_my_id = -1;

void   *bch_percpu_chunks[BCH_PERCPU_MAX_CPUS];
int     bch_percpu_nr_cpus;
size_t  bch_percpu_static_size;
size_t  bch_percpu_dynamic_size;

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
 * Map from grain index to allocation size in grains, so free_percpu() doesn't
 * need a size argument.
 *
 * Grown to cover dynamic_used rather than sized to the whole arena: the bump
 * allocator only moves forward, and free_percpu() only ever looks up a grain
 * that was allocated, so nothing above the high water mark is ever read. Sized
 * to the arena instead this would be a 64MB BSS array to describe a few
 * thousand live allocations.
 */
static u32		*size_at_grain;
static size_t		nr_size_at_grain;

/* Caller must hold bch_percpu_lock. */
static bool size_at_grain_resize(size_t nr)
{
	if (nr <= nr_size_at_grain)
		return true;

	size_t new_nr = max(nr, nr_size_at_grain * 2);
	u32 *new = realloc(size_at_grain, new_nr * sizeof(*size_at_grain));
	if (!new)
		return false;

	memset(new + nr_size_at_grain, 0,
	       (new_nr - nr_size_at_grain) * sizeof(*new));
	size_at_grain	 = new;
	nr_size_at_grain = new_nr;
	return true;
}

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

	if (!bch_percpu_static_size) {
		bch_percpu_static_size = __stop_bch_percpu - __start_bch_percpu;

		/*
		 * The arena is address space, not memory, so on 64 bit we can
		 * be generous - but a 32 bit process has ~3G of it to share
		 * between every thread's chunk and everything else, so it gets
		 * a much smaller reservation.
		 */
		bch_percpu_dynamic_size = sizeof(void *) > 4
			? 256UL << 20
			:   8UL << 20;
	}

	/* Address space, not memory - pages fault in as they're touched: */
	size_t chunk_size = bch_percpu_static_size + bch_percpu_dynamic_size;
	void *chunk = mmap(NULL, chunk_size, PROT_READ|PROT_WRITE,
			   MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE, -1, 0);
	if (chunk == MAP_FAILED) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "bch_percpu_thread_init: chunk mmap failed\n");
		abort();
	}

	int my_id = bch_percpu_nr_cpus;
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

	/*
	 * Publish the slot last. Readers don't take bch_percpu_lock: they walk
	 * [0, bch_percpu_nr_cpus) and per_cpu_ptr() dereferences the chunk
	 * without a NULL check, so bumping the count before the chunk is
	 * installed and initialized hands them a NULL - or a chunk whose
	 * counters haven't been zeroed yet.
	 */
	smp_store_release(&bch_percpu_nr_cpus, my_id + 1);

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

	if (dynamic_used + size > bch_percpu_dynamic_size)
		return SIZE_MAX;

	/* Before committing, so a failed grow leaves nothing half done. Reuse
	 * from the free list needs no grow - those grains are covered. */
	if (!size_at_grain_resize((dynamic_used + size) / BCH_PERCPU_GRAIN)) {
		fprintf(stderr, "alloc_percpu: out of memory growing the size table\n");
		return SIZE_MAX;
	}

	off = dynamic_used;
	dynamic_used += size;
	return off;
}

void *__alloc_percpu_gfp(size_t size, size_t align, gfp_t gfp)
{
	/*
	 * Zero has no grain to record a size in: the free list would match the
	 * first run without consuming anything and hand the same offset out
	 * again, and the bump path would write one past the end of
	 * size_at_grain. No caller wants a zero sized percpu variable anyway.
	 */
	BUG_ON(!size);

	/* Round to grain; align is honored implicitly because all offsets
	 * are grain-aligned and BCH_PERCPU_GRAIN is 8 (covers any alignof
	 * request bcachefs makes). */
	size = (size + BCH_PERCPU_GRAIN - 1) & ~(BCH_PERCPU_GRAIN - 1);

	pthread_mutex_lock(&bch_percpu_lock);

	size_t off = bch_percpu_dynamic_alloc(size);
	if (off == SIZE_MAX) {
		pthread_mutex_unlock(&bch_percpu_lock);
		fprintf(stderr, "alloc_percpu: dynamic arena exhausted "
			"(used %zu, requested %zu, max %zu)\n",
			dynamic_used, size, bch_percpu_dynamic_size);
		return NULL;
	}

	size_at_grain[off / BCH_PERCPU_GRAIN] = size / BCH_PERCPU_GRAIN;

	/* Zero across all live chunks (covers reuse from free list; new
	 * threads get zero-filled mmap'd chunks so the slot is already zero in chunks
	 * created later). */
	size_t chunk_off = bch_percpu_static_size + off;
	for (int cpu = 0; cpu < bch_percpu_nr_cpus; cpu++)
		if (bch_percpu_chunks[cpu])
			memset((char *)bch_percpu_chunks[cpu] + chunk_off, 0, size);

	pthread_mutex_unlock(&bch_percpu_lock);

	return __start_bch_percpu + chunk_off;
}

void *__alloc_percpu(size_t size, size_t align)
{
	return __alloc_percpu_gfp(size, align, 0);
}

void free_percpu(void *p)
{
	if (!p)
		return;

	/*
	 * Everything else is a caller bug: a DEFINE_PER_CPU variable, a
	 * pointer from somewhere other than alloc_percpu(), or a double free.
	 * Returning quietly turns any of those into an arena slot that is
	 * never reused and never reported - so say so instead.
	 *
	 * is_static_percpu() is the same test as chunk_off <
	 * bch_percpu_static_size, since static_size is exactly
	 * __stop_bch_percpu - __start_bch_percpu; one bounds check covers
	 * both ends.
	 */
	size_t off = ((uintptr_t)p - (uintptr_t)__start_bch_percpu) -
		bch_percpu_static_size;
	BUG_ON(off >= bch_percpu_dynamic_size);

	pthread_mutex_lock(&bch_percpu_lock);

	/*
	 * size_at_grain only covers what's been allocated, so a pointer we
	 * never handed out indexes past the end - where the old whole-arena
	 * array would quietly have read a zero.
	 */
	size_t grain = off / BCH_PERCPU_GRAIN;
	BUG_ON(grain >= nr_size_at_grain);

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

		munmap(chunk, bch_percpu_static_size + bch_percpu_dynamic_size);
		bch_percpu_chunks[cpu] = NULL;
	}
	darray_exit(&free_runs);
	darray_exit(&dynamic_inits);
	free(size_at_grain);
	size_at_grain	 = NULL;
	nr_size_at_grain = 0;
	pthread_mutex_unlock(&bch_percpu_lock);
}
