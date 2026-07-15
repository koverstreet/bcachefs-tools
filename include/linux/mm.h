/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _TOOLS_LINUX_MM_H
#define _TOOLS_LINUX_MM_H

#include <sys/syscall.h>
#include <unistd.h>
#include <linux/bug.h>
#include <linux/types.h>

struct sysinfo {
	long uptime;		/* Seconds since boot */
	unsigned long loads[3];	/* 1, 5, and 15 minute load averages */
	unsigned long totalram;	/* Total usable main memory size */
	unsigned long freeram;	/* Available memory size */
	unsigned long sharedram;	/* Amount of shared memory */
	unsigned long bufferram;	/* Memory used by buffers */
	unsigned long totalswap;	/* Total swap space size */
	unsigned long freeswap;	/* swap space still available */
	__u16 procs;		   	/* Number of current processes */
	__u16 pad;		   	/* Explicit padding for m68k */
	unsigned long totalhigh;	/* Total high memory size */
	unsigned long freehigh;	/* Available high memory size */
	__u32 mem_unit;			/* Memory unit size in bytes */
	/*
	 * Padding to match the kernel's struct sysinfo layout. 8 bytes on
	 * 32-bit, 0 on 64-bit. Without this, syscall(SYS_sysinfo) writes
	 * past the end of the struct and corrupts the stack on 32-bit.
	 */
	char _f[20 - 2 * sizeof(unsigned long) - sizeof(__u32)];
};

static inline void si_meminfo(struct sysinfo *val)
{
	BUG_ON(syscall(SYS_sysinfo, val));
}

extern unsigned long _totalram_pages;
static inline unsigned long totalram_pages(void)
{
	return _totalram_pages;
}

#endif /* _TOOLS_LINUX_MM_H */
