#include <pthread.h>

#include "tools-util.h"

#include <linux/errname.h>
#include <linux/kthread.h>
#include <linux/slab.h>
#include <linux/workqueue.h>

static pthread_mutex_t	wq_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t	work_finished = PTHREAD_COND_INITIALIZER;
static LIST_HEAD(wq_list);

/*
 * One thread per queue meant a work item that blocks stopped every other item
 * on that queue. For an executor whose tasks are work items (see
 * fs/util/async_exec.rs) that is worse than slow: two tasks racing on one
 * queue, where the first blocks, is a deadlock - the second is what would have
 * woken the first.
 *
 * So: up to @max_active workers per queue, created when work arrives that no
 * free worker is left to take. Cruder than the kernel's concurrency
 * management, which spawns when a worker actually goes to sleep - there is no
 * scheduler hook here for that - but the same shape, and enough that a
 * blocking item cannot own the queue.
 *
 * The guarantee that has to be restored by hand once there is more than one
 * worker is non-reentrancy: the kernel never runs a single work item on two
 * workers at once, and callers rely on it. btree_interior_update_work() is one
 * - it peeks the head of a list and leaves the entry there until it is done
 * with it, so a second concurrent execution picks up the same entry and
 * processes it twice. One worker per queue supplied that exclusion for free;
 * find_runnable_work() is what supplies it now.
 */
struct wq_worker {
	struct list_head	list;
	struct workqueue_struct	*wq;
	struct task_struct	*task;
	struct work_struct	*current_work;
};

struct workqueue_struct {
	struct list_head	list;

	struct list_head	pending_work;

	struct list_head	workers;
	unsigned		nr_workers;
	unsigned		max_active;
	char			name[24];
};

/* What the kernel uses when a caller passes 0. */
#define WQ_DFL_ACTIVE	256

static unsigned nr_idle_workers(struct workqueue_struct *wq)
{
	struct wq_worker *w;
	unsigned nr = 0;

	list_for_each_entry(w, &wq->workers, list)
		nr += !w->current_work;

	return nr;
}

static bool wq_busy(struct workqueue_struct *wq)
{
	return nr_idle_workers(wq) < wq->nr_workers;
}

/* Queued work that no free worker is left to take. */
static bool work_starved(struct workqueue_struct *wq)
{
	unsigned free = nr_idle_workers(wq), nr = 0;
	struct work_struct *work;

	list_for_each_entry(work, &wq->pending_work, entry)
		if (++nr > free)
			return true;

	return false;
}

static void clear_work_pending(struct work_struct *work)
{
	clear_bit(WORK_PENDING_BIT, work_data_bits(work));
}

static bool set_work_pending(struct work_struct *work)
{
	return !test_and_set_bit(WORK_PENDING_BIT, work_data_bits(work));
}

/*
 * The rule: a given work item is never running more than once at a time.
 *
 * The pending list cannot answer this on its own - queue_work() clears the
 * pending bit before the work function is called, so an item that re-queues
 * itself (or is re-queued by someone else) while running is both pending and
 * running at once. Which worker holds it is the only reliable answer.
 */
static bool work_running(struct work_struct *work)
{
	struct workqueue_struct *wq;

	list_for_each_entry(wq, &wq_list, list) {
		struct wq_worker *w;

		list_for_each_entry(w, &wq->workers, list)
			if (w->current_work == work)
				return true;
	}

	return false;
}

/*
 * The oldest queued item nobody is already running - see the non-reentrancy
 * note above. Skipping past a busy item rather than waiting behind it is what
 * the kernel does too, and it is the whole point of having more than one
 * worker: an item that blocks must not hold up unrelated ones.
 */
static struct work_struct *find_runnable_work(struct workqueue_struct *wq)
{
	struct work_struct *work;

	list_for_each_entry(work, &wq->pending_work, entry)
		if (!work_running(work))
			return work;

	return NULL;
}

/*
 * Which idle worker takes which item is settled by who gets wq_lock, and the
 * losers go straight back to sleep. A worker that is running something is
 * deliberately left alone: it re-checks the pending list under wq_lock before
 * it sleeps, so waking it would only cost a futex call.
 */
static void wake_idle_workers(struct workqueue_struct *wq)
{
	struct wq_worker *w;

	list_for_each_entry(w, &wq->workers, list)
		if (!w->current_work)
			wake_up_process(w->task);
}

static int worker_thread(void *arg)
{
	struct wq_worker *worker = arg;
	struct workqueue_struct *wq = worker->wq;
	struct work_struct *work;

	pthread_mutex_lock(&wq_lock);
	while (1) {
		set_current_state(TASK_INTERRUPTIBLE);

		if (kthread_should_stop()) {
			BUG_ON(!list_empty(&wq->pending_work));
			break;
		}

		work = find_runnable_work(wq);
		if (!work) {
			pthread_mutex_unlock(&wq_lock);
			schedule();
			pthread_mutex_lock(&wq_lock);
			continue;
		}
		__set_current_state(TASK_RUNNING);

		BUG_ON(!work_pending(work));
		list_del_init(&work->entry);
		clear_work_pending(work);
		worker->current_work = work;

		pthread_mutex_unlock(&wq_lock);
		work->func(work);
		pthread_mutex_lock(&wq_lock);

		/*
		 * Before the broadcast, not after the next loop iteration
		 * overwrites it: a flush_work() waiter woken here would
		 * otherwise still see its own work as running, wait again, and
		 * never be signalled - the clear did not broadcast.
		 */
		worker->current_work = NULL;
		pthread_cond_broadcast(&work_finished);

		/*
		 * Anything still queued may have been passed over by a worker
		 * that then slept - including because it was this very item,
		 * skipped for non-reentrancy.
		 */
		if (!list_empty(&wq->pending_work))
			wake_idle_workers(wq);
	}
	pthread_mutex_unlock(&wq_lock);

	return 0;
}

static void __queue_work(struct workqueue_struct *wq,
			 struct work_struct *work)
{
	BUG_ON(!work_pending(work));
	BUG_ON(!list_empty(&work->entry));

	struct wq_worker *w;

	list_add_tail(&work->entry, &wq->pending_work);

	/*
	 * Nobody free to take it, and room to grow: another worker. The first
	 * item on a fresh queue takes this path, so a queue nothing blocks on
	 * settles at exactly one thread.
	 *
	 * Counting idle workers by walking them, rather than keeping a counter
	 * maintained on the sleep path, is the difference between this working
	 * and not: a worker that has been woken but has not yet reacquired
	 * wq_lock still looks idle by a counter, and the item it is about to
	 * take is the one that would have justified the new worker.
	 */
	if (work_starved(wq) && wq->nr_workers < wq->max_active) {
		w = kzalloc(sizeof(*w), GFP_KERNEL);
		if (!w)
			die("error allocating workqueue worker\n");

		w->wq = wq;
		w->task = kthread_run(worker_thread, w, "%s/%u",
				      wq->name, wq->nr_workers);

		int ret = PTR_ERR_OR_ZERO(w->task);
		if (ret)
			die("error creating workqueue thread: %s\n", errname(ret));

		list_add_tail(&w->list, &wq->workers);
		wq->nr_workers++;
	}

	wake_idle_workers(wq);
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

bool flush_work(struct work_struct *work)
{
	bool ret = false;

	pthread_mutex_lock(&wq_lock);
	while (work_pending(work) || work_running(work)) {
		pthread_cond_wait(&work_finished, &wq_lock);
		ret = true;
	}
	pthread_mutex_unlock(&wq_lock);

	return ret;
}

static bool __flush_work(struct work_struct *work)
{
	bool ret = false;

	while (work_running(work)) {
		pthread_cond_wait(&work_finished, &wq_lock);
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
	while (!list_empty(&wq->pending_work) || wq_busy(wq))
		pthread_cond_wait(&work_finished, &wq_lock);
	pthread_mutex_unlock(&wq_lock);
}

void destroy_workqueue(struct workqueue_struct *wq)
{
	struct wq_worker *w, *n;

	/*
	 * kthread_stop() waits for the worker, and the worker needs wq_lock to
	 * get out of its loop - so stop them all without it. Freeing them is
	 * the part that needs care: work_running() walks every queue's worker
	 * list from other threads, so nothing may be freed until this queue is
	 * off wq_list and unreachable.
	 */
	list_for_each_entry(w, &wq->workers, list)
		kthread_stop(w->task);

	pthread_mutex_lock(&wq_lock);
	list_del(&wq->list);
	pthread_mutex_unlock(&wq_lock);

	list_for_each_entry_safe(w, n, &wq->workers, list)
		kfree(w);

	kfree(wq);
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

	INIT_LIST_HEAD(&wq->list);
	INIT_LIST_HEAD(&wq->pending_work);
	INIT_LIST_HEAD(&wq->workers);

	va_start(args, max_active);
	vsnprintf(wq->name, sizeof(wq->name), fmt, args);
	va_end(args);

	/*
	 * Whatever the caller asked for. Capping it lower looks harmless -
	 * threads are not free and 512 is a lot - but max_active is the bound
	 * on how many of this queue's items may be blocked at once, so a queue
	 * whose callers can block more items than we allow workers deadlocks
	 * exactly like the one worker per queue this replaced. It costs
	 * nothing to honour: workers are created only when queued work has no
	 * free worker to take it, so a queue nothing blocks on still settles
	 * at one thread no matter what this says.
	 */
	wq->max_active = max_active > 0 ? max_active : WQ_DFL_ACTIVE;

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
