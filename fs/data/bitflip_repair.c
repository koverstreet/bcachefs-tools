// SPDX-License-Identifier: GPL-2.0
/*
 * bcachefs CRC32C single-bit error correction
 *
 * When a CRC32C checksum mismatch is detected on read and no other
 * replica is available, attempt to locate and correct a single-bit
 * error using syndrome analysis.
 *
 * CRC is linear over GF(2): CRC(A ^ B) = CRC(A) ^ CRC(B).
 * The syndrome S = CRC(received) ^ CRC(expected) equals CRC(error_pattern).
 * For a single-bit flip at position i, S = CRC of a buffer with only
 * bit i set.  CRC32C has minimum Hamming distance >= 3, so each
 * single-bit syndrome is unique — no false positives.
 *
 * Optimization: for reflected CRC32C, the syndrome for bit j in byte b
 * of an L-byte buffer equals T[0x80>>j] advanced through (L-b-1) zero
 * bytes, where T[] is the standard CRC32C byte lookup table and zero-byte
 * advancement Z(s) = T[s & 0xFF] ^ (s >> 8) is GF(2)-linear.  By
 * iterating right-to-left we compute all syndromes in O(L) with O(1)
 * per bit — 8 table lookups + 8 XOR/shift per byte position.
 *
 * Reference: Stefan Filipek, "On correcting bit errors with CRCs"
 *   https://srfilipek.medium.com/on-correcting-bit-errors-with-crcs-1f1c98fc58b
 */

#include "bcachefs.h"
#include "data/bitflip_repair.h"
#include "data/checksum.h"

#include <linux/bio.h>
#include <linux/crc32c.h>
#include <linux/sort.h>
#include <linux/slab.h>

#define CRC32C_POLY	0x82F63B78U

/* CRC32C byte lookup table — matches the kernel's reflected convention */
static u32 bch2_crc32c_table[256];
static bool bch2_crc32c_table_ready;
static DEFINE_MUTEX(bch2_crc32c_table_lock);

static void bch2_init_crc32c_table(void)
{
	unsigned int i, j;

	mutex_lock(&bch2_crc32c_table_lock);
	if (bch2_crc32c_table_ready)
		goto out;

	for (i = 0; i < 256; i++) {
		u32 c = i;

		for (j = 0; j < 8; j++) {
			if (c & 1)
				c = (c >> 1) ^ CRC32C_POLY;
			else
				c >>= 1;
		}
		bch2_crc32c_table[i] = c;
	}
	smp_wmb();
	bch2_crc32c_table_ready = true;
out:
	mutex_unlock(&bch2_crc32c_table_lock);
}

/* Advance CRC state through one zero byte */
static inline u32 crc32c_advance_zero(u32 s)
{
	return bch2_crc32c_table[s & 0xFF] ^ (s >> 8);
}

struct syndrome_entry {
	u32	syndrome;
	u32	bit_pos;
};

static int syndrome_cmp(const void *a, const void *b)
{
	const struct syndrome_entry *sa = a;
	const struct syndrome_entry *sb = b;

	if (sa->syndrome < sb->syndrome)
		return -1;
	if (sa->syndrome > sb->syndrome)
		return 1;
	return 0;
}

/*
 * Build syndrome table in O(n) by iterating byte positions right-to-left.
 *
 * At the rightmost byte (0 trailing zeros):
 *   syndrome for bit j = T[0x80 >> j]
 * Moving one byte left:
 *   syndrome = Z(old) = advance through one more trailing zero byte
 */
static struct syndrome_entry *bch2_build_syndrome_table(unsigned long total_bits,
							unsigned long byte_count)
{
	struct syndrome_entry *table;
	u32 basis[8];
	long b;
	int j;

	table = kvmalloc_array(total_bits, sizeof(*table), GFP_KERNEL);
	if (!table)
		return NULL;

	for (j = 0; j < 8; j++)
		basis[j] = bch2_crc32c_table[0x80U >> j];

	for (b = byte_count - 1; b >= 0; b--) {
		for (j = 0; j < 8; j++) {
			unsigned long idx = (unsigned long)b * 8 + j;

			table[idx].syndrome = basis[j];
			table[idx].bit_pos  = idx;
		}
		if (b > 0) {
			for (j = 0; j < 8; j++)
				basis[j] = crc32c_advance_zero(basis[j]);
		}
	}

	sort(table, total_bits, sizeof(*table), syndrome_cmp, NULL);
	return table;
}

static struct syndrome_entry *
syndrome_bsearch(struct syndrome_entry *table, unsigned long count, u32 target)
{
	unsigned long lo = 0, hi = count;

	while (lo < hi) {
		unsigned long mid = lo + (hi - lo) / 2;

		if (table[mid].syndrome < target)
			lo = mid + 1;
		else
			hi = mid;
	}
	if (lo < count && table[lo].syndrome == target)
		return &table[lo];
	return NULL;
}

static inline void bio_flip_bit(struct bio *bio, unsigned long bit_pos)
{
	unsigned long byte_pos = bit_pos / 8;
	unsigned int  bit_off  = bit_pos % 8;
	struct bio_vec bv;
	struct bvec_iter iter;
	unsigned long offset = 0;

	bio_for_each_segment(bv, bio, iter) {
		if (byte_pos < offset + bv.bv_len) {
			unsigned page_off = bv.bv_offset + (byte_pos - offset);
			u8 *p = kmap_local_page(bv.bv_page);

			p[page_off] ^= 0x80U >> bit_off;
			kunmap_local(p);
			return;
		}
		offset += bv.bv_len;
	}
}

/*
 * Attempt single-bit error correction on a bio with a CRC32C checksum
 * mismatch.  Only applies to BCH_CSUM_crc32c and BCH_CSUM_crc32c_nonzero.
 *
 * Returns 0 on successful repair, -errno on failure.
 */
int bch2_try_bitflip_repair_bio(struct bch_fs *c, struct bio *bio,
				struct bch_extent_crc_unpacked *crc,
				struct bch_csum expected)
{
	struct bch_csum computed;
	struct nonce nonce = { .d = { 0 } };
	struct syndrome_entry *table, *match;
	unsigned long total_bytes, total_bits;
	u32 syndrome;

	if (crc->csum_type != BCH_CSUM_crc32c &&
	    crc->csum_type != BCH_CSUM_crc32c_nonzero)
		return -EINVAL;

	total_bytes = bio->bi_iter.bi_size;
	total_bits  = total_bytes * 8;
	if (!total_bits || total_bits > (unsigned long)U32_MAX)
		return -EINVAL;

	/* Compute syndrome: XOR of computed and expected CRC values.
	 * The syndrome is identical regardless of init value (zero vs
	 * nonzero) because the init XOR cancels out. */
	computed = bch2_checksum_bio(c, crc->csum_type, nonce, bio);
	syndrome = (u32)(le64_to_cpu(computed.lo) ^ le64_to_cpu(expected.lo));

	if (!syndrome)
		return 0;  /* no error */

	bch2_init_crc32c_table();

	table = bch2_build_syndrome_table(total_bits, total_bytes);
	if (!table)
		return -ENOMEM;

	match = syndrome_bsearch(table, total_bits, syndrome);
	if (!match) {
		kvfree(table);
		return -ENODATA; /* multi-bit error, can't fix */
	}

	bio_flip_bit(bio, match->bit_pos);
	kvfree(table);

	/* Verify repair by recomputing checksum */
	computed = bch2_checksum_bio(c, crc->csum_type, nonce, bio);
	if (bch2_crc_cmp(computed, expected)) {
		/* Shouldn't happen for a true 1-bit error; undo */
		bio_flip_bit(bio, match->bit_pos);
		return -ENODATA;
	}

	bch_info(c, "bitflip repair: corrected single-bit error at bit %u (byte %u, bit %u in byte)",
		 match->bit_pos, match->bit_pos / 8, match->bit_pos % 8);
	return 0;
}
