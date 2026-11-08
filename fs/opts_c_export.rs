//! opts.rs's wrappers for C: the tables C reads, the defaults under C's
//! name, and each a C name over a Rust function, with C's types - superblock
//! pointers, C strings, an errcode returned. Deleted with the last C caller.

use super::*;
use cstruct_macros::c_export;
use crate::format::{Btree, DataType, MemberState, SbFeature};

#[c_export]
pub static bch2_opts_default: c::bch_opts = DEFAULTS;

/// The tables C still reads: each enum's names, as C's string tables have
/// them.
macro_rules! c_names {
    ($($decl:tt $name:ident: $t:ty;)*) => { $(
        #[c_export]
        #[c($decl)]
        pub static $name: [*const c_char; names::c_table_len::<$t>()] = crate::c_names_table!($t);
    )* };
}
c_names! {
    "const char * const bch2_error_actions[]"  bch2_error_actions: c::bch_error_actions;
    "const char * const bch2_sb_features[]"    bch2_sb_features: SbFeature;
    "const char * const __bch2_btree_ids[]"    __bch2_btree_ids: Btree;
    "const char * const __bch2_csum_types[]"   __bch2_csum_types: c::bch_csum_type;
    "const char * const __bch2_data_types[]"   __bch2_data_types: DataType;
    "const char * const bch2_member_states[]"  bch2_member_states: MemberState;
}
