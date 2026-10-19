// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "sb/members.h"

/* For Rust: static inlines over percpu refs and RCU */
void rust_bch2_dev_put(struct bch_dev *ca)
{
	bch2_dev_put(ca);
}

void rust_bch2_dev_get(struct bch_dev *ca)
{
	bch2_dev_get(ca);
}

void rust_bch2_dev_get_outer(struct bch_dev *ca)
{
	bch2_dev_get_outer(ca);
}

void rust_bch2_dev_put_outer(struct bch_dev *ca)
{
	bch2_dev_put_outer(ca);
}
