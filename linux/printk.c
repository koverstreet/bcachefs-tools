#include <stdarg.h>
#include <stdio.h>

#include <linux/sched.h>

#include "util/printbuf.h"
#include "util/util.h"

static inline const char *real_fmt(const char *fmt)
{
	return fmt[0] == '\001' ? fmt + 2 : fmt;
}

void vprintk(const char *fmt, va_list args)
{
	vprintf(real_fmt(fmt), args);
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
