/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ENUM_KIND_H
#define _BCACHEFS_ENUM_KIND_H

/*
 * What an enum's values can be - which C doesn't say, and which decides the
 * enum's Rust type. Every enum is marked with one of these, after the enum
 * keyword: enum __enum_open btree_id { ... }.
 *
 * __enum_closed:	a value is always one of those listed: none comes
 *			from outside - disk, userspace - unchecked. A Rust enum.
 *
 * __enum_open:		a value may come from outside, and be anything: on
 *			disk, in an ioctl. A newtype over the integer, with the
 *			listed values as constants.
 *
 * __enum_flags:	the values are bits, or'd together. A bitflags type.
 *
 * To C they're nothing.
 */
#define __enum_closed
#define __enum_open
#define __enum_flags

#endif /* _BCACHEFS_ENUM_KIND_H */
