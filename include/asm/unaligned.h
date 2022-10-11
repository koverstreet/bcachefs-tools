#ifndef _ASM_UNALIGNED_H
#define _ASM_UNALIGNED_H

#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
# include <linux/unaligned/le_struct.h>
# include <linux/unaligned/be_byteshift.h>
# include <linux/unaligned/generic.h>
# define get_unaligned	__get_unaligned_le
# define put_unaligned	__put_unaligned_le
#elif __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
# include <linux/unaligned/be_struct.h>
# include <linux/unaligned/le_byteshift.h>
# include <linux/unaligned/generic.h>
# define get_unaligned	__get_unaligned_be
# define put_unaligned	__put_unaligned_be
#else
# error need to define endianness
#endif

#endif /* _ASM_UNALIGNED_H */
