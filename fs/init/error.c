// SPDX-License-Identifier: GPL-2.0
/*
 * The printf-style entry points of init/error.rs: the message formatted here,
 * a variadic not being Rust's to define - everything else Rust's.
 */

#include "bcachefs.h"

#include "init/error.h"

bool bch2_fs_inconsistent(struct bch_fs *c, const char *fmt, ...)
{
	CLASS(printbuf, buf)();
	guard(printbuf_atomic)(&buf);

	va_list args;
	va_start(args, fmt);
	prt_vprintf(&buf, fmt, args);
	va_end(args);

	return bch2_fs_trans_inconsistent_str(c, NULL, &buf);
}

bool bch2_trans_inconsistent(struct btree_trans *trans, const char *fmt, ...)
{
	CLASS(printbuf, buf)();
	guard(printbuf_atomic)(&buf);

	va_list args;
	va_start(args, fmt);
	prt_vprintf(&buf, fmt, args);
	va_end(args);

	return bch2_fs_trans_inconsistent_str(trans->c, trans, &buf);
}

int bch2_fs_topology_error(struct bch_fs *c, const char *fmt, ...)
{
	CLASS(bch_log_msg, msg)(c);

	va_list args;
	va_start(args, fmt);
	prt_vprintf(&msg.m, fmt, args);
	va_end(args);

	return __bch2_topology_error(c, &msg.m);
}

void bch2_fatal_error(struct bch_fs *c, const char *func, const char *fmt, ...)
{
	CLASS(printbuf, buf)();

	va_list args;
	va_start(args, fmt);
	prt_vprintf(&buf, fmt, args);
	va_end(args);

	bch2_fatal_error_str(c, func, &buf);
}

int __bch2_fsck_err(struct bch_fs *c,
		  struct btree_trans *trans,
		  struct bpos pos,
		  enum bch_fsck_flags flags,
		  enum bch_sb_error_id err,
		  const char *fmt, ...)
{
	CLASS(printbuf, buf)();

	va_list args;
	va_start(args, fmt);
	prt_vprintf(&buf, fmt, args);
	va_end(args);

	return bch2_fsck_err_str(c, trans, pos, flags, err, &buf);
}

int __bch2_bkey_fsck_err(struct bch_fs *c,
			 struct bkey_s_c k,
			 const struct bkey_validate_context *from,
			 enum bch_sb_error_id err,
			 const char *fmt, ...)
{
	CLASS(printbuf, buf)();

	va_list args;
	va_start(args, fmt);
	prt_vprintf(&buf, fmt, args);
	va_end(args);

	return bch2_bkey_fsck_err_str(c, k, from, err, &buf);
}
