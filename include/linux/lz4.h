#include <lz4.h>

#define LZ4_compress_destSize(src, dst, srclen, dstlen, workspace)	\
	LZ4_compress_destSize(src, dst, srclen, dstlen)

#define LZ4_compress_HC(src, dst, srclen, dstlen, level, workspace)	-1

#define LZ4_MEM_COMPRESS 0
#define LZ4HC_MEM_COMPRESS 0
#define LZ4HC_MIN_CLEVEL 0
