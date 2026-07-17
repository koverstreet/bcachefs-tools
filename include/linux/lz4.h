#include <lz4.h>
#include <lz4hc.h>

/* the kernel lz4 API takes an explicit workspace; liblz4 allocates internally */
#define LZ4_compress_destSize(src, dst, srclen, dstlen, workspace)	\
	LZ4_compress_destSize(src, dst, srclen, dstlen)

#define LZ4_compress_HC(src, dst, srclen, dstlen, level, workspace)	\
	LZ4_compress_HC(src, dst, srclen, dstlen, level)

#define LZ4_MEM_COMPRESS 0
#define LZ4HC_MEM_COMPRESS 0

/*
 * The kernel's threshold for plain lz4 vs lz4hc; must be > 0, or the
 * unsigned comparison in attempt_compress() sends every compress - level 0
 * included - down the HC path:
 */
#define LZ4HC_MIN_CLEVEL 3
