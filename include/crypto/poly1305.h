/*
 * Common values for the Poly1305 algorithm
 */

#ifndef _CRYPTO_POLY1305_H
#define _CRYPTO_POLY1305_H

#include <sodium/crypto_onetimeauth_poly1305.h>

#define POLY1305_KEY_SIZE	crypto_onetimeauth_poly1305_KEYBYTES
#define POLY1305_DIGEST_SIZE	crypto_onetimeauth_poly1305_BYTES

struct poly1305_desc_ctx {
	crypto_onetimeauth_poly1305_state	s;
};

static inline void poly1305_init(struct poly1305_desc_ctx *desc, const u8 *key)
{
	int ret = crypto_onetimeauth_poly1305_init(&desc->s, key);
	BUG_ON(ret);
}

static inline void poly1305_update(struct poly1305_desc_ctx *desc,
				   const u8 *src, unsigned int nbytes)
{
	int ret = crypto_onetimeauth_poly1305_update(&desc->s, src, nbytes);
	BUG_ON(ret);
}

static inline void poly1305_final(struct poly1305_desc_ctx *desc, u8 *digest)
{
	int ret = crypto_onetimeauth_poly1305_final(&desc->s, digest);
	BUG_ON(ret);
}

#endif
