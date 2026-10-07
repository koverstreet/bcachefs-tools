/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_BKEY_TYPES_INLINE_H
#define _BCACHEFS_BTREE_BKEY_TYPES_INLINE_H

/* Included at the end of btree/bkey_types.h. */

static inline void set_bkey_val_u64s(struct bkey *k, unsigned val_u64s)
{
	unsigned u64s = BKEY_U64s + val_u64s;

	BUG_ON(u64s > U8_MAX);
	k->u64s = u64s;
}

static inline void set_bkey_val_bytes(struct bkey *k, unsigned bytes)
{
	set_bkey_val_u64s(k, DIV_ROUND_UP(bytes, sizeof(u64)));
}

/*
 * The conversion functions for the types bkey_types.h generates for each type
 * of value; they check that the value is of the correct type.
 */
#define x(name, ...)							\
static inline struct bkey_i_##name *bkey_i_to_##name(struct bkey_i *k)	\
{									\
	BUG_ON(!IS_ERR_OR_NULL(k) && k->k.type != KEY_TYPE_##name);	\
	return container_of(&k->k, struct bkey_i_##name, k);		\
}									\
									\
static inline const struct bkey_i_##name *				\
bkey_i_to_##name##_c(const struct bkey_i *k)				\
{									\
	BUG_ON(!IS_ERR_OR_NULL(k) && k->k.type != KEY_TYPE_##name);	\
	return container_of(&k->k, struct bkey_i_##name, k);		\
}									\
									\
static inline struct bkey_s_##name bkey_s_to_##name(struct bkey_s k)	\
{									\
	BUG_ON(!IS_ERR_OR_NULL(k.k) && k.k->type != KEY_TYPE_##name);	\
	return (struct bkey_s_##name) {					\
		.k = k.k,						\
		.v = container_of(k.v, struct bch_##name, v),		\
	};								\
}									\
									\
static inline struct bkey_s_c_##name bkey_s_c_to_##name(struct bkey_s_c k)\
{									\
	BUG_ON(!IS_ERR_OR_NULL(k.k) && k.k->type != KEY_TYPE_##name);	\
	return (struct bkey_s_c_##name) {				\
		.k = k.k,						\
		.v = container_of(k.v, struct bch_##name, v),		\
	};								\
}									\
									\
static inline struct bkey_s_##name name##_i_to_s(struct bkey_i_##name *k)\
{									\
	return (struct bkey_s_##name) {					\
		.k = &k->k,						\
		.v = &k->v,						\
	};								\
}									\
									\
static inline struct bkey_s_c_##name					\
name##_i_to_s_c(const struct bkey_i_##name *k)				\
{									\
	return (struct bkey_s_c_##name) {				\
		.k = &k->k,						\
		.v = &k->v,						\
	};								\
}									\
									\
static inline struct bkey_s_##name bkey_i_to_s_##name(struct bkey_i *k)	\
{									\
	BUG_ON(!IS_ERR_OR_NULL(k) && k->k.type != KEY_TYPE_##name);	\
	return (struct bkey_s_##name) {					\
		.k = &k->k,						\
		.v = container_of(&k->v, struct bch_##name, v),		\
	};								\
}									\
									\
static inline struct bkey_s_c_##name					\
bkey_i_to_s_c_##name(const struct bkey_i *k)				\
{									\
	BUG_ON(!IS_ERR_OR_NULL(k) && k->k.type != KEY_TYPE_##name);	\
	return (struct bkey_s_c_##name) {				\
		.k = &k->k,						\
		.v = container_of(&k->v, struct bch_##name, v),		\
	};								\
}									\
									\
static inline struct bkey_i_##name *bkey_##name##_init(struct bkey_i *_k)\
{									\
	struct bkey_i_##name *k =					\
		container_of(&_k->k, struct bkey_i_##name, k);		\
									\
	bkey_init(&k->k);						\
	memset(&k->v, 0, sizeof(k->v));					\
	k->k.type = KEY_TYPE_##name;					\
	set_bkey_val_bytes(&k->k, sizeof(k->v));			\
									\
	return k;							\
}

BCH_BKEY_TYPES();
#undef x

static inline struct bkey_i *bkey_next(struct bkey_i *k)
{
	return (struct bkey_i *) ((u64 *) k->_data + k->k.u64s);
}

static inline size_t bkey_val_bytes(const struct bkey *k)
{
	return bkey_val_u64s(k) * sizeof(u64);
}

static inline struct bkey_s bkey_to_s(struct bkey *k)
{
	return (struct bkey_s) { .k = k, .v = NULL };
}

static inline struct bkey_s_c bkey_to_s_c(const struct bkey *k)
{
	return (struct bkey_s_c) { .k = k, .v = NULL };
}

static inline struct bkey_s bkey_i_to_s(struct bkey_i *k)
{
	return (struct bkey_s) { .k = &k->k, .v = &k->v };
}

static inline struct bkey_s_c bkey_i_to_s_c(const struct bkey_i *k)
{
	return (struct bkey_s_c) { .k = &k->k, .v = &k->v };
}

#endif /* _BCACHEFS_BTREE_BKEY_TYPES_INLINE_H */
