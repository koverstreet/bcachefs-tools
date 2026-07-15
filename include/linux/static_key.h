/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _LINUX_STATIC_KEY_H
#define _LINUX_STATIC_KEY_H

struct static_key {
	int v;
};

static inline void static_key_enable(struct static_key *key) {}
static inline void static_key_disable(struct static_key *key) {}
static inline bool static_key_enabled(struct static_key *key) { return false; }

struct static_key_false {
	struct static_key	key;
};

#define DEFINE_STATIC_KEY_FALSE(n)	struct static_key_false n = {}

#define static_branch_unlikely(x)	unlikely((x)->key.v)
#define static_branch_likely(x)		likely((x)->key.v)

#endif /* _LINUX_STATIC_KEY_H */
