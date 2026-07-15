/*
 * Common values for the ChaCha20 algorithm
 */

#ifndef _CRYPTO_CHACHA20_H
#define _CRYPTO_CHACHA20_H

#include <linux/types.h>
#include <linux/crypto.h>
#include <linux/unaligned.h>

#define CHACHA_IV_SIZE	16
#define CHACHA_KEY_SIZE	32
#define CHACHA_BLOCK_SIZE	64

#define CHACHA_KEY_WORDS	8
#define CHACHA_STATE_WORDS	16

struct chacha_state {
	u32 x[CHACHA_STATE_WORDS];
};

enum chacha_constants { /* expand 32-byte k */
	CHACHA_CONSTANT_EXPA = 0x61707865U,
	CHACHA_CONSTANT_ND_3 = 0x3320646eU,
	CHACHA_CONSTANT_2_BY = 0x79622d32U,
	CHACHA_CONSTANT_TE_K = 0x6b206574U
};

static inline void chacha_init_consts(struct chacha_state *state)
{
	state->x[0]  = CHACHA_CONSTANT_EXPA;
	state->x[1]  = CHACHA_CONSTANT_ND_3;
	state->x[2]  = CHACHA_CONSTANT_2_BY;
	state->x[3]  = CHACHA_CONSTANT_TE_K;
}

static inline void chacha_init(struct chacha_state *state,
			       const u32 key[CHACHA_KEY_WORDS],
			       const u8 iv[CHACHA_IV_SIZE])
{
	chacha_init_consts(state);
	state->x[4]  = key[0];
	state->x[5]  = key[1];
	state->x[6]  = key[2];
	state->x[7]  = key[3];
	state->x[8]  = key[4];
	state->x[9]  = key[5];
	state->x[10] = key[6];
	state->x[11] = key[7];
	state->x[12] = get_unaligned_le32(iv +  0);
	state->x[13] = get_unaligned_le32(iv +  4);
	state->x[14] = get_unaligned_le32(iv +  8);
	state->x[15] = get_unaligned_le32(iv + 12);
}

#include <sodium/crypto_stream_chacha20.h>

static inline void chacha20_crypt(struct chacha_state *state, u8 *dst, const u8 *src,
				  unsigned int bytes)
{
	u32 *key = state->x + 4;
	u32 *iv  = state->x + 12;
	int ret = crypto_stream_chacha20_xor_ic(dst, src, bytes,
						(void *) &iv[2],
						iv[0] | ((u64) iv[1] << 32),
						(void *) key);
	BUG_ON(ret);
}

static inline void chacha_zeroize_state(struct chacha_state *state)
{
	memzero_explicit(state, sizeof(*state));
}

#endif
