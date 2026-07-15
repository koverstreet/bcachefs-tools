/*
 * Scatterlist Cryptographic API.
 *
 * Copyright (c) 2002 James Morris <jmorris@intercode.com.au>
 * Copyright (c) 2002 David S. Miller (davem@redhat.com)
 * Copyright (c) 2005 Herbert Xu <herbert@gondor.apana.org.au>
 *
 * Portions derived from Cryptoapi, by Alexander Kjeldaas <astor@fast.no>
 * and Nettle, by Niels Möller.
 * 
 * This program is free software; you can redistribute it and/or modify it
 * under the terms of the GNU General Public License as published by the Free
 * Software Foundation; either version 2 of the License, or (at your option) 
 * any later version.
 *
 */
#ifndef _LINUX_CRYPTO_H
#define _LINUX_CRYPTO_H

#include <linux/kernel.h>
#include <linux/list.h>
#include <linux/slab.h>

#define CRYPTO_MINALIGN ARCH_KMALLOC_MINALIGN
#define CRYPTO_MINALIGN_ATTR __attribute__ ((__aligned__(CRYPTO_MINALIGN)))

struct crypto_type;

struct crypto_alg {
	struct list_head	cra_list;

	const char		*cra_name;
	const struct crypto_type *cra_type;

	void *			(*alloc_tfm)(void);
} CRYPTO_MINALIGN_ATTR;

int crypto_register_alg(struct crypto_alg *alg);

struct crypto_tfm {
	struct crypto_alg	*alg;
};

#endif	/* _LINUX_CRYPTO_H */

