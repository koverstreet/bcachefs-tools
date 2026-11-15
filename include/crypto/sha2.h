/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Common values for SHA algorithms
 */

#ifndef _CRYPTO_SHA_H
#define _CRYPTO_SHA_H

#include <linux/types.h>
#include <sodium/crypto_hash_sha256.h>

static inline void sha256(const u8 *data, unsigned int len, u8 *out)
{
	crypto_hash_sha256(out, data, len);
}
#endif
