#ifndef _TOOLS_LINUX_BITOPS_H_
#define _TOOLS_LINUX_BITOPS_H_

static inline unsigned long __fls(unsigned long word)
{
	return (sizeof(word) * 8) - 1 - __builtin_clzl(word);
}

#endif
