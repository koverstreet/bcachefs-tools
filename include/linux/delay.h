/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __TOOLS_LINUX_DELAY_H
#define __TOOLS_LINUX_DELAY_H

#include <errno.h>
#include <time.h>

/*
 * The kernel's msleep() is "sleep at least this long, and let anything else
 * run"; nanosleep() is the same bargain in userspace. It restarts on EINTR
 * rather than returning early, because no caller of msleep() expects to have
 * slept less than it asked for.
 */
static inline void msleep(unsigned int msecs)
{
	struct timespec ts = {
		.tv_sec		= msecs / 1000,
		.tv_nsec	= (long) (msecs % 1000) * 1000000,
	};

	while (nanosleep(&ts, &ts) && errno == EINTR)
		;
}

#endif /* __TOOLS_LINUX_DELAY_H */
