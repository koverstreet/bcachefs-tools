#ifndef _LINUX_ERRNAME_H
#define _LINUX_ERRNAME_H

#include <string.h>

static inline const char *errname(int err)
{
	return strerror(abs(err));
}

#endif /* _LINUX_ERRNAME_H */
