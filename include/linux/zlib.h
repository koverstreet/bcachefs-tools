#ifndef _ZLIB_H
#define _ZLIB_H

#include <zlib.h>

#define zlib_inflate_workspacesize()		0
#define zlib_deflate_workspacesize(windowBits, memLevel)	0

#define zlib_inflateInit2	inflateInit2
#define zlib_inflate		inflate

#define zlib_deflateInit2	deflateInit2
#define zlib_deflate		deflate
#define zlib_deflateEnd		deflateEnd

#define DEF_MEM_LEVEL 8

#endif /* _ZLIB_H */
