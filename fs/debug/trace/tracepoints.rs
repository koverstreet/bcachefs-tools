// SPDX-License-Identifier: GPL-2.0

//! The tracepoints, and the one class every event is, fs_str - the
//! filesystem's name and a string.

use core::ffi::CStr;
use core::mem::{offset_of, size_of};

use crate::c;
use crate::os::tracepoint::{EventClass, Field, TraceSeq};
use crate::util::vstructs::{cast_plain, cast_plain_mut};
use cstruct_macros::Plain;

pub(super) enum FsStr {}

/// __array(char, fs, 40): the filesystem's name.
const FS_LEN: usize = 40;

/// struct trace_event_raw_fs_str's fields, then the string.
#[repr(C)]
#[derive(Plain)]
struct Record {
    fs:      [u8; FS_LEN],
    /// __string(str): str's offset in the entry, its length above 16 bits
    str_loc: u32,
}

const _: () = assert!(offset_of!(Record, str_loc) == FS_LEN);

impl EventClass for FsStr {
    const SYSTEM: &'static CStr = c"bcachefs";
    const FIELDS: &'static [Field] = &[
        Field { ty: c"char[40]", name: c"fs", size: FS_LEN, align: 1, len: FS_LEN },
        Field { ty: c"__data_loc char[]", name: c"str", size: 4, align: 4, len: 0 },
    ];
    const PRINT_FMT: &'static CStr = c"\"%s: %s\", REC->fs, __get_str(str)";

    fn size(_fs: &CStr, s: &CStr) -> usize {
        size_of::<Record>() + s.to_bytes_with_nul().len()
    }

    /// @fs truncated as strscpy() does.
    fn assign(rec: &mut [u8], at: usize, fs: &CStr, s: &CStr) {
        let (head, data) = rec.split_at_mut(size_of::<Record>());
        let r = cast_plain_mut::<Record>(head).expect("an fs_str record");

        let fs = fs.to_bytes();
        let n = fs.len().min(FS_LEN - 1);
        r.fs[..n].copy_from_slice(&fs[..n]);

        let s = s.to_bytes_with_nul();
        r.str_loc = (at + size_of::<Record>()) as u32 | ((s.len() as u32) << 16);
        data[..s.len()].copy_from_slice(s);
    }

    fn print(rec: &[u8], at: usize, out: &mut TraceSeq<'_>) {
        fn until_nul(b: &[u8]) -> &[u8] {
            CStr::from_bytes_until_nul(b).map_or(b, CStr::to_bytes)
        }

        let r = cast_plain::<Record>(rec).expect("an fs_str record");
        let s = &rec[(r.str_loc & 0xffff) as usize - at..][..(r.str_loc >> 16) as usize];

        out.write(until_nul(&r.fs));
        out.write(b": ");
        out.write(until_nul(s));
        out.write(b"\n");
    }
}

crate::event_class!(FsStr);

macro_rules! tracepoints {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        $( crate::tracepoint!(pub(super) FsStr, $name); )*
    };
}
c::PersistentCounter_arms!(tracepoints);
c::BCH_NOCOUNTER_TRACEPOINTS!(tracepoints);
#[cfg(CONFIG_BCACHEFS_PATH_TRACEPOINTS)]
c::BCH_PATH_TRACEPOINTS!(tracepoints);
