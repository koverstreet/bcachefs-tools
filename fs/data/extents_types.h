/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_EXTENTS_TYPES_H
#define _BCACHEFS_EXTENTS_TYPES_H

#include "bcachefs_format.h"

struct bch_extent_crc_unpacked {
	u32			compressed_size;
	u32			uncompressed_size;
	u32			live_size;

	u8			csum_type;
	u8			compression_type;

	u16			offset;

	u16			nonce;

	struct bch_csum		csum;
};

/*
 * Read the device, rebuild from the rest of its stripe, or read around it: a
 * rebuild from the stripe's fastest blocks, tried before the device and
 * without checking which block is bad, so a failed one still leaves the full
 * rebuild. Each is tried at most once per device, in an order that depends on
 * the read, so what's been tried is tracked: bch_dev_io_failures.tried
 */
#define BCH_READ_MODES()	\
	x(direct)		\
	x(ec)			\
	x(ec_read_around)

enum bch_read_mode {
#define x(n)	BCH_READ_MODE_##n,
	BCH_READ_MODES()
#undef x
	BCH_READ_MODE_NR
};

struct extent_ptr_decoded {
	bool				has_ec;
	u8				mode;	/* enum bch_read_mode */
	u8				crc_retry_nr;
	struct bch_extent_crc_unpacked	crc;
	struct bch_extent_ptr		ptr;
	struct bch_extent_stripe_ptr	ec;
};

struct bch_dev_io_failures {
	u8			dev;
	u8			tried;		/* reads: BIT(enum bch_read_mode) */
	unsigned		csum_nr:7;
	s16			errcode;	/* direct read, or write */
	s16			ec_errcode;
};

struct bch_io_failures {
	u8				nr;
	struct bch_dev_io_failures	data[BCH_REPLICAS_MAX + 1];

	struct printbuf			ec_msg;
};

#define BCH_READ_FLAGS()		\
	x(retry_if_stale)		\
	x(may_promote)			\
	x(user_mapped)			\
	x(soft_require_read_device)	\
	x(hard_require_read_device)	\
	x(last_fragment)		\
	x(must_bounce)			\
	x(must_clone)			\
	x(in_retry)			\
	x(no_poison_check)		\
	x(ec_read_around)

enum __bch_read_flags {
#define x(n)	__BCH_READ_##n,
	BCH_READ_FLAGS()
#undef x
};

enum bch_read_flags {
#define x(n)	BCH_READ_##n = BIT(__BCH_READ_##n),
	BCH_READ_FLAGS()
#undef x
};

#endif /* _BCACHEFS_EXTENTS_TYPES_H */
