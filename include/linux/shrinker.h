#ifndef __TOOLS_LINUX_SHRINKER_H
#define __TOOLS_LINUX_SHRINKER_H

#include <linux/list.h>
#include <linux/types.h>

struct shrink_control {
	gfp_t gfp_mask;
	unsigned long nr_to_scan;
};

#define SHRINK_STOP (~0UL)

struct seq_buf;
struct shrinker {
	unsigned long (*count_objects)(struct shrinker *,
				       struct shrink_control *sc);
	unsigned long (*scan_objects)(struct shrinker *,
				      struct shrink_control *sc);
	void (*to_text)(struct seq_buf *, struct shrinker *);

	int seeks;	/* seeks to recreate an obj */
	long batch;	/* reclaim batch size, 0 = default */
	struct list_head list;
	void	*private_data;
};

void shrinker_free(struct shrinker *);
struct shrinker *shrinker_alloc(unsigned int, const char *, ...);

int shrinker_register(struct shrinker *);

void run_shrinkers(gfp_t gfp_mask, bool);

void linux_shrinkers_init(void);

#endif /* __TOOLS_LINUX_SHRINKER_H */
