
#include <pthread.h>
#include <signal.h>
#include <time.h>

#include <linux/kernel.h>
#include <linux/kthread.h>
#include <linux/timer.h>

/**
 * timespec_add_ns - Adds nanoseconds to a timespec
 * @a:		pointer to timespec to be incremented
 * @ns:		unsigned nanoseconds value to be added
 *
 * This must always be inlined because its used from the x86-64 vdso,
 * which cannot call other kernel functions.
 */
static struct timespec timespec_add_ns(struct timespec a, u64 ns)
{
	a.tv_nsec	+= ns;
	a.tv_sec	+= a.tv_nsec / NSEC_PER_SEC;
	a.tv_nsec	%= NSEC_PER_SEC;
	return a;
}

#define DECLARE_HEAP(type)						\
struct {								\
	size_t size, used;						\
	type *data;							\
}

#define heap_init(heap, _size)						\
({									\
	size_t _bytes;							\
	(heap)->used = 0;						\
	(heap)->size = (_size);						\
	_bytes = (heap)->size * sizeof(*(heap)->data);			\
	(heap)->data = malloc(_bytes);					\
	(heap)->data;							\
})

#define heap_free(heap)							\
do {									\
	free((heap)->data);						\
	(heap)->data = NULL;						\
} while (0)

#define heap_swap(h, i, j)	swap((h)->data[i], (h)->data[j])

#define heap_sift(h, i, cmp)						\
do {									\
	size_t _r, _j = i;						\
									\
	for (; _j * 2 + 1 < (h)->used; _j = _r) {			\
		_r = _j * 2 + 1;					\
		if (_r + 1 < (h)->used &&				\
		    cmp((h)->data[_r], (h)->data[_r + 1]))		\
			_r++;						\
									\
		if (cmp((h)->data[_r], (h)->data[_j]))			\
			break;						\
		heap_swap(h, _r, _j);					\
	}								\
} while (0)

#define heap_sift_down(h, i, cmp)					\
do {									\
	while (i) {							\
		size_t p = (i - 1) / 2;					\
		if (cmp((h)->data[i], (h)->data[p]))			\
			break;						\
		heap_swap(h, i, p);					\
		i = p;							\
	}								\
} while (0)

#define heap_add(h, d, cmp)						\
({									\
	bool _r = !heap_full(h);					\
	if (_r) {							\
		size_t _i = (h)->used++;				\
		(h)->data[_i] = d;					\
									\
		heap_sift_down(h, _i, cmp);				\
		heap_sift(h, _i, cmp);					\
	}								\
	_r;								\
})

#define heap_del(h, i, cmp)						\
do {									\
	size_t _i = (i);						\
									\
	BUG_ON(_i >= (h)->used);					\
	(h)->used--;							\
	if ((_i) < (h)->used) {						\
		heap_swap(h, _i, (h)->used);				\
		heap_sift_down(h, _i, cmp);				\
		heap_sift(h, _i, cmp);					\
	}								\
} while (0)

#define heap_pop(h, d, cmp)						\
({									\
	bool _r = (h)->used;						\
	if (_r) {							\
		(d) = (h)->data[0];					\
		heap_del(h, 0, cmp);					\
	}								\
	_r;								\
})

#define heap_peek(h)	((h)->used ? &(h)->data[0] : NULL)
#define heap_full(h)	((h)->used == (h)->size)
#define heap_empty(h)	((h)->used == 0)

#define heap_resort(heap, cmp)						\
do {									\
	ssize_t _i;							\
	for (_i = (ssize_t) (heap)->used / 2 -  1; _i >= 0; --_i)	\
		heap_sift(heap, _i, cmp);				\
} while (0)

struct pending_timer {
	struct timer_list	*timer;
	unsigned long		expires;
};

static inline bool pending_timer_cmp(struct pending_timer a,
				     struct pending_timer b)
{
	return a.expires < b.expires;
}

static DECLARE_HEAP(struct pending_timer) pending_timers;

static pthread_mutex_t	timer_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t	timer_cond = PTHREAD_COND_INITIALIZER;
static pthread_cond_t	timer_running_cond = PTHREAD_COND_INITIALIZER;
static unsigned long	timer_seq;

static inline bool timer_running(void)
{
	return timer_seq & 1;
}

static ssize_t timer_idx(struct timer_list *timer)
{
	size_t i;

	for (i = 0; i < pending_timers.used; i++)
		if (pending_timers.data[i].timer == timer)
			return i;

	return -1;
}

int del_timer(struct timer_list *timer)
{
	ssize_t idx;

	pthread_mutex_lock(&timer_lock);
	idx = timer_idx(timer);
	if (idx >= 0)
		heap_del(&pending_timers, idx, pending_timer_cmp);

	timer->pending = false;
	pthread_mutex_unlock(&timer_lock);

	return idx >= 0;
}

void flush_timers(void)
{
	unsigned long seq;

	pthread_mutex_lock(&timer_lock);
	seq = timer_seq;
	while (timer_running() && seq == timer_seq)
		pthread_cond_wait(&timer_running_cond, &timer_lock);

	pthread_mutex_unlock(&timer_lock);
}

int del_timer_sync(struct timer_list *timer)
{
	unsigned long seq;
	ssize_t idx;

	pthread_mutex_lock(&timer_lock);
	idx = timer_idx(timer);
	if (idx >= 0)
		heap_del(&pending_timers, idx, pending_timer_cmp);

	timer->pending = false;

	seq = timer_seq;
	while (timer_running() && seq == timer_seq)
		pthread_cond_wait(&timer_running_cond, &timer_lock);
	pthread_mutex_unlock(&timer_lock);

	return idx >= 0;
}

int mod_timer(struct timer_list *timer, unsigned long expires)
{
	ssize_t idx;

	pthread_mutex_lock(&timer_lock);
	timer->expires = expires;
	timer->pending = true;
	idx = timer_idx(timer);

	if (idx >= 0 &&
	    pending_timers.data[idx].expires == expires)
		goto out;

	if (idx >= 0) {
		pending_timers.data[idx].expires = expires;

		heap_sift_down(&pending_timers, idx, pending_timer_cmp);
		heap_sift(&pending_timers, idx, pending_timer_cmp);
	} else {
		if (heap_full(&pending_timers)) {
			pending_timers.size *= 2;
			pending_timers.data =
				realloc(pending_timers.data,
					pending_timers.size *
					sizeof(struct pending_timer));

			BUG_ON(!pending_timers.data);
		}

		heap_add(&pending_timers,
			 ((struct pending_timer) {
				.timer = timer,
				.expires = expires,
			 }),
			 pending_timer_cmp);
	}

	pthread_cond_signal(&timer_cond);
out:
	pthread_mutex_unlock(&timer_lock);

	return idx >= 0;
}

static bool timer_thread_stop = false;

static int timer_thread(void *arg)
{
	struct pending_timer *p;
	struct timespec ts;
	unsigned long now;
	int ret;

	pthread_mutex_lock(&timer_lock);

	while (!timer_thread_stop) {
		now = jiffies;
		p = heap_peek(&pending_timers);

		if (!p) {
			pthread_cond_wait(&timer_cond, &timer_lock);
			continue;
		}

		if (time_after_eq(now, p->expires)) {
			struct timer_list *timer = p->timer;

			heap_del(&pending_timers, 0, pending_timer_cmp);
			BUG_ON(!timer_pending(timer));
			timer->pending = false;

			timer_seq++;
			BUG_ON(!timer_running());

			pthread_mutex_unlock(&timer_lock);
			timer->function(timer);
			pthread_mutex_lock(&timer_lock);

			timer_seq++;
			pthread_cond_broadcast(&timer_running_cond);
			continue;
		}


		ret = clock_gettime(CLOCK_REALTIME, &ts);
		BUG_ON(ret);

		ts = timespec_add_ns(ts, jiffies_to_nsecs(p->expires - now));

		pthread_cond_timedwait(&timer_cond, &timer_lock, &ts);
	}

	pthread_mutex_unlock(&timer_lock);

	return 0;
}

struct task_struct *timer_task;

__attribute__((constructor(103)))
static void timers_init(void)
{
	heap_init(&pending_timers, 64);
	BUG_ON(!pending_timers.data);

	timer_task = kthread_run(timer_thread, NULL, "timers");
	BUG_ON(IS_ERR(timer_task));
}

__attribute__((destructor(103)))
static void timers_cleanup(void)
{
	get_task_struct(timer_task);

	pthread_mutex_lock(&timer_lock);
	timer_thread_stop = true;
	pthread_cond_signal(&timer_cond);
	pthread_mutex_unlock(&timer_lock);

	int ret = kthread_stop(timer_task);
	BUG_ON(ret);

	put_task_struct(timer_task);
	timer_task = NULL;
	heap_free(&pending_timers);
}
