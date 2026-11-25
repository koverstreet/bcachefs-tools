// SPDX-License-Identifier: GPL-2.0

//! Writes tables.rs from the kernel's fs/unicode/utf8data.c: its NFDICF trie,
//! for the Unicode versions given, as the kernel has it - the bytes from the
//! first of their roots on, a version's root an offset into them. The
//! kernel's table is what names on disk were folded with, so it's the source,
//! not the UCD.
//!
//!   rustc -O os/unicode/gen.rs -o /tmp/unicode-gen
//!   /tmp/unicode-gen path/to/utf8data.c 12.1.0 > os/unicode/tables.rs
//!
//! Not part of the build. With more than one version, what only then does
//! anything - load() choosing a root, a leaf too new for the loaded version
//! being a stopper - wants a golden case per version: mod.rs's are 12.1's.

use std::fmt::Write;

/// The hex numbers in @text from the line starting @start to the next "};".
fn array(text: &str, start: &str) -> Vec<u32> {
    let from = text.find(start).unwrap_or_else(|| panic!("no {start}"));
    let body = &text[from..];
    let body = &body[body.find('{').unwrap() + 1..body.find("};").unwrap()];
    body.lines()
        .filter(|l| !l.trim_start().starts_with("/*"))
        .flat_map(|l| l.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|w| match w.strip_prefix("0x") {
            Some(h) => u32::from_str_radix(h, 16).unwrap(),
            None    => w.parse().unwrap(),
        })
        .collect()
}

/// The leaf for UTF-8 sequence @s in the trie at @root, and the lowest byte
/// the walk read - the kernel's utf8nlookup(), as mod.rs has it.
fn lookup(data: &[u8], root: usize, s: &[u8]) -> Option<(usize, usize)> {
    let (mut trie, mut i, mut node, mut low) = (root, 0, true, root);
    while node {
        let n = data[trie];
        let mut offlen = ((n & 0x30) >> 4) as usize;
        if n & 0x08 != 0 {
            i += 1;
            if i == s.len() {
                return None;
            }
        }
        if s[i] & (1 << (n & 0x07)) != 0 {
            if offlen != 0 {
                node = n & 0x40 != 0;
                let mut offset = data[trie + offlen] as usize;
                while { offlen -= 1; offlen != 0 } {
                    offset = (offset << 8) | data[trie + offlen] as usize;
                }
                trie += offset;
            } else if n & 0x40 != 0 {
                node = n & 0x80 != 0;
                trie += 1;
            } else {
                return None;
            }
        } else if offlen != 0 {
            node = n & 0x80 != 0;
            trie += offlen + 1;
        } else if n & 0x40 != 0 {
            return None;
        } else {
            node = n & 0x80 != 0;
            trie += 1;
        }
        low = low.min(trie);
    }
    Some((trie, low))
}

fn version(s: &str) -> u32 {
    let v: Vec<u32> = s.split('.').map(|n| n.parse().expect("a version is major.minor.revision")).collect();
    assert_eq!(v.len(), 3, "a version is major.minor.revision");
    v[0] << 16 | v[1] << 8 | v[2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: unicode-gen utf8data.c VERSION...");
    let versions: Vec<u32> = args.map(|v| version(&v)).collect();
    assert!(!versions.is_empty(), "usage: unicode-gen utf8data.c VERSION...");

    let text = std::fs::read_to_string(&path).unwrap();
    let ages = array(&text, "static const unsigned int utf8agetab[]");
    let nfdicf = array(&text, "static const struct utf8data utf8nfdicfdata[]");
    let data: Vec<u8> = array(&text, "static const unsigned char utf8data[")
        .into_iter().map(|b| b as u8).collect();

    let roots: Vec<(u32, usize)> = versions.iter().map(|&v| {
        let e = nfdicf.chunks(2).find(|e| e[0] == v)
            .unwrap_or_else(|| panic!("no NFDICF table for {v:#x}"));
        (v, e[1] as usize)
    }).collect();
    let base = roots.iter().map(|r| r.1).min().unwrap();

    // What mod.rs takes as given, of every leaf of each root, and that the
    // walk never reads before the bytes kept: offsets point forward
    let mut buf = [0u8; 4];
    for &(v, root) in &roots {
        for cp in 0..0x110000u32 {
            let Some(c) = char::from_u32(cp) else { continue };
            let (leaf, low) = lookup(&data, root, c.encode_utf8(&mut buf).as_bytes())
                .unwrap_or_else(|| panic!("no leaf for U+{cp:04X}"));
            assert!(low >= base);

            if data[leaf + 1] != 255 || ages[data[leaf] as usize] > v {
                continue;
            }
            if data[leaf + 2] == 0xff {
                assert!((0xac00..=0xd7a3).contains(&cp), "Hangul decomposition at U+{cp:04X}");
                // The kernel's leaf for one is made in code, of 2.0: mod.rs
                // checks the age of this one, so it must be the same
                assert_eq!(ages[data[leaf] as usize], 0x20000, "U+{cp:04X} isn't of 2.0");
                continue;
            }
            let d = &data[leaf + 2..leaf + 2 + data[leaf + 2..].iter().position(|&b| b == 0).unwrap()];
            for dc in std::str::from_utf8(d).expect("a decomposition is UTF-8").chars() {
                let (dl, _) = lookup(&data, root, dc.encode_utf8(&mut [0; 4]).as_bytes()).unwrap();
                assert!(data[dl + 1] != 255, "U+{cp:04X}'s decomposition decomposes again");
            }
        }
    }

    let kept = &data[base..];
    let mut out = String::new();
    writeln!(out, "// SPDX-License-Identifier: GPL-2.0\n").unwrap();
    writeln!(out, "// Generated by gen.rs from the kernel's utf8data.c: its NFDICF trie, for").unwrap();
    writeln!(out, "// Unicode {}. Do not edit.\n",
             versions.iter().map(|v| format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff))
                 .collect::<Vec<_>>().join(", ")).unwrap();

    writeln!(out, "/// The Unicode version each leaf's code point is from, by leaf[0].").unwrap();
    writeln!(out, "pub(super) static AGES: [u32; {}] = [", ages.len()).unwrap();
    for l in ages.chunks(8) {
        writeln!(out, "    {}", l.iter().map(|a| format!("{a:#x},")).collect::<Vec<_>>().join(" ")).unwrap();
    }
    writeln!(out, "];\n").unwrap();

    writeln!(out, "/// Each version's root in UTF8DATA.").unwrap();
    writeln!(out, "pub(super) static ROOTS: [(u32, usize); {}] = [", roots.len()).unwrap();
    for (v, root) in &roots {
        writeln!(out, "    ({v:#x}, {}),", root - base).unwrap();
    }
    writeln!(out, "];\n").unwrap();

    writeln!(out, "pub(super) static UTF8DATA: [u8; {}] = [", kept.len()).unwrap();
    for l in kept.chunks(16) {
        writeln!(out, "    {}", l.iter().map(|b| format!("{b:#04x},")).collect::<Vec<_>>().join(" ")).unwrap();
    }
    writeln!(out, "];").unwrap();

    eprintln!("{} bytes of trie, from offset {base}", kept.len());
    print!("{out}");
}
