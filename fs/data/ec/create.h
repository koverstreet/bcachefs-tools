/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_CREATE_H
#define _BCACHEFS_DATA_EC_CREATE_H

#include "io.h"
#include "util/darray.h"

#include "data/ec/create_gen.h"
#include "data/ec/stripe_new_gen.h"

#define bch2_bkey_ops_logged_op_stripe_update ((struct bkey_ops) {	\
	.val_to_text	= bch2_logged_op_stripe_update_to_text,		\
})

#endif /* _BCACHEFS_DATA_EC_CREATE_H */
