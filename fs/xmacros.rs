// SPDX-License-Identifier: GPL-2.0

//! C's x-macro lists that Rust uses, read from C's headers until those are
//! converted (c_xmacro_from_c!): each is NAME!(cb), the same as the c_xmacro!
//! its header gets then, which replaces it here. Paths are relative to fs/.

use cstruct_macros::c_xmacro_from_c;

c_xmacro_from_c!(BCH_SB_FIELDS,			"bcachefs_format_types.h");
c_xmacro_from_c!(BCH_SB_COMPAT,			"bcachefs_format_types.h");
c_xmacro_from_c!(BCH_METADATA_VERSIONS,		"bcachefs_format_types.h");
c_xmacro_from_c!(BCH_BKEY_TYPES,		"bcachefs_format_types.h");
c_xmacro_from_c!(BCH_BTREE_IDS,			"bcachefs_format_types.h");
c_xmacro_from_c!(BCH_JSET_ENTRY_TYPES,		"bcachefs_format_types.h");
c_xmacro_from_c!(BCH_COMPRESSION_TYPES,		"bcachefs_format_types.h");

c_xmacro_from_c!(BCH_DATA_TYPES,		"alloc/accounting_format.h");
c_xmacro_from_c!(BCH_DISK_ACCOUNTING_TYPES,	"alloc/accounting_format.h");
c_xmacro_from_c!(BCH_EXTENT_ENTRY_TYPES,	"data/extents_format.h");
c_xmacro_from_c!(BCH_RECONCILE_OPTS,		"data/reconcile/format.h");
c_xmacro_from_c!(BCH_RECONCILE_ACCOUNTING,	"data/reconcile/format.h");

c_xmacro_from_c!(BLK_ERRS,			"errcode_types.h");
c_xmacro_from_c!(ZSTD_ERRS,			"errcode_types.h");
c_xmacro_from_c!(BCH_ERRCODES,			"errcode_types.h");

c_xmacro_from_c!(BCH_INODE_FIELDS_v2,		"fs/inode_format.h");
c_xmacro_from_c!(BCH_INODE_FIELDS_v3,		"fs/inode_format.h");
c_xmacro_from_c!(BCH_INODE_OPTS,		"fs/inode_format.h");
c_xmacro_from_c!(BCH_INODE_FLAGS,		"fs/inode_format.h");
c_xmacro_from_c!(BCH_OPTS,			"opts_types.h");

c_xmacro_from_c!(BCH_MEMBER_STATES,		"sb/members_format.h");
c_xmacro_from_c!(BCH_PERSISTENT_COUNTERS,	"sb/counters_format.h");
c_xmacro_from_c!(BCH_SB_ERRS,			"sb/errors_format.h");

c_xmacro_from_c!(BCH_SNAPSHOT_STATES,		"snapshots/format.h");
c_xmacro_from_c!(BCH_SUBVOLUME_STATES,		"snapshots/format.h");
