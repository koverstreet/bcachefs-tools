/*
 * Generic waiting primitives.
 *
 * (C) 2004 Nadia Yvette Chambers, Oracle
 */

#include <linux/completion.h>
#include <linux/sched.h>
#include <linux/wait.h>
#include <linux/wait_bit.h>

static inline int waitqueue_active(wait_queue_head_t *q)
{
	return !list_empty(&q->task_list);
}

static inline void __add_wait_queue(wait_queue_head_t *head, wait_queue_t *new)
{
	list_add(&new->task_list, &head->task_list);
}

static inline void __add_wait_queue_tail(wait_queue_head_t *head,
					 wait_queue_t *new)
{
	list_add_tail(&new->task_list, &head->task_list);
}

static inline void
__add_wait_queue_tail_exclusive(wait_queue_head_t *q, wait_queue_t *wait)
{
	wait->flags |= WQ_FLAG_EXCLUSIVE;
	__add_wait_queue_tail(q, wait);
}

static inline void
__remove_wait_queue(wait_queue_head_t *head, wait_queue_t *old)
{
	list_del(&old->task_list);
}

static void __wake_up_common(wait_queue_head_t *q, unsigned int mode,
			     int nr_exclusive, int wake_flags, void *key)
{
	wait_queue_t *curr, *next;

	list_for_each_entry_safe(curr, next, &q->task_list, task_list) {
		unsigned flags = curr->flags;

		if (curr->func(curr, mode, wake_flags, key) &&
				(flags & WQ_FLAG_EXCLUSIVE) && !--nr_exclusive)
			break;
	}
}

static void __wake_up(wait_queue_head_t *q, unsigned int mode,
		      int nr_exclusive, void *key)
{
	unsigned long flags;

	spin_lock_irqsave(&q->lock, flags);
	__wake_up_common(q, mode, nr_exclusive, 0, key);
	spin_unlock_irqrestore(&q->lock, flags);
}

void wake_up(wait_queue_head_t *q)
{
	__wake_up(q, TASK_NORMAL, 1, NULL);
}

void wake_up_all(wait_queue_head_t *q)
{
	__wake_up(q, TASK_NORMAL, 0, NULL);
}

static void __wake_up_locked(wait_queue_head_t *q, unsigned int mode, int nr)
{
	__wake_up_common(q, mode, nr, 0, NULL);
}

void
prepare_to_wait(wait_queue_head_t *q, wait_queue_t *wait, int state)
{
	unsigned long flags;

	wait->flags &= ~WQ_FLAG_EXCLUSIVE;
	spin_lock_irqsave(&q->lock, flags);
	if (list_empty(&wait->task_list))
		__add_wait_queue(q, wait);
	set_current_state(state);
	spin_unlock_irqrestore(&q->lock, flags);
}

static void
prepare_to_wait_exclusive(wait_queue_head_t *q, wait_queue_t *wait, int state)
{
	unsigned long flags;

	wait->flags |= WQ_FLAG_EXCLUSIVE;
	spin_lock_irqsave(&q->lock, flags);
	if (list_empty(&wait->task_list))
		__add_wait_queue_tail(q, wait);
	set_current_state(state);
	spin_unlock_irqrestore(&q->lock, flags);
}

void finish_wait(wait_queue_head_t *q, wait_queue_t *wait)
{
	unsigned long flags;

	__set_current_state(TASK_RUNNING);
	/*
	 * We can check for list emptiness outside the lock
	 * IFF:
	 *  - we use the "careful" check that verifies both
	 *    the next and prev pointers, so that there cannot
	 *    be any half-pending updates in progress on other
	 *    CPU's that we haven't seen yet (and that might
	 *    still change the stack area.
	 * and
	 *  - all other users take the lock (ie we can only
	 *    have _one_ other CPU that looks at or modifies
	 *    the list).
	 */
	if (!list_empty_careful(&wait->task_list)) {
		spin_lock_irqsave(&q->lock, flags);
		list_del_init(&wait->task_list);
		spin_unlock_irqrestore(&q->lock, flags);
	}
}

int default_wake_function(wait_queue_t *curr, unsigned mode, int wake_flags,
			  void *key)
{
	return wake_up_process(curr->private);
}

int autoremove_wake_function(wait_queue_t *wait, unsigned mode, int sync, void *key)
{
	int ret = default_wake_function(wait, mode, sync, key);

	if (ret)
		list_del_init(&wait->task_list);
	return ret;
}

struct wait_bit_queue {
	struct wait_bit_key	key;
	wait_queue_t		wait;
};

static int wake_bit_function(wait_queue_t *wait, unsigned mode, int sync, void *arg)
{
	struct wait_bit_key *key = arg;
	struct wait_bit_queue *wait_bit =
		container_of(wait, struct wait_bit_queue, wait);

	return (wait_bit->key.flags == key->flags &&
		wait_bit->key.bit_nr == key->bit_nr &&
		!test_bit(key->bit_nr, key->flags))
		? autoremove_wake_function(wait, mode, sync, key) : 0;
}

static DECLARE_WAIT_QUEUE_HEAD(bit_wq);

#define __WAIT_BIT_KEY_INITIALIZER(word, bit)				\
	{ .flags = word, .bit_nr = bit, }

#define DEFINE_WAIT_BIT(name, word, bit)				\
	struct wait_bit_queue name = {					\
		.key = __WAIT_BIT_KEY_INITIALIZER(word, bit),		\
		.wait	= {						\
			.private	= current,			\
			.func		= wake_bit_function,		\
			.task_list	=				\
				LIST_HEAD_INIT((name).wait.task_list),	\
		},							\
	}

void wake_up_bit(void *word, int bit)
{
	struct wait_bit_key key = __WAIT_BIT_KEY_INITIALIZER(word, bit);

	if (waitqueue_active(&bit_wq))
		__wake_up(&bit_wq, TASK_NORMAL, 1, &key);
}

void __wait_on_bit(void *word, int bit, unsigned mode)
{
	DEFINE_WAIT_BIT(wait, word, bit);

	do {
		prepare_to_wait(&bit_wq, &wait.wait, mode);
		if (test_bit(wait.key.bit_nr, wait.key.flags))
			schedule();
	} while (test_bit(wait.key.bit_nr, wait.key.flags));

	finish_wait(&bit_wq, &wait.wait);
}

int out_of_line_wait_on_bit_timeout(unsigned long *word, int bit,
				    wait_bit_action_f *action,
				    unsigned mode, unsigned long timeout)
{
	DEFINE_WAIT_BIT(wait, word, bit);
	int ret = 0;

	wait.key.timeout = jiffies + timeout;

	do {
		prepare_to_wait(&bit_wq, &wait.wait, mode);
		if (test_bit(wait.key.bit_nr, wait.key.flags))
			ret = action(&wait.key, mode);
	} while (test_bit(wait.key.bit_nr, wait.key.flags) && !ret);

	finish_wait(&bit_wq, &wait.wait);
	return ret;
}

void __wait_on_bit_lock(void *word, int bit, unsigned mode)
{
	DEFINE_WAIT_BIT(wait, word, bit);

	do {
		prepare_to_wait_exclusive(&bit_wq, &wait.wait, mode);
		if (!test_bit(wait.key.bit_nr, wait.key.flags))
			continue;
		schedule();
	} while (test_and_set_bit(wait.key.bit_nr, wait.key.flags));
	finish_wait(&bit_wq, &wait.wait);
}

void complete(struct completion *x)
{
	unsigned long flags;

	spin_lock_irqsave(&x->wait.lock, flags);
	x->done++;
	__wake_up_locked(&x->wait, TASK_NORMAL, 1);
	spin_unlock_irqrestore(&x->wait.lock, flags);
}

void wait_for_completion(struct completion *x)
{
	spin_lock_irq(&x->wait.lock);

	if (!x->done) {
		DECLARE_WAITQUEUE(wait, current);

		__add_wait_queue_tail_exclusive(&x->wait, &wait);
		do {
			__set_current_state(TASK_UNINTERRUPTIBLE);
			spin_unlock_irq(&x->wait.lock);

			schedule();
			spin_lock_irq(&x->wait.lock);
		} while (!x->done);
		__remove_wait_queue(&x->wait, &wait);
		if (!x->done)
			goto out;
	}
	x->done--;
out:
	spin_unlock_irq(&x->wait.lock);
}

unsigned long wait_for_completion_timeout(struct completion *x, unsigned long timeout)
{
	spin_lock_irq(&x->wait.lock);

	if (!x->done) {
		DECLARE_WAITQUEUE(wait, current);

		__add_wait_queue_tail_exclusive(&x->wait, &wait);
		do {
			__set_current_state(TASK_UNINTERRUPTIBLE);
			spin_unlock_irq(&x->wait.lock);

			timeout = schedule_timeout(timeout);
			spin_lock_irq(&x->wait.lock);
		} while (!x->done);
		__remove_wait_queue(&x->wait, &wait);
		if (!x->done)
			goto out;
	}
	x->done--;
out:
	spin_unlock_irq(&x->wait.lock);
	return timeout;
}
