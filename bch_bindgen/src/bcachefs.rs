#![allow(clippy::missing_safety_doc)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::transmute_int_to_bool)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::useless_transmute)]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unused)]
#![allow(unnecessary_transmutes)]

use crate::c;

include!(concat!(env!("OUT_DIR"), "/bcachefs.rs"));

use bitfield::bitfield;
bitfield! {
    pub struct bch_scrypt_flags(u64);
    pub N, _: 15, 0;
    pub R, _: 31, 16;
    pub P, _: 47, 32;
}
bitfield! {
    pub struct bch_crypt_flags(u64);
    pub TYPE, _: 4, 0;
}
impl bch_sb_field_crypt {
    pub fn scrypt_flags(&self) -> Option<bch_scrypt_flags> {
        use std::convert::TryInto;
        match bch_kdf_types(bch_crypt_flags(self.flags).TYPE().try_into().ok()?) {
            bch_kdf_types::BCH_KDF_SCRYPT => Some(bch_scrypt_flags(self.kdf_flags)),
            _ => None,
        }
    }
    pub fn key(&self) -> &bch_encrypted_key {
        &self.key
    }
}
impl PartialEq for bch_sb {
    fn eq(&self, other: &Self) -> bool {
        self.magic.b == other.magic.b
            && self.user_uuid.b == other.user_uuid.b
            && self.block_size == other.block_size
            && self.version == other.version
            && self.uuid.b == other.uuid.b
            && self.seq == other.seq
    }
}

impl bch_sb {
    pub fn field<F: crate::sb::SbField>(&self) -> Option<&F> {
        crate::sb::sb_field_get(self)
    }

    pub fn crypt(&self) -> Option<&bch_sb_field_crypt> {
        self.field()
    }

    pub fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_bytes(self.user_uuid.b)
    }

    pub fn number_of_devices(&self) -> u32 {
        unsafe { c::bch2_sb_nr_devices(self) }
    }

    /// Get the nonce used to encrypt the superblock
    pub fn nonce(&self) -> nonce {
        let [a, b, c, d, e, f, g, h, _rest @ ..] = self.uuid.b;
        let dword1 = u32::from_le_bytes([a, b, c, d]);
        let dword2 = u32::from_le_bytes([e, f, g, h]);
        nonce {
            d: [0, 0, dword1, dword2],
        }
    }
}
impl bch_sb_handle {
    pub fn sb(&self) -> &bch_sb {
        unsafe { &*self.sb }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        unsafe { &mut *self.sb }
    }

    pub fn bdev(&self) -> &block_device {
        unsafe { &*self.bdev }
    }

    /// Get a typed reference to a superblock field, or None if absent.
    pub fn field<F: crate::sb::SbField>(&self) -> Option<&F> {
        crate::sb::sb_field_get(self.sb())
    }

    /// Get a typed mutable reference to a superblock field, or None if absent.
    pub fn field_mut<F: crate::sb::SbField>(&mut self) -> Option<&mut F> {
        crate::sb::sb_field_get_mut(self)
    }

    /// Resize a superblock field to `u64s` 64-bit words.
    pub fn field_resize<F: crate::sb::SbField>(&mut self, u64s: u32) -> Option<&mut F> {
        crate::sb::sb_field_resize(self, u64s)
    }

    /// Get or create a superblock field with at least `min_u64s` size.
    pub fn field_get_minsize<F: crate::sb::SbField>(&mut self, min_u64s: u32) -> Option<&mut F> {
        crate::sb::sb_field_get_minsize(self, min_u64s)
    }

    /// Get a mutable reference to a single member entry by device index.
    ///
    /// This is the simple accessor for one-shot field mutation. For
    /// iteration, use `members_v2_mut()`.
    pub fn member_mut(&mut self, idx: u32) -> Option<&mut bch_member> {
        let nr = self.sb().nr_devices as u32;
        if idx >= nr { return None; }
        unsafe { Some(&mut *c::bch2_members_v2_get_mut(self.sb, idx as i32)) }
    }

    /// Read-only, bounds-checked access to members_v2.
    pub fn members_v2(&self) -> Option<crate::sb::MembersV2<'_>> {
        crate::sb::members_v2(self.sb())
    }

    /// Mutable, bounds-checked access to members_v2.
    pub fn members_v2_mut(&mut self) -> Option<crate::sb::MembersV2Mut<'_>> {
        crate::sb::members_v2_mut(self)
    }

    /// Read-only, bounds-checked access to members_v1.
    pub fn members_v1(&self) -> Option<crate::sb::MembersV1<'_>> {
        crate::sb::members_v1(self.sb())
    }
}

impl Drop for bch_sb_handle {
    fn drop(&mut self) {
        unsafe { bch2_free_super(&mut *self); }
    }
}

impl bch_opt_strs {
    /// Set a deferred option string by opt table index.
    ///
    /// The string is strdup'd into C heap memory so it can be freed
    /// by `bch2_opt_strs_free`.
    pub fn set(&mut self, id: usize, val: &std::ffi::CStr) {
        unsafe {
            self.__bindgen_anon_1.by_id[id] = libc::strdup(val.as_ptr());
        }
    }

    /// Parse all option strings into a bch_opts struct.
    pub fn parse(&self) -> bch_opts {
        unsafe { c::bch2_parse_opts(*self) }
    }

    /// Free all strdup'd option strings.
    pub fn free(&mut self) {
        unsafe { c::bch2_opt_strs_free(self) }
    }
}

// #[repr(u8)]
pub enum rhash_lock_head {}
pub enum srcu_struct {}
