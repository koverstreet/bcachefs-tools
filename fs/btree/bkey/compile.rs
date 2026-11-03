//! A key format's unpack, compiled to x86-64: off while the kernel has no
//! interface for allocating executable memory - bkey.h's
//! HAVE_BCACHEFS_COMPILED_UNPACK.

use core::mem::offset_of;

use super::format::FIELDS;
use super::BKEY_U64S;
use crate::c;
use crate::cstructs::c::bkey_fields;
use crate::format::bch_bkey_fields::{self, *};
use crate::format::bkey_format;
use crate::types::field_size;

struct Emit<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Emit<'_> {
    fn i(&mut self, bytes: &[u8]) {
        self.out[self.len..][..bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
}

fn compile_bkey_field(format: &bkey_format, out: &mut Emit<'_>, field: bch_bkey_fields,
                      dst_offset: u8, dst_size: usize, eax_zeroed: &mut bool) {
    let bits = format.bits(field);
    let offset = format.offset(field);

    if bits == 0 && offset == 0 {
        if !*eax_zeroed {
            /* xor eax, eax */
            out.i(&[0x31, 0xc0]);
        }

        *eax_zeroed = true;
    } else if bits == 0 {
        /* just return offset: */

        match dst_size {
            8 => if offset > i32::MAX as u64 {
                /* mov [rdi + dst_offset], offset */
                out.i(&[0xc7, 0x47, dst_offset]);
                out.i(&(offset as u32).to_le_bytes());

                out.i(&[0xc7, 0x47, dst_offset + 4]);
                out.i(&((offset >> 32) as u32).to_le_bytes());
            } else {
                /* mov [rdi + dst_offset], offset */
                /* sign extended */
                out.i(&[0x48, 0xc7, 0x47, dst_offset]);
                out.i(&(offset as u32).to_le_bytes());
            },
            4 => {
                /* mov [rdi + dst_offset], offset */
                out.i(&[0xc7, 0x47, dst_offset]);
                out.i(&(offset as u32).to_le_bytes());
            }
            _ => unreachable!("a {dst_size} byte bkey field"),
        }

        return;
    } else {
        let mut bit_offset = format.key_u64s as u32 * 64;
        for &f in &FIELDS[..=field as usize] {
            bit_offset -= format.bits(f);
        }

        let mut byte = bit_offset / 8;
        bit_offset -= byte * 8;

        *eax_zeroed = false;

        if bit_offset == 0 && bits == 8 {
            /* movzx eax, BYTE PTR [rsi + imm8] */
            out.i(&[0x0f, 0xb6, 0x46, byte as u8]);
        } else if bit_offset == 0 && bits == 16 {
            /* movzx eax, WORD PTR [rsi + imm8] */
            out.i(&[0x0f, 0xb7, 0x46, byte as u8]);
        } else if bit_offset + bits <= 32 {
            let align = (4 - (bit_offset + bits).div_ceil(8)).min(byte & 3);
            byte -= align;
            bit_offset += align * 8;

            assert!(bit_offset + bits <= 32);

            /* mov eax, [rsi + imm8] */
            out.i(&[0x8b, 0x46, byte as u8]);

            if bit_offset != 0 {
                /* shr eax, imm8 */
                out.i(&[0xc1, 0xe8, bit_offset as u8]);
            }

            if bit_offset + bits < 32 {
                let mask = !0u32 >> (32 - bits);

                /* and eax, imm32 */
                out.i(&[0x25]);
                out.i(&mask.to_le_bytes());
            }
        } else if bit_offset + bits <= 64 {
            let align = (8 - (bit_offset + bits).div_ceil(8)).min(byte & 7);
            byte -= align;
            bit_offset += align * 8;

            assert!(bit_offset + bits <= 64);

            /* mov rax, [rsi + imm8] */
            out.i(&[0x48, 0x8b, 0x46, byte as u8]);

            let shl = 64 - bit_offset - bits;
            let shr = bit_offset + shl;

            if shl != 0 {
                /* shl rax, imm8 */
                out.i(&[0x48, 0xc1, 0xe0, shl as u8]);
            }

            if shr != 0 {
                /* shr rax, imm8 */
                out.i(&[0x48, 0xc1, 0xe8, shr as u8]);
            }
        } else {
            let align = 4u32.wrapping_sub((bit_offset + bits).div_ceil(8)).min(byte & 3);
            byte -= align;
            bit_offset += align * 8;

            assert!(bit_offset + bits <= 96);

            /* mov rax, [rsi + byte] */
            out.i(&[0x48, 0x8b, 0x46, byte as u8]);

            /* mov edx, [rsi + byte + 8] */
            out.i(&[0x8b, 0x56, (byte + 8) as u8]);

            /* bits from next word: */
            let mut shr = bit_offset + bits - 64;
            assert!(shr <= bit_offset);

            /* shr rax, bit_offset */
            out.i(&[0x48, 0xc1, 0xe8, shr as u8]);

            /* shl rdx, imm8 */
            out.i(&[0x48, 0xc1, 0xe2, (64 - shr) as u8]);

            /* or rax, rdx */
            out.i(&[0x48, 0x09, 0xd0]);

            shr = bit_offset - shr;

            if shr != 0 {
                /* shr rax, imm8 */
                out.i(&[0x48, 0xc1, 0xe8, shr as u8]);
            }
        }

        /* rax += offset: */
        if offset > i32::MAX as u64 {
            /* mov rdx, imm64 */
            out.i(&[0x48, 0xba]);
            out.i(&offset.to_le_bytes());
            /* add %rdx, %rax */
            out.i(&[0x48, 0x01, 0xd0]);
        } else if offset.wrapping_add(!0u64 >> (64 - bits)) > u32::MAX as u64 {
            /* add rax, imm32 */
            out.i(&[0x48, 0x05]);
            out.i(&(offset as u32).to_le_bytes());
        } else if offset != 0 {
            /* add eax, imm32 */
            out.i(&[0x05]);
            out.i(&(offset as u32).to_le_bytes());
        }
    }

    match dst_size {
        8 => {
            /* mov [rdi + dst_offset], rax */
            out.i(&[0x48, 0x89, 0x47, dst_offset]);
        }
        4 => {
            /* mov [rdi + dst_offset], eax */
            out.i(&[0x89, 0x47, dst_offset]);
        }
        _ => unreachable!("a {dst_size} byte bkey field"),
    }
}

impl bkey_format {
    /// Into @out: returns its length.
    pub fn compile(&self, out: &mut [u8]) -> usize {
        let mut out = Emit { out, len: 0 };
        let mut eax_zeroed = false;

        /*
         * rdi: dst - unpacked key
         * rsi: src - packed key
         */

        /* k->u64s, k->format, k->type */

        /* mov eax, [rsi] */
        out.i(&[0x8b, 0x06]);

        /* add eax, BKEY_U64s - format->key_u64s */
        out.i(&[0x05, (BKEY_U64S as u8).wrapping_sub(self.key_u64s), c::KEY_FORMAT_CURRENT as u8, 0, 0]);

        /* and eax, imm32: mask out k->pad: */
        out.i(&[0x25, 0xff, 0xff, 0xff, 0]);

        /* mov [rdi], eax */
        out.i(&[0x89, 0x07]);

        macro_rules! compile_fields {
            ($( ($id:ident, $($field:ident).+) ),* $(,)?) => {
                $(compile_bkey_field(self, &mut out, $id,
                                     offset_of!(c::bkey, $($field).+) as u8,
                                     field_size(|k: *const c::bkey| unsafe {
                                         core::ptr::addr_of!((*k).$($field).+)
                                     }) as usize,
                                     &mut eax_zeroed);)*
            };
        }
        bkey_fields!(compile_fields);

        /* retq */
        out.i(&[0xc3]);

        out.len
    }
}
