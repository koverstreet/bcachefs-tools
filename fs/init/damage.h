/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DAMAGE_H
#define _BCACHEFS_DAMAGE_H

/* The damage btree: init/damage.rs */

#define bch2_bkey_ops_damage ((struct bkey_ops) {	\
	.key_validate	= bch2_damage_validate,		\
	.val_to_text	= bch2_damage_to_text,		\
})

#endif /* _BCACHEFS_DAMAGE_H */
