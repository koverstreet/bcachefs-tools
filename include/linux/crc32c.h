#ifndef _LINUX_CRC32C_H
#define _LINUX_CRC32C_H

#include <linux/kernel.h>

/* Avoid conflicts with libblkid's crc32 function in static builds */
#define crc32c bch_crc32c
u32 crc32c(u32, const void *, size_t);

#endif	/* _LINUX_CRC32C_H */
