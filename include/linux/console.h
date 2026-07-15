#ifndef _LINUX_CONSOLE_H_
#define _LINUX_CONSOLE_H_

#define console_lock()		do {} while (0)
#define console_trylock()	true
#define console_unlock()	do {} while (0)

#endif /* _LINUX_CONSOLE_H */
