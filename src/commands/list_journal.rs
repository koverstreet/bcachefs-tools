use std::ffi::CString;
use std::marker::PhantomData;

use anyhow::{bail, Result};
use bch_bindgen::c;
use bch_bindgen::bkey;
use bch_bindgen::opt_set;
use bch_bindgen::fs::Fs;
use clap::Parser;

use crate::wrappers::printbuf::Printbuf;

// ---- vstruct pointer arithmetic ----

/// jset_entry is 8 bytes; _data is at offset 0 (u64 flexible array).
/// vstruct_next(entry) = (u64*)entry._data + le16(entry.u64s)
unsafe fn vstruct_next_entry(entry: *const c::jset_entry) -> *const c::jset_entry {
    let u64s = u16::from_le((*entry).u64s) as usize;
    // offsetof(jset_entry, _data) = 8 (header is 8 bytes)
    // vstruct_next = (u64*)entry->_data + u64s = entry + 8 + u64s * 8
    (entry as *const u8).add(8 + u64s * 8) as *const c::jset_entry
}

/// Pointer to one past the last entry in a jset.
/// vstruct_last(jset) = &jset.start[0] + jset.u64s * 8
/// (start and _data are at the same offset; last uses start type)
unsafe fn vstruct_last_jset(jset: *const c::jset) -> *const c::jset_entry {
    let u64s = u32::from_le((*jset).u64s) as usize;
    // _data is at offset 56 from jset start (sizeof(jset) = 56)
    // vstruct_last = (u64*)jset._data + __vstruct_u64s(jset)
    (jset as *const u8).add(56 + u64s * 8) as *const c::jset_entry
}

/// Total byte size of a jset including header.
fn jset_vstruct_bytes(jset: &c::jset) -> usize {
    let u64s = u32::from_le(jset.u64s) as usize;
    56 + u64s * 8
}

/// Number of sectors occupied by a jset on disk.
fn jset_vstruct_sectors(jset: &c::jset, block_bits: u16) -> usize {
    let bytes = jset_vstruct_bytes(jset);
    let block_size = 512usize << block_bits;
    ((bytes + block_size - 1) / block_size) * block_size >> 9
}

/// JSET_NO_FLUSH bitfield: bit 5 of le32 flags.
fn jset_no_flush(jset: &c::jset) -> bool {
    (u32::from_le(jset.flags) >> 5) & 1 != 0
}

/// bkey_next: advance past a bkey_i.
/// In C: (u64*)k->_data + k->k.u64s
/// _data is at offset 0 of bkey_i, k.u64s counts u64s from _data.
/// sizeof(bkey_i) = 40, offsetof(_data) = 0.
/// So next = (char*)k + 0 + k.k.u64s * 8 = k + k.k.u64s * 8.
unsafe fn bkey_next_raw(k: *const c::bkey_i) -> *const c::bkey_i {
    let u64s = (*k).k.u64s as usize;
    (k as *const u8).add(u64s * 8) as *const c::bkey_i
}

/// Start position of a bkey (p.offset - size).
fn bkey_start_pos(k: &c::bkey) -> c::bpos {
    c::bpos {
        inode: k.p.inode,
        offset: k.p.offset.wrapping_sub(k.size as u64),
        snapshot: k.p.snapshot,
    }
}

// ---- vstruct iterators ----

/// Iterator over jset_entry references within a jset.
struct JsetEntryIter<'a> {
    cur: *const c::jset_entry,
    end: *const c::jset_entry,
    _phantom: PhantomData<&'a c::jset>,
}

impl<'a> Iterator for JsetEntryIter<'a> {
    type Item = &'a c::jset_entry;
    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }
        let entry = unsafe { &*self.cur };
        let next = unsafe { vstruct_next_entry(self.cur) };
        if next > self.end {
            return None;
        }
        self.cur = next;
        Some(entry)
    }
}

fn jset_entries(jset: &c::jset) -> JsetEntryIter<'_> {
    let start = jset.start.as_ptr();
    let end = unsafe { vstruct_last_jset(jset as *const c::jset) };
    JsetEntryIter { cur: start, end, _phantom: PhantomData }
}

/// Iterator over bkey_i references within a jset_entry.
struct JsetEntryKeyIter<'a> {
    cur: *const c::bkey_i,
    end: *const c::bkey_i,
    _phantom: PhantomData<&'a c::jset_entry>,
}

impl<'a> Iterator for JsetEntryKeyIter<'a> {
    type Item = &'a c::bkey_i;
    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }
        let k = unsafe { &*self.cur };
        if k.k.u64s == 0 {
            return None;
        }
        let next = unsafe { bkey_next_raw(self.cur) };
        if next > self.end {
            return None;
        }
        self.cur = next;
        Some(k)
    }
}

fn jset_entry_keys(entry: &c::jset_entry) -> JsetEntryKeyIter<'_> {
    let start = entry.start.as_ptr();
    let end = unsafe { vstruct_next_entry(entry as *const c::jset_entry) as *const c::bkey_i };
    JsetEntryKeyIter { cur: start, end, _phantom: PhantomData }
}

// ---- jset_entry_log helpers ----

/// Get log message bytes from a jset_entry of type log.
/// Layout: jset_entry header (8 bytes) followed by d[] message bytes.
fn entry_log_msg(entry: &c::jset_entry) -> &[u8] {
    let total = 8 + u16::from_le(entry.u64s) as usize * 8;
    let msg_bytes = total.saturating_sub(8);
    if msg_bytes == 0 {
        return &[];
    }
    let ptr = entry as *const c::jset_entry as *const u8;
    let data = unsafe { std::slice::from_raw_parts(ptr.add(8), msg_bytes) };
    // Trim trailing nulls
    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..len]
}

fn entry_log_str_eq(entry: &c::jset_entry, s: &str) -> bool {
    let msg = entry_log_msg(entry);
    msg.len() >= s.len() && &msg[..s.len()] == s.as_bytes()
}

// ---- entry classification ----

/// Convert entry type byte to the enum. The enum is non-exhaustive so we
/// use unsafe transmute — the C code does the same implicitly.
fn entry_type(entry: &c::jset_entry) -> c::bch_jset_entry_type {
    unsafe { std::mem::transmute(entry.type_ as u32) }
}

fn entry_is_transaction_start(entry: &c::jset_entry) -> bool {
    entry_type(entry) == c::bch_jset_entry_type::BCH_JSET_ENTRY_log
        && entry.level == 0
}

fn entry_is_log_msg(entry: &c::jset_entry) -> bool {
    if !(entry_type(entry) == c::bch_jset_entry_type::BCH_JSET_ENTRY_log
        && entry.level != 0)
    {
        return false;
    }

    // Filter out internal subsystem markers
    if entry_log_str_eq(entry, "rebalance")
        || entry_log_str_eq(entry, "reconcile")
        || entry_log_str_eq(entry, "copygc")
        || entry_log_str_eq(entry, "promote")
    {
        return false;
    }

    true
}

fn entry_is_print_key(entry: &c::jset_entry) -> bool {
    use c::bch_jset_entry_type::*;
    matches!(
        entry_type(entry),
        BCH_JSET_ENTRY_btree_root
            | BCH_JSET_ENTRY_btree_keys
            | BCH_JSET_ENTRY_write_buffer_keys
            | BCH_JSET_ENTRY_overwrite
    )
}

fn entry_is_non_transaction(entry: &c::jset_entry) -> bool {
    use c::bch_jset_entry_type::*;
    matches!(
        entry_type(entry),
        BCH_JSET_ENTRY_btree_root
            | BCH_JSET_ENTRY_datetime
            | BCH_JSET_ENTRY_usage
            | BCH_JSET_ENTRY_clock
    )
}

// ---- filter types ----

struct TransactionMsgFilter {
    sign: i32,
    patterns: Vec<String>,
}

struct TransactionKeyFilter {
    sign: i32,
    ranges: Vec<c::bbpos_range>,
}

struct JournalFilter {
    blacklisted: bool,
    flush_only: bool,
    datetime_only: bool,
    headers_only: bool,
    all_headers: bool,
    log: bool,
    log_only: bool,
    print_offset: bool,
    filtering: bool,
    btree_filter: u64,
    transaction: TransactionMsgFilter,
    key: TransactionKeyFilter,
    bkey_val: bool,
}

// ---- bbpos comparison ----

fn bbpos_cmp(l: &c::bbpos, r: &c::bbpos) -> std::cmp::Ordering {
    (l.btree as u32)
        .cmp(&(r.btree as u32))
        .then_with(|| bkey::bpos_cmp(l.pos, r.pos).cmp(&0))
}

// ---- filter logic ----

fn entry_matches_btree_filter(f: &JournalFilter, entry: &c::jset_entry) -> bool {
    f.btree_filter == !0u64
        || (entry.level == 0
            && entry_type(entry) != c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root
            && (1u64 << entry.btree_id) & f.btree_filter != 0)
}

/// Check if any entry in the transaction (from start+1 to end) matches btree filter.
fn transaction_matches_btree_filter(
    f: &JournalFilter,
    entries: &[&c::jset_entry],
) -> bool {
    entries.iter().skip(1).any(|e| {
        entry_is_print_key(e) && entry_matches_btree_filter(f, e)
    })
}

fn bkey_matches_filter(
    f: &TransactionKeyFilter,
    entry: &c::jset_entry,
    k: &c::bkey_i,
) -> bool {
    for range in &f.ranges {
        let btree: c::btree_id = unsafe { std::mem::transmute(entry.btree_id as u32) };
        let mut k_start = c::bbpos {
            btree,
            pos: bkey_start_pos(&k.k),
        };
        let mut k_end = c::bbpos {
            btree,
            pos: k.k.p,
        };

        if range.start.pos.snapshot == 0 && range.end.pos.snapshot == 0 {
            k_start.pos.snapshot = 0;
            k_end.pos.snapshot = 0;
        }

        // Match the C code: always use point comparison (true || !k.k.size)
        k_start = k_end;

        if bbpos_cmp(&k_start, &range.start) != std::cmp::Ordering::Less
            && bbpos_cmp(&k_end, &range.end) != std::cmp::Ordering::Greater
        {
            return true;
        }
    }
    false
}

fn entry_matches_transaction_filter(
    f: &TransactionKeyFilter,
    entry: &c::jset_entry,
) -> bool {
    if entry.level != 0 {
        return false;
    }
    let t = entry_type(entry);
    if t != c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_keys
        && t != c::bch_jset_entry_type::BCH_JSET_ENTRY_overwrite
    {
        return false;
    }

    for k in jset_entry_keys(entry) {
        if k.k.u64s == 0 {
            break;
        }
        if bkey_matches_filter(f, entry, k) {
            return true;
        }
    }
    false
}

fn transaction_matches_transaction_filter(
    f: &TransactionKeyFilter,
    entries: &[&c::jset_entry],
) -> bool {
    entries
        .iter()
        .skip(1)
        .any(|e| entry_matches_transaction_filter(f, e))
}

fn entry_matches_msg_filter(f: &TransactionMsgFilter, entry: &c::jset_entry) -> bool {
    f.patterns.iter().any(|p| entry_log_str_eq(entry, p))
}

fn entry_is_log_only(entries: &[&c::jset_entry]) -> bool {
    let mut have_log = false;
    for e in entries.iter().skip(1) {
        if e.u64s != 0 && !entry_is_log_msg(e) {
            return false;
        }
        have_log = true;
    }
    have_log
}

fn entry_has_log(entries: &[&c::jset_entry]) -> bool {
    entries.iter().skip(1).any(|e| entry_is_log_msg(e))
}

fn should_print_transaction(
    f: &JournalFilter,
    entries: &[&c::jset_entry],
) -> bool {
    debug_assert!(entry_type(entries[0]) == c::bch_jset_entry_type::BCH_JSET_ENTRY_log);

    if f.log && entry_is_log_only(entries) {
        return true;
    }

    if f.log_only && !entry_has_log(entries) {
        return false;
    }

    if f.btree_filter != !0u64 && !transaction_matches_btree_filter(f, entries) {
        return false;
    }

    if !f.transaction.patterns.is_empty()
        && entry_matches_msg_filter(&f.transaction, entries[0]) != (f.transaction.sign >= 0)
    {
        return false;
    }

    if !f.key.ranges.is_empty()
        && transaction_matches_transaction_filter(&f.key, entries) != (f.key.sign >= 0)
    {
        return false;
    }

    true
}

// ---- printing ----

const NORMAL: &str = "\x1B[0m";
const RED: &str = "\x1B[31m";

fn star_start_of_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.char_indices() {
        if i == 0 && ch == ' ' {
            out.push('*');
        } else if ch == '\n' {
            out.push('\n');
            // peek at next char
        } else if i > 0 && s.as_bytes()[i - 1] == b'\n' && ch == ' ' {
            out.push('*');
        } else {
            out.push(ch);
        }
    }
    out
}

fn journal_entry_header_to_text(
    out: &mut Printbuf,
    c_fs: *mut c::bch_fs,
    p: &c::journal_replay,
    blacklisted: bool,
) {
    use std::fmt::Write;
    write!(
        out,
        "\n{}journal entry     {}\n\
         \x20 bytes           {}\n\
         \x20 sectors         {}\n\
         \x20 version         {}\n\
         \x20 last seq        {}\n\
         \x20 flush           {}\n\
         \x20 written at      ",
        if blacklisted { "blacklisted " } else { "" },
        u64::from_le(p.j.seq),
        jset_vstruct_bytes(&p.j),
        jset_vstruct_sectors(&p.j, unsafe { (*c_fs).block_bits }),
        u32::from_le(p.j.version),
        u64::from_le(p.j.last_seq),
        if jset_no_flush(&p.j) { 0 } else { 1 },
    ).unwrap();

    unsafe {
        c::bch2_journal_ptrs_to_text(out.as_raw(), c_fs, p as *const _ as *mut _);
    }
    out.newline();
}

fn journal_entry_indent(entry: &c::jset_entry) -> u32 {
    if entry_is_transaction_start(entry)
        || entry_type(entry) == c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root
        || entry_type(entry) == c::bch_jset_entry_type::BCH_JSET_ENTRY_datetime
        || entry_type(entry) == c::bch_jset_entry_type::BCH_JSET_ENTRY_usage
    {
        2
    } else {
        4
    }
}

fn journal_entry_keys_noval_to_text(out: &mut Printbuf, entry: &c::jset_entry) {
    for k in jset_entry_keys(entry) {
        if k.k.u64s == 0 {
            break;
        }
        unsafe {
            c::bch2_prt_jset_entry_type(out.as_raw(), entry_type(entry));
        }
        use std::fmt::Write;
        write!(out, ": ").unwrap();
        let btree: c::btree_id = unsafe { std::mem::transmute(entry.btree_id as u32) };
        unsafe {
            c::bch2_btree_id_level_to_text(out.as_raw(), btree, entry.level as u32);
        }
        write!(out, " ").unwrap();
        unsafe {
            c::bch2_bkey_to_text(out.as_raw(), &k.k);
        }
        out.newline();
    }
}

fn print_one_entry(
    out: &mut Printbuf,
    c_fs: *mut c::bch_fs,
    f: &JournalFilter,
    p: &c::journal_replay,
    entry: &c::jset_entry,
) {
    if entry_is_print_key(entry) && entry.u64s == 0 {
        return;
    }

    if entry_is_print_key(entry) && !entry_matches_btree_filter(f, entry) {
        return;
    }

    let highlight = entry_matches_transaction_filter(&f.key, entry);
    if highlight {
        use std::fmt::Write;
        write!(out, "{RED}").unwrap();
    }

    let indent = journal_entry_indent(entry);
    out.indent_add(indent);

    if f.print_offset {
        use std::fmt::Write;
        // Compute offset of entry._data relative to p.j._data
        let entry_data = entry._data.as_ptr() as usize;
        let jset_data = p.j._data.as_ptr() as usize;
        let offset = (entry_data - jset_data) / 8;
        write!(out, "{offset:4} ").unwrap();
    }

    if !f.bkey_val && entry_is_print_key(entry) {
        journal_entry_keys_noval_to_text(out, entry);
    } else {
        unsafe {
            c::bch2_journal_entry_to_text(out.as_raw(), c_fs, entry as *const _ as *mut _);
        }
        out.newline();
    }

    out.indent_sub(indent);

    if highlight {
        use std::fmt::Write;
        write!(out, "{NORMAL}").unwrap();
    }
}

fn journal_replay_print(c_fs: *mut c::bch_fs, f: &JournalFilter, p: &c::journal_replay) {
    let mut buf = Printbuf::new();
    let seq = u64::from_le(p.j.seq);
    let blacklisted = p.ignore_blacklisted
        || unsafe { c::bch2_journal_seq_is_blacklisted(c_fs, seq, false) };
    let mut printed_header = false;

    if f.datetime_only {
        use std::fmt::Write;
        write!(
            &mut buf,
            "{}journal entry     {:<8} ",
            if blacklisted { "blacklisted " } else { "" },
            seq,
        ).unwrap();

        for entry in jset_entries(&p.j) {
            if entry_type(entry) == c::bch_jset_entry_type::BCH_JSET_ENTRY_datetime {
                unsafe {
                    c::bch2_journal_entry_to_text(
                        buf.as_raw(), c_fs, entry as *const _ as *mut _,
                    );
                }
                break;
            }
        }
        buf.newline();

        print_buf(&buf, blacklisted);
        return;
    }

    // Collect all entries for this jset
    let all_entries: Vec<&c::jset_entry> = jset_entries(&p.j).collect();

    if !f.filtering {
        journal_entry_header_to_text(&mut buf, c_fs, p, blacklisted);

        if !f.headers_only {
            for entry in &all_entries {
                print_one_entry(&mut buf, c_fs, f, p, entry);
            }
        }
    } else {
        if f.all_headers {
            journal_entry_header_to_text(&mut buf, c_fs, p, blacklisted);
            printed_header = true;
        }

        // Find transaction boundaries and process
        let mut i = 0;
        while i < all_entries.len() {
            // Skip to next transaction start
            while i < all_entries.len() && !entry_is_transaction_start(all_entries[i]) {
                i += 1;
            }
            if i >= all_entries.len() {
                break;
            }

            // Find transaction end
            let t_start = i;
            i += 1;
            while i < all_entries.len()
                && !entry_is_transaction_start(all_entries[i])
                && !entry_is_non_transaction(all_entries[i])
            {
                i += 1;
            }
            let t_end = i;

            let t_entries = &all_entries[t_start..t_end];
            if should_print_transaction(f, t_entries) {
                if !printed_header {
                    journal_entry_header_to_text(&mut buf, c_fs, p, blacklisted);
                    printed_header = true;
                }

                for entry in t_entries {
                    print_one_entry(&mut buf, c_fs, f, p, entry);
                }
            }
        }
    }

    print_buf(&buf, blacklisted);
}

fn print_buf(buf: &Printbuf, blacklisted: bool) {
    let s = buf.as_str();
    if !s.is_empty() {
        if blacklisted {
            print!("{}", star_start_of_lines(s));
        } else {
            print!("{s}");
        }
    }
}

// ---- seq range parsing ----

fn parse_seq_range(arg: &str) -> (u64, u64) {
    if let Some((start_s, end_s)) = arg.split_once("..") {
        let start = if start_s.is_empty() {
            0
        } else {
            start_s
                .parse::<u64>()
                .unwrap_or_else(|_| die_msg(&format!("error parsing seq range start: {start_s}")))
        };
        let end = if end_s.is_empty() {
            u64::MAX
        } else {
            end_s
                .parse::<u64>()
                .unwrap_or_else(|_| die_msg(&format!("error parsing seq range end: {end_s}")))
        };
        if start > end {
            (end, start)
        } else {
            (start, end)
        }
    } else {
        let seq = arg
            .parse::<u64>()
            .unwrap_or_else(|_| die_msg(&format!("error parsing seq: {arg}")));
        (seq, seq)
    }
}

fn die_msg(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

// ---- sign parsing ----

fn parse_sign(s: &str) -> (i32, &str) {
    if let Some(rest) = s.strip_prefix('+') {
        (1, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (-1, rest)
    } else {
        (0, s)
    }
}

// ---- bool parsing ----

fn parse_bool_val(s: &str) -> bool {
    match s {
        "1" | "true" | "yes" => true,
        "0" | "false" | "no" => false,
        _ => die_msg(&format!("error parsing {s}")),
    }
}

// ---- CLI ----

/// List filesystem journal entries
#[derive(Parser, Debug)]
#[command(name = "list_journal")]
pub struct Cli {
    /// Read entire journal, not just contiguous entries
    #[arg(short = 'a', long)]
    all: bool,

    /// Only read dirty entries
    #[arg(short = 'd', long = "dirty-only")]
    dirty_only: bool,

    /// Number of journal entries to print
    #[arg(short = 'n', long = "nr-entries")]
    nr_entries: Option<u32>,

    /// Sequence number or range (seq or seq..seq)
    #[arg(short = 's', long)]
    seq: Option<String>,

    /// Include blacklisted entries
    #[arg(short = 'B', long)]
    blacklisted: bool,

    /// Only flush entries
    #[arg(short = 'F', long = "flush-only")]
    flush_only: bool,

    /// Datetime entries only
    #[arg(short = 'D', long)]
    datetime: bool,

    /// Headers only
    #[arg(short = 'H', long = "headers-only")]
    headers_only: bool,

    /// Print all headers even if no transactions matched
    #[arg(long = "all-headers")]
    all_headers: bool,

    /// Include log-only entries when filtering
    #[arg(short = 'l', long)]
    log: bool,

    /// Only print transactions containing log messages
    #[arg(short = 'L', long = "log-only")]
    log_only: bool,

    /// Print offset of each subentry
    #[arg(short = 'o', long)]
    offset: bool,

    /// Filter by btree (+/-btree1,btree2)
    #[arg(short = 'b', long, allow_hyphen_values = true)]
    btree: Option<String>,

    /// Filter transactions by function (+/-fn1,fn2)
    #[arg(short = 't', long, allow_hyphen_values = true)]
    transaction: Option<String>,

    /// Filter by key range (+/-bbpos[-bbpos],...)
    #[arg(short = 'k', long, allow_hyphen_values = true)]
    key: Option<String>,

    /// Print bkey values (true/false)
    #[arg(short = 'V', long = "bkey-val")]
    bkey_val: Option<String>,

    /// Verbose mode
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Devices
    #[arg(required = true)]
    devices: Vec<String>,
}

pub fn cmd_list_journal(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, norecovery, 1);
    opt_set!(opts, read_only, 1);
    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(opts, errors, c::bch_error_actions::BCH_ON_ERROR_continue as u8);
    opt_set!(opts, fix_errors, c::fsck_err_opts::FSCK_FIX_yes as u8);
    opt_set!(opts, retain_recovery_info, 1);
    opt_set!(opts, read_journal_only, 1);
    opt_set!(opts, read_entire_journal, 1);

    let mut contiguous_only = true;
    let mut seq_start = 0u64;
    let mut seq_end = u64::MAX;

    if cli.dirty_only {
        opt_set!(opts, read_entire_journal, 0);
    }

    if let Some(nr) = cli.nr_entries {
        if nr == 0 {
            // keep default
        }
        opt_set!(opts, read_entire_journal, 1);
    }

    if let Some(ref seq_arg) = cli.seq {
        let (s, e) = parse_seq_range(seq_arg);
        seq_start = s;
        seq_end = e;
        contiguous_only = false;
        opt_set!(opts, read_entire_journal, 1);
    }

    if cli.all {
        contiguous_only = false;
    }

    if cli.verbose {
        opt_set!(opts, verbose, 1);
    }

    // Build filter
    let mut f = JournalFilter {
        blacklisted: cli.blacklisted,
        flush_only: cli.flush_only,
        datetime_only: cli.datetime,
        headers_only: cli.headers_only,
        all_headers: cli.all_headers,
        log: cli.log,
        log_only: cli.log_only,
        print_offset: cli.offset,
        filtering: false,
        btree_filter: !0u64,
        transaction: TransactionMsgFilter {
            sign: 0,
            patterns: Vec::new(),
        },
        key: TransactionKeyFilter {
            sign: 0,
            ranges: Vec::new(),
        },
        bkey_val: true,
    };

    if cli.log_only {
        f.filtering = true;
    }

    if let Some(ref btree_arg) = cli.btree {
        let (sign, rest) = parse_sign(btree_arg);
        let c_str = CString::new(rest).unwrap();
        f.btree_filter = unsafe {
            c::read_flag_list_or_die(
                c_str.as_ptr() as *mut _,
                c::__bch2_btree_ids.as_ptr(),
                b"btree id\0".as_ptr() as *const _,
            )
        };
        if sign < 0 {
            f.btree_filter = !f.btree_filter;
        }
        f.filtering = true;
    }

    if let Some(ref txn_arg) = cli.transaction {
        let (sign, rest) = parse_sign(txn_arg);
        f.transaction.sign = sign;
        for part in rest.split(',') {
            f.transaction.patterns.push(part.to_string());
        }
        f.filtering = true;
    }

    if let Some(ref key_arg) = cli.key {
        let (sign, rest) = parse_sign(key_arg);
        f.key.sign = sign;
        for part in rest.split(',') {
            let c_str = CString::new(part).unwrap();
            let range = unsafe { c::bbpos_range_parse(c_str.as_ptr() as *mut _) };
            f.key.ranges.push(range);
        }
        f.filtering = true;
    }

    if let Some(ref bkey_val_arg) = cli.bkey_val {
        f.bkey_val = parse_bool_val(bkey_val_arg);
    }

    if cli.devices.is_empty() {
        bail!("Please supply device(s) to open");
    }

    let devs: Vec<std::path::PathBuf> = cli.devices.iter().map(std::path::PathBuf::from).collect();
    let fs = Fs::open(&devs, opts)
        .map_err(|e| anyhow::anyhow!("error opening {}: {}", cli.devices[0], e))?;

    let c_fs = fs.raw;

    // Collect journal entries via C shim
    let je = unsafe { c::rust_collect_journal_entries(c_fs) };
    let entries: &[*mut c::journal_replay] = if je.entries.is_null() || je.nr == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(je.entries, je.nr) }
    };

    // Compute min_seq_to_print for contiguous checking
    let mut min_seq_to_print = 0u64;

    if contiguous_only {
        let mut seq = 0u64;
        for &ep in entries {
            let p = unsafe { &*ep };
            let p_seq = u64::from_le(p.j.seq);

            if seq == 0 {
                seq = p_seq;
            }

            loop {
                let missing = unsafe {
                    c::bch2_journal_entry_missing_range(c_fs, seq, p_seq)
                };
                if missing.start == 0 {
                    break;
                }
                seq = missing.end;
                min_seq_to_print = missing.end;
            }

            seq = p_seq + 1;
        }
    }

    if let Some(nr) = cli.nr_entries {
        // journal.seq isn't set in read_journal_only mode, so compute
        // the max seq from the entries we actually collected
        let max_seq = entries.iter()
            .map(|&ep| unsafe { u64::from_le((*ep).j.seq) })
            .max()
            .unwrap_or(0);
        let computed = (max_seq as i64) - (nr as i64) + 1;
        min_seq_to_print = min_seq_to_print.max(computed.max(0) as u64);
    }

    // Main iteration
    let mut seq = 0u64;
    let last_seq_ondisk = unsafe { (*c_fs).journal.last_seq_ondisk };

    for &ep in entries {
        let p = unsafe { &*ep };
        let p_seq = u64::from_le(p.j.seq);

        if p_seq < min_seq_to_print {
            continue;
        }

        if p_seq < seq_start {
            continue;
        }

        if p_seq > seq_end {
            break;
        }

        if seq == 0 {
            seq = p_seq;
        }

        // Print missing ranges
        loop {
            let missing = unsafe {
                c::bch2_journal_entry_missing_range(c_fs, seq, p_seq)
            };
            if missing.start == 0 {
                break;
            }
            println!(
                "missing {} entries at {}-{}{}",
                missing.end - missing.start,
                missing.start,
                missing.end - 1,
                if missing.end < last_seq_ondisk {
                    " (not dirty)"
                } else {
                    ""
                },
            );
            seq = missing.end;
        }

        seq = p_seq + 1;

        if !f.blacklisted
            && (p.ignore_blacklisted
                || unsafe { c::bch2_journal_seq_is_blacklisted(c_fs, p_seq, false) })
        {
            continue;
        }

        if f.flush_only && jset_no_flush(&p.j) {
            continue;
        }

        journal_replay_print(c_fs, &f, p);
    }

    // Free the collected entries array
    if !je.entries.is_null() {
        unsafe { libc::free(je.entries as *mut _) };
    }

    Ok(())
}
