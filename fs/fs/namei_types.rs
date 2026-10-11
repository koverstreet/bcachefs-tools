// SPDX-License-Identifier: GPL-2.0

//! fs/namei.h's constants - C gets them in fs/namei_gen.h, which is generated
//! from this file and namei.rs: see fs/types/lib.rs.

use cstruct_macros::c_const;

// bch2_create_trans()'s flags:
c_const! { pub const BCH_CREATE_TMPFILE: u32 = 1 << 0; }
c_const! { pub const BCH_CREATE_SUBVOL: u32 = 1 << 1; }
c_const! { pub const BCH_CREATE_SNAPSHOT: u32 = 1 << 2; }
c_const! { pub const BCH_CREATE_SNAPSHOT_RO: u32 = 1 << 3; }
