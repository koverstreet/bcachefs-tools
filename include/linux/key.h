#ifndef _LINUX_KEY_H
#define _LINUX_KEY_H

#include <linux/types.h>
#include <linux/atomic.h>
#include <keyutils.h>

struct user_key_payload {
	size_t		datalen;	/* length of this data */
	char		data[0];	/* actual data */
};

struct key {
	atomic_t		usage;		/* number of references */
	key_serial_t		serial;		/* key serial number */
	struct rw_semaphore	sem;		/* change vs change sem */
	struct user_key_payload	payload;
};

static inline const struct user_key_payload *user_key_payload(const struct key *key)
{
	return &key->payload;
}

static inline void key_put(struct key *key)
{
	if (atomic_dec_and_test(&key->usage))
		free(key);
}

static inline struct key *__key_get(struct key *key)
{
	atomic_inc(&key->usage);
	return key;
}

static inline struct key *key_get(struct key *key)
{
	return key ? __key_get(key) : key;
}

#endif /* _LINUX_KEY_H */
