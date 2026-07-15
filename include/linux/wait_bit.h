#ifndef _LINUX_WAIT_BIT_H
#define _LINUX_WAIT_BIT_H

#include <linux/wait.h>

struct wait_bit_key {
	void			*flags;
	int			bit_nr;
	unsigned long		timeout;
};

typedef int wait_bit_action_f(struct wait_bit_key *, int);

int out_of_line_wait_on_bit_timeout(unsigned long *word, int bit,
				    wait_bit_action_f *action,
				    unsigned mode, unsigned long timeout);

#endif /* _LINUX_WAIT_BIT_H */
