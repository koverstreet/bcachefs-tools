use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use clap::Parser;

use crate::wrappers::accounting::{self, AccountingEntry, DiskAccountingKind, data_type_is_empty};
use crate::wrappers::handle::{BcachefsHandle, DevUsage};
use bch_bindgen::printbuf::Printbuf;
use crate::wrappers::sysfs::{self, DevInfo, bcachefs_kernel_version};

use c::bch_data_type::*;
use c::disk_accounting_type::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
enum Field {
    Replicas,
    Btree,
    Compression,
    RebalanceWork,
    Devices,
}

#[derive(Parser, Debug)]
#[command(name = "usage", about = "Display detailed filesystem usage", disable_help_flag = true)]
pub struct Cli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Comma-separated list of fields
    #[arg(short = 'f', long = "fields", value_delimiter = ',', value_enum)]
    fields: Vec<Field>,

    /// Print all accounting fields
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Human-readable units
    #[arg(short = 'h', long = "human-readable")]
    human_readable: bool,

    /// Filesystem mountpoints
    #[arg(default_value = ".")]
    mountpoints: Vec<String>,
}

pub fn fs_usage(argv: Vec<String>) -> Result<()> {
    let cli = Cli::try_parse_from(argv)?;

    let fields: Vec<Field> = if cli.all {
        vec![Field::Replicas, Field::Btree, Field::Compression,
             Field::RebalanceWork, Field::Devices]
    } else if cli.fields.is_empty() {
        vec![Field::RebalanceWork]
    } else {
        cli.fields
    };

    for path in &cli.mountpoints {
        let mut out = Printbuf::new();
        out.set_human_readable(cli.human_readable);
        fs_usage_to_text(&mut out, path, &fields)?;
        print!("{}", out);
    }

    Ok(())
}

struct DevContext {
    info: DevInfo,
    usage: DevUsage,
    leaving: u64,
}

fn fs_usage_to_text(out: &mut Printbuf, path: &str, fields: &[Field]) -> Result<()> {
    let handle = BcachefsHandle::open(path)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;

    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path)?;

    fs_usage_v1_to_text(out, &handle, &devs, fields)
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;

    devs_usage_to_text(out, &handle, &devs, fields)?;

    Ok(())
}

fn fs_usage_v1_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: &[Field],
) -> Result<(), errno::Errno> {
    let has = |f: Field| -> bool { fields.contains(&f) };

    let mut accounting_types: u32 =
        (1 << BCH_DISK_ACCOUNTING_replicas as u32) |
        (1 << BCH_DISK_ACCOUNTING_persistent_reserved as u32);

    if has(Field::Compression) {
        accounting_types |= 1 << BCH_DISK_ACCOUNTING_compression as u32;
    }
    if has(Field::Btree) {
        accounting_types |= 1 << BCH_DISK_ACCOUNTING_btree as u32;
    }
    if has(Field::RebalanceWork) {
        let version_reconcile =
            c::bcachefs_metadata_version::bcachefs_metadata_version_reconcile as u64;
        if bcachefs_kernel_version() < version_reconcile {
            accounting_types |= 1 << BCH_DISK_ACCOUNTING_rebalance_work as u32;
        } else {
            accounting_types |= 1 << BCH_DISK_ACCOUNTING_reconcile_work as u32;
            accounting_types |= 1 << BCH_DISK_ACCOUNTING_dev_leaving as u32;
        }
    }

    let result = handle.query_accounting(accounting_types)?;

    // Sort entries by bpos
    let mut sorted: Vec<&AccountingEntry> = result.entries.iter().collect();
    sorted.sort_by(|a, b| a.pos.cmp(&b.pos));

    // Header
    let uuid = uuid::Uuid::from_bytes(handle.uuid());
    writeln!(out, "Filesystem: {}", uuid.hyphenated()).unwrap();

    write!(out, "Size:\t").unwrap();
    out.units_sectors(result.capacity);
    write!(out, "\r\n").unwrap();

    write!(out, "Used:\t").unwrap();
    out.units_sectors(result.used);
    write!(out, "\r\n").unwrap();

    write!(out, "Online reserved:\t").unwrap();
    out.units_sectors(result.online_reserved);
    write!(out, "\r\n").unwrap();

    out.tabstop_align();

    // Replicas summary
    replicas_summary_to_text(out, &sorted, devs);

    // Detailed replicas
    if has(Field::Replicas) {
        write!(out, "\nData type\tRequired/total\tDurability\tDevices\n").unwrap();

        for entry in &sorted {
            match entry.pos.decode() {
                DiskAccountingKind::PersistentReserved { nr_replicas } => {
                    let sectors = entry.counter(0);
                    if sectors == 0 { continue; }
                    write!(out, "reserved:\t1/{}\t[] ", nr_replicas).unwrap();
                    out.units_sectors(sectors);
                    write!(out, "\r\n").unwrap();
                }
                DiskAccountingKind::Replicas { data_type, nr_devs, nr_required, devs: dev_list } => {
                    let sectors = entry.counter(0);
                    if sectors == 0 { continue; }

                    let dev_list = &dev_list[..nr_devs as usize];
                    let dur = replicas_durability(nr_devs, nr_required, dev_list, devs);

                    accounting::prt_data_type(out, data_type);
                    write!(out, ":\t{}/{}\t{}\t[", nr_required, nr_devs, dur.durability).unwrap();

                    prt_dev_list(out, dev_list, devs);
                    write!(out, "]\t").unwrap();

                    out.units_sectors(sectors);
                    write!(out, "\r\n").unwrap();
                }
                _ => {}
            }
        }
        out.tabstop_align();
    }

    // Compression
    if has(Field::Compression) {
        let compr: Vec<_> = sorted.iter()
            .filter(|e| e.pos.accounting_type() == Some(BCH_DISK_ACCOUNTING_compression))
            .collect();
        if !compr.is_empty() {
            write!(out, "\nCompression:\n").unwrap();
            write!(out, "type\tcompressed\runcompressed\raverage extent size\r\n").unwrap();

            for entry in &compr {
                if let DiskAccountingKind::Compression { compression_type } = entry.pos.decode() {
                    accounting::prt_compression_type(out, compression_type);
                    write!(out, "\t").unwrap();

                    let nr_extents = entry.counter(0);
                    let sectors_uncompressed = entry.counter(1);
                    let sectors_compressed = entry.counter(2);

                    out.units_sectors(sectors_compressed);
                    write!(out, "\r").unwrap();
                    out.units_sectors(sectors_uncompressed);
                    write!(out, "\r").unwrap();

                    let avg = if nr_extents > 0 {
                        (sectors_uncompressed << 9) / nr_extents
                    } else { 0 };
                    out.units_u64(avg);
                    write!(out, "\r\n").unwrap();
                }
            }
            out.tabstop_align();
        }
    }

    // Btree usage
    if has(Field::Btree) {
        let btrees: Vec<_> = sorted.iter()
            .filter(|e| e.pos.accounting_type() == Some(BCH_DISK_ACCOUNTING_btree))
            .collect();
        if !btrees.is_empty() {
            write!(out, "\nBtree usage:\n").unwrap();
            for entry in &btrees {
                if let DiskAccountingKind::Btree { id } = entry.pos.decode() {
                    write!(out, "{}:\t", accounting::btree_id_str(id)).unwrap();
                    out.units_sectors(entry.counter(0));
                    write!(out, "\r\n").unwrap();
                }
            }
            out.tabstop_align();
        }
    }

    // Rebalance / reconcile work
    if has(Field::RebalanceWork) {
        let rebalance: Vec<_> = sorted.iter()
            .filter(|e| e.pos.accounting_type() == Some(BCH_DISK_ACCOUNTING_rebalance_work))
            .collect();
        if !rebalance.is_empty() {
            write!(out, "\nPending rebalance work:\n").unwrap();
            for entry in &rebalance {
                out.units_sectors(entry.counter(0));
                out.newline();
            }
        }

        let reconcile: Vec<_> = sorted.iter()
            .filter(|e| e.pos.accounting_type() == Some(BCH_DISK_ACCOUNTING_reconcile_work))
            .collect();
        if !reconcile.is_empty() {
            write!(out, "\nPending reconcile:\tdata\rmetadata\r\n").unwrap();
            for entry in &reconcile {
                if let DiskAccountingKind::ReconcileWork { work_type } = entry.pos.decode() {
                    accounting::prt_reconcile_type(out, work_type);
                    write!(out, ":\t").unwrap();
                    out.units_sectors(entry.counter(0));
                    write!(out, "\r").unwrap();
                    out.units_sectors(entry.counter(1));
                    write!(out, "\r\n").unwrap();
                }
            }
            out.tabstop_align();
        }
    }

    Ok(())
}

// ──────────────────────────── Replicas summary ──────────────────────────────

struct Durability {
    durability: u32,
    degraded: u32,
}

fn replicas_durability(
    nr_devs: u8,
    nr_required: u8,
    dev_list: &[u8],
    devs: &[DevInfo],
) -> Durability {
    let mut durability: u32 = 0;
    let mut degraded: u32 = 0;

    for &dev_idx in dev_list {
        let dev = devs.iter().find(|d| d.idx == dev_idx as u32);
        let dev_durability = dev.map_or(1, |d| d.durability);

        if dev.is_none() {
            degraded += dev_durability;
        }
        durability += dev_durability;
    }

    if nr_required > 1 {
        durability = (nr_devs - nr_required + 1) as u32;
    }

    Durability { durability, degraded }
}

/// Durability x degraded matrix: matrix[durability][degraded] = sectors
type DurabilityMatrix = Vec<Vec<u64>>;

fn durability_matrix_add(matrix: &mut DurabilityMatrix, durability: u32, degraded: u32, sectors: u64) {
    while matrix.len() <= durability as usize {
        matrix.push(Vec::new());
    }
    let row = &mut matrix[durability as usize];
    while row.len() <= degraded as usize {
        row.push(0);
    }
    row[degraded as usize] += sectors;
}

/// Print the degradation header row: "undegraded  -1x  -2x ..."
fn prt_degraded_header(out: &mut Printbuf, max_degraded: usize) {
    write!(out, "\t").unwrap();
    for i in 0..max_degraded {
        if i == 0 {
            write!(out, "undegraded\r").unwrap();
        } else {
            write!(out, "-{}x\r", i).unwrap();
        }
    }
    out.newline();
}

/// Print a row of sector values, right-justified in columns.
fn prt_sector_row(out: &mut Printbuf, values: &[u64]) {
    for &val in values {
        if val != 0 {
            out.units_sectors(val);
        }
        write!(out, "\r").unwrap();
    }
    out.newline();
}

fn durability_matrix_to_text(out: &mut Printbuf, matrix: &DurabilityMatrix) {
    let max_degraded = matrix.iter().map(|r| r.len()).max().unwrap_or(0);
    if max_degraded == 0 { return; }

    prt_degraded_header(out, max_degraded);

    for (dur, row) in matrix.iter().enumerate() {
        if row.is_empty() { continue; }
        write!(out, "{}x:\t", dur).unwrap();
        prt_sector_row(out, row);
    }
    out.tabstop_align();
}

/// EC entries grouped by stripe config: (nr_data, nr_parity) → [degraded] = sectors
struct EcConfig {
    nr_data:    u8,
    nr_parity:  u8,
    degraded:   Vec<u64>,
}

fn ec_config_add(configs: &mut Vec<EcConfig>, nr_required: u8, nr_devs: u8, degraded: u32, sectors: u64) {
    let nr_parity = nr_devs - nr_required;
    let cfg = match configs.iter_mut().find(|c| c.nr_data == nr_required && c.nr_parity == nr_parity) {
        Some(c) => c,
        None => {
            configs.push(EcConfig { nr_data: nr_required, nr_parity, degraded: Vec::new() });
            configs.last_mut().unwrap()
        }
    };
    while cfg.degraded.len() <= degraded as usize {
        cfg.degraded.push(0);
    }
    cfg.degraded[degraded as usize] += sectors;
}

fn ec_configs_to_text(out: &mut Printbuf, configs: &mut [EcConfig]) {
    configs.sort_by_key(|c| (c.nr_data, c.nr_parity));

    let max_degraded = configs.iter().map(|c| c.degraded.len()).max().unwrap_or(0);
    if max_degraded == 0 { return; }

    prt_degraded_header(out, max_degraded);

    for cfg in configs.iter() {
        write!(out, "{}+{}:\t", cfg.nr_data, cfg.nr_parity).unwrap();
        prt_sector_row(out, &cfg.degraded);
    }
    out.tabstop_align();
}

fn replicas_summary_to_text(
    out: &mut Printbuf,
    sorted: &[&AccountingEntry],
    devs: &[DevInfo],
) {
    let mut replicated: DurabilityMatrix = Vec::new();
    let mut ec_configs: Vec<EcConfig> = Vec::new();
    let mut cached: u64 = 0;
    let mut reserved: u64 = 0;

    for entry in sorted {
        match entry.pos.decode() {
            DiskAccountingKind::PersistentReserved { .. } => {
                reserved += entry.counter(0);
            }
            DiskAccountingKind::Replicas { data_type, nr_devs, nr_required, devs: dev_list } => {
                if data_type == BCH_DATA_cached {
                    cached += entry.counter(0);
                    continue;
                }

                let dev_list = &dev_list[..nr_devs as usize];
                let d = replicas_durability(nr_devs, nr_required, dev_list, devs);

                if nr_required > 1 {
                    ec_config_add(&mut ec_configs, nr_required, nr_devs, d.degraded, entry.counter(0));
                } else {
                    durability_matrix_add(&mut replicated, d.durability, d.degraded, entry.counter(0));
                }
            }
            _ => {}
        }
    }

    let has_ec = !ec_configs.is_empty();

    writeln!(out).unwrap();
    if has_ec {
        writeln!(out, "Replicated:").unwrap();
    }
    durability_matrix_to_text(out, &replicated);

    if has_ec {
        write!(out, "\nErasure coded (data+parity):\n").unwrap();
        ec_configs_to_text(out, &mut ec_configs);
    }

    if cached > 0 {
        write!(out, "cached:\t").unwrap();
        out.units_sectors(cached);
        write!(out, "\r\n").unwrap();
    }
    if reserved > 0 {
        write!(out, "reserved:\t").unwrap();
        out.units_sectors(reserved);
        write!(out, "\r\n").unwrap();
    }
    if cached > 0 || reserved > 0 {
        out.tabstop_align();
    }
}

/// Print a device list like [sda sdb sdc].
fn prt_dev_list(out: &mut Printbuf, dev_list: &[u8], devs: &[DevInfo]) {
    for (i, &dev_idx) in dev_list.iter().enumerate() {
        if i > 0 { write!(out, " ").unwrap(); }
        if dev_idx == c::BCH_SB_MEMBER_INVALID as u8 {
            write!(out, "none").unwrap();
        } else if let Some(d) = devs.iter().find(|d| d.idx == dev_idx as u32) {
            write!(out, "{}", d.dev).unwrap();
        } else {
            write!(out, "{}", dev_idx).unwrap();
        }
    }
}

// ──────────────────────────── Device usage ───────────────────────────────────

fn devs_usage_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: &[Field],
) -> Result<()> {
    let has = |f: Field| -> bool { fields.contains(&f) };

    // Query dev_leaving accounting if available
    let dev_leaving_map = match handle.query_accounting(1 << BCH_DISK_ACCOUNTING_dev_leaving as u32) {
        Ok(result) => result.entries,
        Err(_) => Vec::new(),
    };

    let mut dev_ctxs: Vec<DevContext> = Vec::new();
    for dev in devs {
        let usage = handle.dev_usage(dev.idx)
            .map_err(|e| anyhow!("getting usage for device {}: {}", dev.idx, e))?;
        let leaving = dev_leaving_sectors(&dev_leaving_map, dev.idx);
        dev_ctxs.push(DevContext { info: dev.clone(), usage, leaving });
    }

    // Sort by label, then dev name, then idx
    dev_ctxs.sort_by(|a, b| {
        a.info.label.cmp(&b.info.label)
            .then(a.info.dev.cmp(&b.info.dev))
            .then(a.info.idx.cmp(&b.info.idx))
    });

    let has_leaving = dev_ctxs.iter().any(|d| d.leaving != 0);

    out.newline();

    if has(Field::Devices) {
        // Full per-device breakdown
        for d in &dev_ctxs {
            dev_usage_full_to_text(out, d);
        }
    } else {
        // Summary table
        write!(out, "Device label\tDevice\tState\tSize\rUsed\rUse%\r").unwrap();
        if has_leaving {
            write!(out, "Leaving\r").unwrap();
        }
        out.newline();

        for d in &dev_ctxs {
            let capacity = d.usage.capacity_sectors();
            let used = d.usage.used_sectors();
            let label = d.info.label.as_deref().unwrap_or("(no label)");
            let state = accounting::member_state_str(d.usage.state);

            write!(out, "{} (device {}):\t{}\t{}\t", label, d.info.idx, d.info.dev, state).unwrap();

            out.units_sectors(capacity);
            write!(out, "\r").unwrap();
            out.units_sectors(used);

            let pct = if capacity > 0 { used * 100 / capacity } else { 0 };
            write!(out, "\r{:>2}%\r", pct).unwrap();

            if d.leaving > 0 {
                out.units_sectors(d.leaving);
                write!(out, "\r").unwrap();
            }

            out.newline();
        }
        out.tabstop_align();
    }

    Ok(())
}

fn dev_usage_full_to_text(out: &mut Printbuf, d: &DevContext) {
    let u = &d.usage;
    let capacity = u.capacity_sectors();
    let used = u.used_sectors();

    let label = d.info.label.as_deref().unwrap_or("(no label)");
    let state = accounting::member_state_str(u.state);
    let pct = if capacity > 0 { used * 100 / capacity } else { 0 };

    write!(out, "{} (device {}):\t{}\t{}\t{:>2}%\n", label, d.info.idx, d.info.dev, state, pct).unwrap();

    {
        let out = &mut *out.indent(2);
        write!(out, "\tdata\rbuckets\rfragmented\r\n").unwrap();

        for (dt_type, dt) in u.iter_typed() {
            accounting::prt_data_type(out, dt_type);
            write!(out, ":\t").unwrap();

            let sectors = if data_type_is_empty(dt_type) {
                dt.buckets * u.bucket_size as u64
            } else {
                dt.sectors
            };
            out.units_sectors(sectors);

            write!(out, "\r{}\r", dt.buckets).unwrap();

            if dt.fragmented > 0 {
                out.units_sectors(dt.fragmented);
            }
            write!(out, "\r\n").unwrap();
        }

        write!(out, "capacity:\t").unwrap();
        out.units_sectors(capacity);
        write!(out, "\r{}\r\n", u.nr_buckets).unwrap();

        write!(out, "bucket size:\t").unwrap();
        out.units_sectors(u.bucket_size as u64);
        write!(out, "\r\n").unwrap();
    }
    out.tabstop_align();
    out.newline();
}

fn dev_leaving_sectors(entries: &[AccountingEntry], dev_idx: u32) -> u64 {
    entries.iter()
        .find_map(|e| match e.pos.decode() {
            DiskAccountingKind::DevLeaving { dev } if dev == dev_idx => Some(e.counter(0)),
            _ => None,
        })
        .unwrap_or(0)
}
