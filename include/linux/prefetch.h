#ifndef _LINUX_PREFETCH_H
#define _LINUX_PREFETCH_H

#define prefetch(p)	\
	({ __maybe_unused typeof(p) __var = (p); })

#define prefetchw(p)	\
	({ __maybe_unused typeof(p) __var = (p); })

#endif /* _LINUX_PREFETCH_H */
