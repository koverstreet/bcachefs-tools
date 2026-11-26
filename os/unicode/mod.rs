// SPDX-License-Identifier: GPL-2.0

//! Casefolding as the kernel's fs/unicode does it: NFD, default ignorables
//! removed, and full casefolding - NFDICF - walking the kernel's own trie,
//! for the versions gen.rs took from it. Names on disk were folded by the
//! kernel's, so this must match it byte for byte: walk() is its
//! utf8nlookup(), Cursor its utf8cursor and utf8byte(), and casefold() its
//! utf8_casefold(), kept as they are - a stray continuation byte is passed
//! through where the kernel's finds it.

mod tables;

use crate::errcode::{Errcode, BchError};
use crate::util::os_str::{OsStr, OsStrExt};

/// A Unicode version's map: where its trie starts, and the newest code
/// points it has.
pub struct UnicodeMap {
    root:   usize,
    maxage: u32,
}

/*
 * utf8trie_t
 *
 * A compact binary tree, used to decode UTF-8 characters.
 *
 * Internal nodes are one byte for the node itself, and up to three
 * bytes for an offset into the tree.  The first byte contains the
 * following information:
 *  NEXTBYTE  - flag        - advance to next byte if set
 *  BITNUM    - 3 bit field - the bit number to tested
 *  OFFLEN    - 2 bit field - number of bytes in the offset
 * if offlen == 0 (non-branching node)
 *  RIGHTPATH - 1 bit field - set if the following node is for the
 *                            right-hand path (tested bit is set)
 *  TRIENODE  - 1 bit field - set if the following node is an internal
 *                            node, otherwise it is a leaf node
 * if offlen != 0 (branching node)
 *  LEFTNODE  - 1 bit field - set if the left-hand node is internal
 *  RIGHTNODE - 1 bit field - set if the right-hand node is internal
 *
 * Due to the way utf8 works, there cannot be branching nodes with
 * NEXTBYTE set, and moreover those nodes always have a righthand
 * descendant.
 */
const BITNUM: u8 = 0x07;
const NEXTBYTE: u8 = 0x08;
const OFFLEN: u8 = 0x30;
const OFFLEN_SHIFT: u8 = 4;
const RIGHTPATH: u8 = 0x40;
const TRIENODE: u8 = 0x80;
const RIGHTNODE: u8 = 0x40;
const LEFTNODE: u8 = 0x80;

/*
 * utf8leaf_t
 *
 * The leaves of the trie are embedded in the trie, and so the same
 * underlying datatype: unsigned char.
 *
 * leaf[0]: The unicode version, stored as a generation number that is
 *          an index into AGES. With this we can filter code points based
 *          on the unicode version in which they were defined.  The CCC of
 *          a non-defined code point is 0.
 * leaf[1]: Canonical Combining Class. During normalization, we need
 *          to do a stable sort into ascending order of all characters
 *          with a non-zero CCC that occur between two characters with
 *          a CCC of 0, or at the begin or end of a string.
 *          The unicode standard guarantees that all CCC values are
 *          between 0 and 254 inclusive, which leaves 255 available as
 *          a special value.
 *          Code points with CCC 0 are known as stoppers.
 * leaf[2]: Decomposition. If leaf[1] == 255, then leaf[2] is the
 *          start of a NUL-terminated string that is the decomposition
 *          of the character.
 *          The CCC of a decomposable character is the same as the CCC
 *          of the first character of its decomposition.
 *          Some characters decompose as the empty string: these are
 *          characters with the Default_Ignorable_Code_Point property.
 *          These do affect normalization, as they all have CCC 0.
 *
 * The decompositions in the trie have been fully expanded, with the
 * exception of Hangul syllables, which are decomposed algorithmically.
 *
 * Casefolding, if applicable, is also done using decompositions.
 *
 * The trie is constructed in such a way that leaves exist for all
 * valid UTF-8 sequences - shortest form, no surrogates, nothing past
 * U+10FFFF - and only for those sequences.  Therefore a lookup in the
 * trie can be used to validate the UTF-8 input.
 */
const DECOMPOSE: u8 = 255;
const HANGUL: u8 = 0xff;

const STOPPER: i16 = 0;
const MINCCC: i16 = 0;
const MAXCCC: i16 = 254;

/*
 * Hangul decomposition (algorithm from Section 3.12 of Unicode 6.3.0)
 *
 * AC00;<Hangul Syllable, First>;Lo;0;L;;;;;N;;;;;
 * D7A3;<Hangul Syllable, Last>;Lo;0;L;;;;;N;;;;;
 *
 * SBase = 0xAC00
 * LBase = 0x1100
 * VBase = 0x1161
 * TBase = 0x11A7
 * LCount = 19
 * VCount = 21
 * TCount = 28
 * NCount = 588 (VCount * TCount)
 * SCount = 11172 (LCount * NCount)
 *
 * Decomposition:
 *   SIndex = s - SBase
 *
 * LV (Canonical/Full)
 *   LIndex = SIndex / NCount
 *   VIndex = (Sindex % NCount) / TCount
 *   LPart = LBase + LIndex
 *   VPart = VBase + VIndex
 *
 * LVT (Canonical)
 *   LVIndex = (SIndex / TCount) * TCount
 *   TIndex = (Sindex % TCount)
 *   LVPart = SBase + LVIndex
 *   TPart = TBase + TIndex
 *
 * LVT (Full)
 *   LIndex = SIndex / NCount
 *   VIndex = (Sindex % NCount) / TCount
 *   TIndex = (Sindex % TCount)
 *   LPart = LBase + LIndex
 *   VPart = VBase + VIndex
 *   if (TIndex == 0) {
 *          d = <LPart, VPart>
 *   } else {
 *          TPart = TBase + TIndex
 *          d = <LPart, VPart, TPart>
 *   }
 */
const S_BASE: u32 = 0xac00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11a7;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;

/// Bytes in the UTF-8 sequence @b starts, if it's a well-formed one's start.
fn clen(b: u8) -> usize {
    1 + (b >= 0xc0) as usize + (b >= 0xe0) as usize + (b >= 0xf0) as usize
}

/// The leaf for the UTF-8 sequence @s starts, from @root, reading no further
/// than @s's end: where it is in UTF8DATA. None if @s isn't valid UTF-8 there.
fn walk(root: usize, s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let data = &tables::UTF8DATA;
    let (mut trie, mut i, mut node) = (root, 0, true);
    while node {
        let n = data[trie];
        let mut offlen = ((n & OFFLEN) >> OFFLEN_SHIFT) as usize;
        if n & NEXTBYTE != 0 {
            i += 1;
            if i == s.len() {
                return None;
            }
        }
        if s[i] & (1 << (n & BITNUM)) != 0 {
            /* Right leg */
            if offlen != 0 {
                /* Right node at offset of trie */
                node = n & RIGHTNODE != 0;
                let mut offset = data[trie + offlen] as usize;
                while { offlen -= 1; offlen != 0 } {
                    offset = (offset << 8) | data[trie + offlen] as usize;
                }
                trie += offset;
            } else if n & RIGHTPATH != 0 {
                /* Right node after this node */
                node = n & TRIENODE != 0;
                trie += 1;
            } else {
                /* No right node. */
                return None;
            }
        } else if offlen != 0 {
            /* Left node after this node. */
            node = n & LEFTNODE != 0;
            trie += offlen + 1;
        } else if n & RIGHTPATH != 0 {
            /* No left node. */
            return None;
        } else {
            /* Left node after this node */
            node = n & TRIENODE != 0;
            trie += 1;
        }
    }
    Some(trie)
}

/// A place in what the cursor reads: the name, a decomposition in the trie,
/// or a Hangul syllable's - each read as NUL terminated.
#[derive(Clone, Copy)]
enum Pos {
    Name(usize),
    Decomp(usize),
    Hangul(usize),
}

impl Pos {
    fn add(self, n: usize) -> Pos {
        match self {
            Pos::Name(i)   => Pos::Name(i + n),
            Pos::Decomp(i) => Pos::Decomp(i + n),
            Pos::Hangul(i) => Pos::Hangul(i + n),
        }
    }
}

enum Leaf {
    Ccc(i16),
    Decomposes(Pos),
}

/*
 * The cursor keeps track of the location in the string in s. When a
 * character is decomposed, the current location is stored in p, and s is set
 * to the start of the decomposition. Note that bytes from a decomposition do
 * not count against len.
 *
 * Characters are emitted if they match the current CCC in ccc. Hitting
 * end-of-string while ccc == STOPPER means we're done, and next() returns 0
 * in that case.
 *
 * Sorting by CCC is done by repeatedly scanning the string. The values of s
 * and p are stored in ss and sp at the start of the scan. The first pass
 * finds the lowest CCC to be emitted and stores it in nccc, the second pass
 * emits the characters with this CCC and finds the next lowest CCC. This
 * limits the number of passes to 1 + the number of different CCCs in the
 * sequence being scanned.
 *
 * Therefore:
 *  p.is_some()  -> a decomposition is being scanned.
 *  ccc == -1    -> this is the first scan of a repeating scan.
 */
struct Cursor<'a> {
    map:    &'a UnicodeMap,
    name:   &'a [u8],
    /// A Hangul syllable's jamo, decomposed: three, of three bytes, and a NUL.
    hangul: [u8; 10],
    /// What's read next.
    s:      Pos,
    /// Where in the name to carry on, while a decomposition is read.
    p:      Option<usize>,
    /// s and p at the start of a scan, where the next pass restarts.
    ss:     Pos,
    sp:     Option<usize>,
    /// The name's bytes left from s, and at the start of the scan.
    len:    usize,
    slen:   usize,
    /// The class being emitted, and the lowest one above it seen.
    ccc:    i16,
    nccc:   i16,
}

impl<'a> Cursor<'a> {
    fn new(map: &'a UnicodeMap, name: &'a [u8]) -> Self {
        Cursor {
            map,
            name,
            hangul: [0; 10],
            s:      Pos::Name(0),
            p:      None,
            ss:     Pos::Name(0),
            sp:     None,
            len:    name.len(),
            slen:   0,
            ccc:    STOPPER,
            nccc:   STOPPER,
        }
    }

    fn bytes(&self, pos: Pos) -> &[u8] {
        match pos {
            Pos::Name(i)   => &self.name[i..],
            Pos::Decomp(i) => &tables::UTF8DATA[i..],
            Pos::Hangul(i) => &self.hangul[i..],
        }
    }

    fn byte(&self, pos: Pos) -> u8 {
        self.bytes(pos).first().copied().unwrap_or(0)
    }

    /// The character at @pos - in the name, no further than self.len: its
    /// class, or its decomposition. None if it isn't valid UTF-8 there.
    fn lookup(&mut self, pos: Pos) -> Option<Leaf> {
        let s = match pos {
            Pos::Name(i) => &self.name[i..i + self.len],
            _            => self.bytes(pos),
        };
        let at = walk(self.map.root, s)?;
        let leaf = &tables::UTF8DATA[at..];

        /* Characters that are too new have CCC 0. */
        if tables::AGES[leaf[0] as usize] > self.map.maxage {
            return Some(Leaf::Ccc(STOPPER));
        }
        if leaf[1] != DECOMPOSE {
            return Some(Leaf::Ccc(leaf[1] as i16));
        }

        /*
         * Hangul decomposition is done algorithmically. These are the
         * codepoints >= 0xAC00 and <= 0xD7A3. Their UTF-8 encoding is
         * always 3 bytes long.
         */
        if leaf[2] == HANGUL {
            let cp = (s[0] as u32 & 0x0f) << 12 | (s[1] as u32 & 0x3f) << 6 | (s[2] as u32 & 0x3f);
            let si = cp - S_BASE;
            let jamo = [L_BASE + si / N_COUNT, V_BASE + (si % N_COUNT) / T_COUNT, T_BASE + si % T_COUNT];
            let n = if si % T_COUNT != 0 { 3 } else { 2 };
            let mut h = 0;
            for j in &jamo[..n] {
                h += char::from_u32(*j).unwrap().encode_utf8(&mut self.hangul[h..]).len();
            }
            self.hangul[h] = 0;
            return Some(Leaf::Decomposes(Pos::Hangul(0)));
        }

        Some(Leaf::Decomposes(Pos::Decomp(at + 2)))
    }

    fn step(&mut self) {
        let n = clen(self.byte(self.s));
        if self.p.is_none() {
            self.len -= n;
        }
        self.s = self.s.add(n);
    }

    /// The next byte of the normalization; 0 at its end, None if the name
    /// isn't UTF-8.
    fn next(&mut self) -> Option<u8> {
        'byte: loop {
            let ccc;
            'mismatch: {
                if let Some(p) = self.p && self.byte(self.s) == 0 {
                    self.s = Pos::Name(p);
                    self.p = None;
                }

                if self.p.is_none() && (self.len == 0 || self.byte(self.s) == 0) {
                    if self.ccc == STOPPER {
                        return Some(0);
                    }
                    ccc = STOPPER;
                    break 'mismatch;
                } else if self.byte(self.s) & 0xc0 == 0x80 {
                    if self.p.is_none() {
                        self.len -= 1;
                    }
                    let b = self.byte(self.s);
                    self.s = self.s.add(1);
                    return Some(b);
                }

                let c = match self.lookup(self.s)? {
                    Leaf::Ccc(c) => c,
                    Leaf::Decomposes(d) => {
                        let n = clen(self.byte(self.s));
                        self.len -= n;
                        self.p = match self.s {
                            Pos::Name(i) => Some(i + n),
                            _            => unreachable!("a decomposition decomposes"),
                        };
                        self.s = d;
                        if self.byte(self.s) == 0 {
                            if self.ccc == STOPPER {
                                continue 'byte;
                            }
                            ccc = STOPPER;
                            break 'mismatch;
                        }
                        match self.lookup(self.s)? {
                            Leaf::Ccc(c) => c,
                            Leaf::Decomposes(_) => unreachable!("a decomposition decomposes"),
                        }
                    }
                };

                if c != STOPPER && self.ccc < c && c < self.nccc {
                    self.nccc = c;
                }

                if c == self.ccc {
                    if self.p.is_none() {
                        self.len -= 1;
                    }
                    let b = self.byte(self.s);
                    self.s = self.s.add(1);
                    return Some(b);
                }
                ccc = c;
            }

            if self.nccc == STOPPER {
                self.ccc = MINCCC - 1;
                self.nccc = ccc;
                self.sp = self.p;
                self.ss = self.s;
                self.slen = self.len;
                self.step();
            } else if ccc != STOPPER {
                self.step();
            } else if self.nccc != MAXCCC + 1 {
                self.ccc = self.nccc;
                self.nccc = MAXCCC + 1;
                self.s = self.ss;
                self.p = self.sp;
                self.len = self.slen;
            } else {
                self.ccc = STOPPER;
                self.nccc = STOPPER;
                self.sp = None;
                self.slen = 0;
            }
        }
    }
}

impl UnicodeMap {
    /// Unicode @version's map, the version packed as major << 16 | minor << 8
    /// | revision: one of tables.rs's. None for any other.
    pub fn load(version: u32) -> Option<Self> {
        let &(maxage, root) = tables::ROOTS.iter().find(|(v, _)| *v == version)?;
        Some(UnicodeMap { root, maxage })
    }

    /// @name casefolded, into @out, NUL terminated: how long it is, without
    /// the NUL.
    pub fn casefold(&self, name: &OsStr, out: &mut [u8]) -> Result<usize, BchError> {
        let name = name.as_bytes();
        if name.first().is_some_and(|&b| b & 0xc0 == 0x80) {
            return Err(Errcode::EINVAL_casefold_invalid_utf8.into());
        }

        let mut cur = Cursor::new(self, name);
        for (i, o) in out.iter_mut().enumerate() {
            let b = cur.next().ok_or(BchError::from(Errcode::EINVAL_casefold_invalid_utf8))?;
            *o = b;
            if b == 0 {
                return Ok(i);
            }
        }
        Err(Errcode::EINVAL_casefold_too_long.into())
    }
}

#[cfg(test)]
mod test {
    use super::UnicodeMap;
    use crate::util::os_str::{OsStr, OsStrExt};

    /// What the kernel's C gave, before it went: its quirks with it - a stray
    /// continuation byte passed through, and emitted again by each rescan.
    const GOLDEN: &[(&[u8], usize, Option<&[u8]>)] = &[
        (b"Hello World", 64, Some(b"hello world\x00")),
        (b"Stra\xc3\x9fe", 64, Some(b"strasse\x00")),
        (b"\xe1\xba\x9e\xce\xa3\xcf\x82\xc4\xb0", 64, Some(b"ss\xcf\x83\xcf\x83i\xcc\x87\x00")),
        (b"\xc3\xa9\xc3\x85\xe1\xbb\x91", 64, Some(b"e\xcc\x81a\xcc\x8ao\xcc\x82\xcc\x81\x00")),
        (b"o\xcc\x82\xcc\x81\xcc\xb8", 64, Some(b"o\xcc\xb8\xcc\x82\xcc\x81\x00")),
        (b"a\xcc\x81\xe2\x80\x8b\xcc\x96", 64, Some(b"a\xcc\x81\xcc\x96\x00")),
        (b"a\xcc\x81\xcc\x96", 64, Some(b"a\xcc\x96\xcc\x81\x00")),
        (b"\xea\xb0\x80\xea\xb0\x81\xed\x9e\xa3", 64,
         Some(b"\xe1\x84\x80\xe1\x85\xa1\xe1\x84\x80\xe1\x85\xa1\xe1\x86\xa8\xe1\x84\x92\xe1\x85\xb5\xe1\x87\x82\x00")),
        (b"a\x80b", 64, Some(b"a\x80b\x00")),
        (b"a\xcc\x81\x80\xcc\x96", 64, Some(b"a\x80\x80\xcc\x96\xcc\x81\x80\x00")),
        (b"\x80a", 64, None),
        (b"a\xff", 64, None),
        (b"ab\x00cd", 64, Some(b"ab\x00")),
        (b"abc", 4, Some(b"abc\x00")),
        (b"abc", 3, None),
        (b"", 1, Some(b"\x00")),
        (b"\xf3\xa0\x80\x81\xef\xb8\x8fx", 64, Some(b"x\x00")),
    ];

    #[test]
    fn casefold_golden() {
        let m = UnicodeMap::load((12 << 16) | (1 << 8)).unwrap();
        for &(name, dlen, want) in GOLDEN {
            let mut out = vec![0xaa; dlen];
            let got = m.casefold(OsStr::from_bytes(name), &mut out).ok().map(|n| &out[..n + 1]);
            assert_eq!(got, want, "{}", name.escape_ascii());
        }
    }
}
