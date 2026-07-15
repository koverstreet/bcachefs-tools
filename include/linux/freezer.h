#ifndef __TOOLS_LINUX_FREEZER_H
#define __TOOLS_LINUX_FREEZER_H

#define try_to_freeze()
#define set_freezable()
#define freezing(task)		false
#define freezable_schedule()	schedule()
#define freezable_schedule_timeout(_t) schedule_timeout(_t)

static inline void __refrigerator(bool f) {}

#endif /* __TOOLS_LINUX_FREEZER_H */
