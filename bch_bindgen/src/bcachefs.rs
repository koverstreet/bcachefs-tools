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

    pub fn bdev(&self) -> &block_device {
        unsafe { &*self.bdev }
    }
}

impl Drop for bch_sb_handle {
    fn drop(&mut self) {
        unsafe { bch2_free_super(&mut *self); }
    }
}

// #[repr(u8)]
pub enum rhash_lock_head {}
pub enum srcu_struct {}
