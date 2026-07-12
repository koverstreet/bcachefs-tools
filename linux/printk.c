#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

#include <linux/sched.h>

#include "util/printbuf.h"
#include "util/util.h"

/*
 * The kernel's printk parses the KERN_SOH loglevel prefix from the
 * formatted output, so callers may pass it as a %s argument rather than
 * in the format string (bch2_print_string_as_lines(KERN_ERR, ...) does) -
 * format first, then strip, or the SOH and loglevel digit leak into the
 * output.
 */
static inline const char *skip_loglevel(const char *s)
{
	while (s[0] == '\001' && s[1])
		s += 2;
	return s;
}

void vprintk(const char *fmt, va_list args)
{
	char *buf = NULL;

	if (vasprintf(&buf, skip_loglevel(fmt), args) < 0)
		return;

	/*
	 * Filesystem log messages, not command output: stderr, so commands
	 * whose stdout is data (bcachefs list) aren't polluted by
	 * recovery-pass chatter:
	 */
	fputs(skip_loglevel(buf), stderr);
	free(buf);
}

void printk(const char *fmt, ...)
{
	va_list args;
	va_start(args, fmt);
	vprintk(fmt, args);
	va_end(args);
}

void dump_stack(void)
{
	struct printbuf buf = PRINTBUF;
	bch2_prt_task_backtrace(&buf, current, 1, GFP_KERNEL);
	fputs(buf.buf ?: "", stderr);
	printbuf_exit(&buf);
}
