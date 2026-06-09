#include <pthread.h>
#include <stdlib.h>
#include <string.h>

#include "tools-util.h"

#include <linux/atomic.h>
#include <linux/errname.h>
#include <linux/kthread.h>
#include <linux/percpu.h>
#include <linux/slab.h>
#include <linux/workqueue.h>

static pthread_mutex_t	wq_lock = PTHREAD_MUTEX_INITIALIZER;
static LIST_HEAD(wq_list);

#define WQ_SHIM_MAX_WORKERS	min(num_online_cpus(), 32U)

struct workqueue_struct {
	struct list_head	list;

	char			name[64];
	unsigned int		max_active;

	struct task_struct	**worker_tasks;
	unsigned int		nr_workers;

	atomic_t		active_count;
	struct work_struct	**worker_current;
	struct list_head	pending;
};

enum {
	WORK_PENDING_BIT,
};

static bool work_pending(struct work_struct *work)
{
	return test_bit(WORK_PENDING_BIT, work_data_bits(work));
}

static void clear_work_pending(struct work_struct *work)
{
	clear_bit(WORK_PENDING_BIT, work_data_bits(work));
}

static bool set_work_pending(struct work_struct *work)
{
	return !test_and_set_bit(WORK_PENDING_BIT, work_data_bits(work));
}

static int wq_worker_thread(void *arg)
{
	struct workqueue_struct *wq = arg;
	struct work_struct *work;
	unsigned int i;

	pthread_mutex_lock(&wq_lock);
	while (1) {
		set_current_state(TASK_INTERRUPTIBLE);

		work = list_first_entry_or_null(&wq->pending,
				struct work_struct, entry);

		if (kthread_should_stop()) {
			BUG_ON(work);
			break;
		}

		if (!work) {
			pthread_mutex_unlock(&wq_lock);
			schedule();
			pthread_mutex_lock(&wq_lock);
			continue;
		}
		__set_current_state(TASK_RUNNING);

		for (i = 0; i < wq->nr_workers; i++)
			if (wq->worker_tasks[i] == current)
				break;

		BUG_ON(i == wq->nr_workers);
		wq->worker_current[i] = work;

		BUG_ON(!work_pending(work));
		list_del_init(&work->entry);
		clear_work_pending(work);
		atomic_inc(&wq->active_count);

		pthread_mutex_unlock(&wq_lock);
		work->func(work);
		pthread_mutex_lock(&wq_lock);

		wq->worker_current[i] = NULL;
		atomic_dec(&wq->active_count);
	}
	pthread_mutex_unlock(&wq_lock);

	return 0;
}

static struct task_struct *create_wq_worker(struct workqueue_struct *wq)
{
	struct task_struct *p;
	unsigned int idx = wq->nr_workers;

	if (idx >= wq->max_active)
		return NULL;

	p = kthread_run(wq_worker_thread, wq, "%s/%u", wq->name, idx);
	if (IS_ERR(p))
		return p;

	wq->worker_tasks[idx] = p;
	wq->nr_workers++;
	return p;
}

static void wake_up_wq_workers(struct workqueue_struct *wq)
{
	for (unsigned int i = 0; i < wq->nr_workers; i++)
		wake_up_process(wq->worker_tasks[i]);
}

static void __queue_work(struct workqueue_struct *wq,
			 struct work_struct *work)
{
	BUG_ON(!work_pending(work));
	BUG_ON(!list_empty(&work->entry));

	list_add_tail(&work->entry, &wq->pending);

	if (wq->nr_workers == 0)
		create_wq_worker(wq);

	wake_up_wq_workers(wq);
}

bool queue_work(struct workqueue_struct *wq, struct work_struct *work)
{
	bool ret;

	pthread_mutex_lock(&wq_lock);
	if ((ret = set_work_pending(work)))
		__queue_work(wq, work);
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

void delayed_work_timer_fn(struct timer_list *timer)
{
	struct delayed_work *dwork =
		container_of(timer, struct delayed_work, timer);

	pthread_mutex_lock(&wq_lock);
	__queue_work(dwork->wq, &dwork->work);
	pthread_mutex_unlock(&wq_lock);
}

static void __queue_delayed_work(struct workqueue_struct *wq,
				 struct delayed_work *dwork,
				 unsigned long delay)
{
	struct timer_list *timer = &dwork->timer;
	struct work_struct *work = &dwork->work;

	BUG_ON(timer->function != delayed_work_timer_fn);
	BUG_ON(timer_pending(timer));
	BUG_ON(!list_empty(&work->entry));

	if (!delay) {
		__queue_work(wq, &dwork->work);
	} else {
		dwork->wq = wq;
		timer->expires = jiffies + delay;
		add_timer(timer);
	}
}

bool queue_delayed_work(struct workqueue_struct *wq,
			struct delayed_work *dwork,
			unsigned long delay)
{
	struct work_struct *work = &dwork->work;
	bool ret;

	pthread_mutex_lock(&wq_lock);
	if ((ret = set_work_pending(work)))
		__queue_delayed_work(wq, dwork, delay);
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

static bool grab_pending(struct work_struct *work, bool is_dwork)
{
retry:
	if (set_work_pending(work)) {
		BUG_ON(!list_empty(&work->entry));
		return false;
	}

	if (is_dwork) {
		struct delayed_work *dwork = to_delayed_work(work);

		if (likely(del_timer(&dwork->timer))) {
			BUG_ON(!list_empty(&work->entry));
			return true;
		}
	}

	if (!list_empty(&work->entry)) {
		list_del_init(&work->entry);
		return true;
	}

	BUG_ON(!is_dwork);

	pthread_mutex_unlock(&wq_lock);
	flush_timers();
	pthread_mutex_lock(&wq_lock);
	goto retry;
}

static bool work_running(struct work_struct *work)
{
	struct workqueue_struct *wq;
	unsigned int i;

	list_for_each_entry(wq, &wq_list, list) {
		for (i = 0; i < wq->nr_workers; i++)
			if (wq->worker_current[i] == work)
				return true;
	}

	return false;
}

bool flush_work(struct work_struct *work)
{
	bool ret = false;

	pthread_mutex_lock(&wq_lock);
	while (work_pending(work) || work_running(work)) {
		pthread_mutex_unlock(&wq_lock);
		schedule();
		pthread_mutex_lock(&wq_lock);
		ret = true;
	}
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

static bool __flush_work(struct work_struct *work)
{
	bool ret = false;

	while (work_running(work)) {
		pthread_mutex_unlock(&wq_lock);
		schedule();
		pthread_mutex_lock(&wq_lock);
		ret = true;
	}

	return ret;
}

bool cancel_work_sync(struct work_struct *work)
{
	bool ret;

	pthread_mutex_lock(&wq_lock);
	ret = grab_pending(work, false);

	__flush_work(work);
	clear_work_pending(work);
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

bool mod_delayed_work(struct workqueue_struct *wq,
		      struct delayed_work *dwork,
		      unsigned long delay)
{
	struct work_struct *work = &dwork->work;
	bool ret;

	pthread_mutex_lock(&wq_lock);
	ret = grab_pending(work, true);

	__queue_delayed_work(wq, dwork, delay);
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

bool cancel_delayed_work(struct delayed_work *dwork)
{
	struct work_struct *work = &dwork->work;
	bool ret;

	pthread_mutex_lock(&wq_lock);
	ret = grab_pending(work, true);

	clear_work_pending(&dwork->work);
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

bool cancel_delayed_work_sync(struct delayed_work *dwork)
{
	struct work_struct *work = &dwork->work;
	bool ret;

	pthread_mutex_lock(&wq_lock);
	ret = grab_pending(work, true);

	__flush_work(work);
	clear_work_pending(work);
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

void drain_workqueue(struct workqueue_struct *wq)
{
	pthread_mutex_lock(&wq_lock);
	while (!list_empty(&wq->pending) || atomic_read(&wq->active_count) > 0) {
		pthread_mutex_unlock(&wq_lock);
		schedule();
		pthread_mutex_lock(&wq_lock);
	}
	pthread_mutex_unlock(&wq_lock);
}

void flush_workqueue(struct workqueue_struct *wq)
{
	drain_workqueue(wq);
}

void destroy_workqueue(struct workqueue_struct *wq)
{
	struct task_struct **tasks;
	unsigned int n;

	pthread_mutex_lock(&wq_lock);
	list_del(&wq->list);
	tasks = wq->worker_tasks;
	n = wq->nr_workers;
	pthread_mutex_unlock(&wq_lock);

	for (unsigned int i = 0; i < n; i++) {
		kthread_stop(tasks[i]);
		put_task_struct(tasks[i]);
	}

	free(wq->worker_current);
	free(tasks);
	kfree(wq);
}

static unsigned int compute_nr_workers(int max_active)
{
	unsigned int nr;

	if (max_active <= 0)
		nr = 1;
	else if ((unsigned int)max_active > WQ_MAX_ACTIVE)
		nr = WQ_SHIM_MAX_WORKERS;
	else
		nr = (unsigned int)max_active;

	if (nr > WQ_SHIM_MAX_WORKERS)
		nr = WQ_SHIM_MAX_WORKERS;
	return nr;
}

struct workqueue_struct *alloc_workqueue(const char *fmt,
					 unsigned flags,
					 int max_active,
					 ...)
{
	va_list args;
	struct workqueue_struct *wq;

	wq = kzalloc(sizeof(*wq), GFP_KERNEL);
	if (!wq)
		return NULL;

	INIT_LIST_HEAD(&wq->pending);
	atomic_set(&wq->active_count, 0);
	wq->max_active = compute_nr_workers(max_active);

	va_start(args, max_active);
	vsnprintf(wq->name, sizeof(wq->name), fmt, args);
	va_end(args);

	wq->worker_tasks = calloc(wq->max_active, sizeof(struct task_struct *));
	if (!wq->worker_tasks) {
		kfree(wq);
		return NULL;
	}

	wq->worker_current = calloc(wq->max_active, sizeof(struct work_struct *));
	if (!wq->worker_current) {
		free(wq->worker_tasks);
		kfree(wq);
		return NULL;
	}

	pthread_mutex_lock(&wq_lock);
	list_add(&wq->list, &wq_list);
	pthread_mutex_unlock(&wq_lock);

	return wq;
}

struct workqueue_struct *system_wq;
struct workqueue_struct *system_highpri_wq;
struct workqueue_struct *system_long_wq;
struct workqueue_struct *system_unbound_wq;
struct workqueue_struct *system_freezable_wq;

__attribute__((constructor(102)))
static void wq_init(void)
{
	system_wq = alloc_workqueue("events", 0, 0);
	system_highpri_wq = alloc_workqueue("events_highpri", WQ_HIGHPRI, 0);
	system_long_wq = alloc_workqueue("events_long", 0, 0);
	system_unbound_wq = alloc_workqueue("events_unbound", WQ_UNBOUND,
					    WQ_UNBOUND_MAX_ACTIVE);
	system_freezable_wq = alloc_workqueue("events_freezable",
					      WQ_FREEZABLE, 0);
	BUG_ON(!system_wq || !system_highpri_wq || !system_long_wq ||
	       !system_unbound_wq || !system_freezable_wq);
}
