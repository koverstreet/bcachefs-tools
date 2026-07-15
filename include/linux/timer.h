#ifndef __TOOLS_LINUX_TIMER_H
#define __TOOLS_LINUX_TIMER_H

#include <string.h>
#include <linux/types.h>

struct timer_list {
	unsigned long		expires;
	void			(*function)(struct timer_list *timer);
	bool			pending;
};

static inline void timer_setup(struct timer_list *timer,
			       void (*func)(struct timer_list *),
			       unsigned int flags)
{
	memset(timer, 0, sizeof(*timer));
	timer->function = func;
}

#define timer_setup_on_stack(timer, callback, flags)			\
	timer_setup(timer, callback, flags)

#define destroy_timer_on_stack(timer) do {} while (0)

static inline int timer_pending(const struct timer_list *timer)
{
	return timer->pending;
}

int del_timer(struct timer_list * timer);
int timer_delete_sync(struct timer_list *timer);

#define del_singleshot_timer_sync(timer) del_timer_sync(timer)

int mod_timer(struct timer_list *timer, unsigned long expires);

static inline void add_timer(struct timer_list *timer)
{
	BUG_ON(timer_pending(timer));
	mod_timer(timer, timer->expires);
}

void flush_timers(void);

#endif /* __TOOLS_LINUX_TIMER_H */
