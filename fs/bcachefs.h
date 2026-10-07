/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_H
#define _BCACHEFS_H

#include "types.h"

#define bch2_fs_init_fault(name)					\
	dynamic_fault("bcachefs:bch_fs_init:" name)
#define bch2_meta_read_fault(name)					\
	 dynamic_fault("bcachefs:meta:read:" name)
#define bch2_meta_write_fault(name)					\
	 dynamic_fault("bcachefs:meta:write:" name)

#ifdef __KERNEL__
#define BCACHEFS_LOG_PREFIX
#endif

#ifdef BCACHEFS_LOG_PREFIX

#define bch2_log_msg(_c, fmt)			"bcachefs (%s): " fmt, bch2_fs_name(_c)
#define bch2_fmt_dev(_ca, fmt)			"bcachefs (%s): " fmt "\n", bch2_dev_name(_ca)
#define bch2_fmt_dev_offset(_ca, _offset, fmt)	"bcachefs (%s sector %llu): " fmt "\n", ((_ca)->name), (_offset)
#define bch2_fmt_inum(_c, _inum, fmt)		"bcachefs (%s inum %llu): " fmt "\n", ((_c)->name), (_inum)
#define bch2_fmt_inum_offset(_c, _inum, _offset, fmt)			\
	 "bcachefs (%s inum %llu offset %llu): " fmt "\n", ((_c)->name), (_inum), (_offset)

#else

#define bch2_log_msg(_c, fmt)			fmt
#define bch2_fmt_dev(_ca, fmt)			"%s: " fmt "\n", ((_ca)->name)
#define bch2_fmt_dev_offset(_ca, _offset, fmt)	"%s sector %llu: " fmt "\n", ((_ca)->name), (_offset)
#define bch2_fmt_inum(_c, _inum, fmt)		"inum %llu: " fmt "\n", (_inum)
#define bch2_fmt_inum_offset(_c, _inum, _offset, fmt)				\
	 "inum %llu offset %llu: " fmt "\n", (_inum), (_offset)

#endif

#define bch2_fmt(_c, fmt)		bch2_log_msg(_c, fmt "\n")

void bch2_print_str_loglevel(struct bch_fs *, int, const char *);
void bch2_print_str(struct bch_fs *, const char *, const char *);

/* For a person rather than a log - see bch_fs.stdio_user_only. */
void bch2_print_str_user(struct bch_fs *, const char *);

__printf(2, 3)
void bch2_print_opts(struct bch_opts *, const char *, ...);

__printf(2, 3)
void __bch2_print(struct bch_fs *c, const char *fmt, ...);

void bch2_ratelimit_state_init(struct ratelimit_state *);
bool bch2_ratelimit_suppress(struct bch_fs *, struct ratelimit_state *, const char *);
void bch2_rust_warn(const char *, unsigned);


#define maybe_dev_to_fs(_c)	_Generic((_c),				\
	struct bch_dev *:	((struct bch_dev *) (_c))->fs,		\
	struct bch_fs *:	(_c))

#define bch2_print(_c, ...) __bch2_print(maybe_dev_to_fs(_c), __VA_ARGS__)

#define __bch2_ratelimit(_c, _rs)					\
	((_c)->opts.ratelimit_errors && !__ratelimit(_rs))

#define bch2_ratelimit(_c)						\
({									\
	static DEFINE_RATELIMIT_STATE(rs,				\
				      DEFAULT_RATELIMIT_INTERVAL,	\
				      DEFAULT_RATELIMIT_BURST);		\
									\
	__bch2_ratelimit(_c, &rs);					\
})

#define bch2_print_ratelimited(_c, ...)					\
do {									\
	if (!bch2_ratelimit(_c))					\
		bch2_print(_c, __VA_ARGS__);				\
} while (0)

#define bch_log(c, loglevel, fmt, ...) \
	bch2_print(c, loglevel bch2_fmt(c, fmt), ##__VA_ARGS__)
#define bch_log_ratelimited(c, loglevel, fmt, ...) \
	bch2_print_ratelimited(c, loglevel bch2_fmt(c, fmt), ##__VA_ARGS__)

#define bch_err(c, ...)			bch_log(c, KERN_ERR, __VA_ARGS__)
#define bch_err_ratelimited(c, ...)	bch_log_ratelimited(c, KERN_ERR, __VA_ARGS__)
#define bch_warn(c, ...)		bch_log(c, KERN_WARNING, __VA_ARGS__)
#define bch_warn_ratelimited(c, ...)	bch_log_ratelimited(c, KERN_WARNING, __VA_ARGS__)
#define bch_notice(c, ...)		bch_log(c, KERN_NOTICE, __VA_ARGS__)
#define bch_info(c, ...)		bch_log(c, KERN_INFO, __VA_ARGS__)
#define bch_info_ratelimited(c, ...)	bch_log_ratelimited(c, KERN_INFO, __VA_ARGS__)
#define bch_verbose(c, ...)		bch_log(c, KERN_DEBUG, __VA_ARGS__)
#define bch_verbose_ratelimited(c, ...)	bch_log_ratelimited(c, KERN_DEBUG, __VA_ARGS__)

#define bch_dev_log(ca, loglevel, fmt, ...) \
	bch2_print(ca->fs, loglevel bch2_fmt_dev(ca, fmt), ##__VA_ARGS__)

#define bch_err_dev(ca, ...)		bch_dev_log(ca, KERN_ERR, __VA_ARGS__)
#define bch_notice_dev(ca, ...)		bch_dev_log(ca, KERN_NOTICE, __VA_ARGS__)
#define bch_info_dev(ca, ...)		bch_dev_log(ca, KERN_INFO, __VA_ARGS__)
#define bch_verbose_dev(ca, ...)	bch_dev_log(ca, KERN_DEBUG, __VA_ARGS__)

#define bch_err_dev_ratelimited(ca, ...)				\
do {									\
	if (!bch2_ratelimit(ca->fs))					\
		bch_err_dev(ca, __VA_ARGS__);				\
} while (0)

static inline bool should_print_err(int err)
{
	return err && !bch2_err_matches(err, BCH_ERR_transaction_restart);
}

#define bch_err_fn(_c, _ret)						\
do {									\
	if (should_print_err(_ret))					\
		bch_err(_c, "%s(): error %s", __func__, bch2_err_str(_ret));\
} while (0)

#define bch_err_fn_ratelimited(_c, _ret)				\
do {									\
	if (should_print_err(_ret))					\
		bch_err_ratelimited(_c, "%s(): error %s", __func__, bch2_err_str(_ret));\
} while (0)

#define bch_err_msg(_c, _ret, _msg, ...)				\
do {									\
	if (should_print_err(_ret))					\
		bch_err(_c, "%s(): error " _msg " %s", __func__,	\
			##__VA_ARGS__, bch2_err_str(_ret));		\
} while (0)

#define bch_err_fn_dev(_ca, _ret)					\
do {									\
	if (should_print_err(_ret))					\
		bch_err_dev(_ca, "%s(): error %s", __func__, bch2_err_str(_ret));\
} while (0)

#define bch_err_msg_dev(_ca, _ret, _msg, ...)				\
do {									\
	if (should_print_err(_ret))					\
		bch_err_dev(_ca, "%s(): error " _msg " %s", __func__,	\
			##__VA_ARGS__, bch2_err_str(_ret));		\
} while (0)

/* Error tracking: */

int __bch2_err_throw(struct bch_fs *, int);

#define bch_err_throw(_c, _err) __bch2_err_throw(_c, -BCH_ERR_##_err)

/*
 * Have we been told to stop? For long-running kthread work, so the check can be
 * try()d where it belongs instead of open coded:
 *
 *	try(bch2_kthread_cancelled(c));
 *
 * Returns 0 outside a kthread, so paths shared with user context are unaffected.
 */
static inline int bch2_kthread_cancelled(struct bch_fs *c)
{
	if ((current->flags & PF_KTHREAD) && kthread_should_stop())
		return bch_err_throw(c, kthread_cancelled);

	return 0;
}

/* Read-only refs: */

static inline bool bch2_ro_ref_tryget(struct bch_fs *c)
{
	if (test_bit(BCH_FS_stopping, &c->flags))
		return false;

	return refcount_inc_not_zero(&c->ro_ref);
}

static inline void bch2_ro_ref_put(struct bch_fs *c)
{
	if (c && refcount_dec_and_test(&c->ro_ref))
		wake_up(&c->ro_ref_wait);
}

/* Unit conversions: */

static inline unsigned bucket_bytes(const struct bch_dev *ca)
{
	return ca->mi.bucket_size << 9;
}

static inline unsigned block_bytes(const struct bch_fs *c)
{
	return c->opts.block_size;
}

static inline unsigned block_sectors(const struct bch_fs *c)
{
	return c->opts.block_size >> 9;
}

/* Time conversion: */

static inline struct timespec64 bch2_time_to_timespec(const struct bch_fs *c, s64 time)
{
	struct timespec64 t;
	s64 sec;
	s32 rem;

	time += c->sb.time_base_lo;

	sec = div_s64_rem(time, c->sb.time_units_per_sec, &rem);

	set_normalized_timespec64(&t, sec, rem * (s64)c->sb.nsec_per_time_unit);

	return t;
}

static inline s64 timespec_to_bch2_time(const struct bch_fs *c, struct timespec64 ts)
{
	return (ts.tv_sec * c->sb.time_units_per_sec +
		(int) ts.tv_nsec / c->sb.nsec_per_time_unit) - c->sb.time_base_lo;
}

static inline s64 bch2_current_time(const struct bch_fs *c)
{
	struct timespec64 now;

	ktime_get_coarse_real_ts64(&now);
	return timespec_to_bch2_time(c, now);
}

static inline u64 bch2_current_io_time(const struct bch_fs *c, int rw)
{
	return max(1ULL, (u64) atomic64_read(&c->io_clock[rw].now) & LRU_TIME_MAX);
}

/* Filesystem and device helpers: */

static inline void bch2_set_ra_pages(struct bch_fs *c, unsigned ra_pages)
{
#ifndef NO_BCACHEFS_FS
	if (c->vfs_sb)
		c->vfs_sb->s_bdi->ra_pages = ra_pages;
#endif
}

static inline struct stdio_redirect *bch2_fs_stdio_redirect_user(struct bch_fs *c)
{
	struct stdio_redirect *stdio = c->stdio;

	if (c->stdio_filter && c->stdio_filter != current)
		stdio = NULL;
	return stdio;
}

static inline struct stdio_redirect *bch2_fs_stdio_redirect_log(struct bch_fs *c)
{
	return c->stdio_user_only ? NULL : bch2_fs_stdio_redirect_user(c);
}

#define BKEY_PADDED_ONSTACK(key, pad)				\
	struct { struct bkey_i key; __u64 key ## _pad[pad]; }

/*
 * This is needed because discard is both a filesystem option and a device
 * option, and mount options are supposed to apply to that mount and not be
 * persisted, i.e. if it's set as a mount option we can't propagate it to the
 * device.
 */
static inline bool bch2_discard_opt_enabled(struct bch_fs *c, struct bch_dev *ca)
{
	return test_bit(BCH_FS_discard_mount_opt_set, &c->flags)
		? c->opts.discard
		: ca->mi.discard;
}

static inline int bch2_fs_casefold_enabled(struct bch_fs *c)
{
	if (!IS_ENABLED(CONFIG_UNICODE))
		return bch_err_throw(c, no_casefolding_without_utf8);
	if (c->opts.casefold_disabled)
		return bch_err_throw(c, casefolding_disabled);
	return 0;
}

static inline const char *strip_bch2(const char *msg)
{
	if (!strncmp("bch2_", msg, 5))
		return msg + 5;
	return msg;
}

static inline const char *bch2_fs_name(const struct bch_fs *c)
{
	return c->name;
}

static inline const char *bch2_dev_name(const struct bch_dev *ca)
{
	return ca->name;
}

static inline bool bch2_dev_rotational(struct bch_fs *c, unsigned dev)
{
	return dev != BCH_SB_MEMBER_INVALID && test_bit(dev, c->devs_rotational.d);
}

/* Log messages: */

void __bch2_log_msg_start(const char *, struct printbuf *);

static inline void bch2_log_msg_start(struct bch_fs *c, struct printbuf *out)
{
	__bch2_log_msg_start(c->name, out);
}

#include "bcachefs_types.h"

static inline void bch2_log_msg_exit(struct bch_log_msg *msg)
{
	if (!msg->m.suppress) {
		/* elastic tabstops: align any raw \t/\r columns */
		bch2_printbuf_tabstop_align(&msg->m);
		bch2_print_str_loglevel(msg->c, msg->loglevel, msg->m.buf);
	}
	printbuf_exit(&msg->m);
}

static inline struct bch_log_msg bch2_log_msg_init(struct bch_fs *c,
						   unsigned loglevel,
						   bool suppress,
						   bool atomic)
{
	struct printbuf buf = PRINTBUF;
	buf.atomic = atomic;
	buf.suppress = suppress;
	bch2_log_msg_start(c, &buf);
	return (struct bch_log_msg) {
		.c		= c,
		.loglevel	= loglevel,
		.m		= buf,
	};
}

DEFINE_CLASS(bch_log_msg, struct bch_log_msg,
	     bch2_log_msg_exit(&_T),
	     bch2_log_msg_init(c, LOGLEVEL_err, false, false),
	     struct bch_fs *c)

EXTEND_CLASS(bch_log_msg, _level,
	     bch2_log_msg_init(c, loglevel, false, false),
	     struct bch_fs *c, unsigned loglevel)

EXTEND_CLASS(bch_log_msg, _atomic,
	     bch2_log_msg_init(c, LOGLEVEL_err, false, true),
	     struct bch_fs *c)

/*
 * Open coded EXTEND_CLASS, because we need the constructor to be a macro for
 * ratelimiting to work correctly
 */

typedef class_bch_log_msg_t class_bch_log_msg_ratelimited_t;

static inline void class_bch_log_msg_ratelimited_destructor(class_bch_log_msg_t *p)
{ bch2_log_msg_exit(p); }

/* btrees_clean: see bch_sb_field_ext.btrees_clean and bch2_set/clear_btree_clean() */
static inline bool bch2_btree_is_clean(struct bch_fs *c, enum btree_id btree)
{
	return c->sb.btrees_clean & BIT_ULL(btree);
}
#define class_bch_log_msg_ratelimited_constructor(_c)		\
	bch2_log_msg_init(_c, 3, bch2_ratelimit(_c), false)

#endif /* _BCACHEFS_TYPES_H */
