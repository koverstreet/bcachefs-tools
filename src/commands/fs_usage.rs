use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use clap::Parser;
use serde::{Serialize, Serializer};

use crate::commands::DeviceNameArgs;
use crate::wrappers::accounting::{
    data_type, data_type_is_empty, disk_accounting_type, AccountingEntry, DiskAccountingKind,
};
use crate::wrappers::handle::{BcachefsHandle, DevUsage};
use crate::wrappers::sysfs::{self, bcachefs_kernel_version, DevInfo, DeviceNameMode};
use bcachefs_kernel::opts::{prt_compression_type, prt_data_type, prt_reconcile_type};
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::{btree, metadata_version};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
enum Field {
    Replicas,
    Btree,
    Compression,
    RebalanceWork,
    Devices,
}

impl Field {
    fn as_str(self) -> &'static str {
        match self {
            Field::Replicas => "replicas",
            Field::Btree => "btree",
            Field::Compression => "compression",
            Field::RebalanceWork => "rebalance_work",
            Field::Devices => "devices",
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "usage",
    about = "Display detailed filesystem usage",
    long_about = "Displays filesystem space usage broken down by category. \
Output modes: replicas (data/metadata replication), btree (per-btree \
space), compression (ratios and savings), rebalance_work (pending \
reconcile work), devices (per-device breakdown). Use -f to select \
specific fields, -a for all, -h for human-readable sizes.",
    disable_help_flag = true
)]
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

    #[command(flatten)]
    device_names: DeviceNameArgs,

    /// Print machine-readable JSON
    #[arg(long = "json")]
    json: bool,

    /// Filesystem mountpoints
    #[arg(default_value = ".")]
    mountpoints: Vec<String>,
}

/// Every field is decoded from the accounting ioctl exactly once, into an
/// `FsUsage`. --json is `serde_json::to_string_pretty(&model)`; the
/// human-readable path prints from the same model. There is exactly one
/// place that walks accounting entries or per-device usage.
fn fs_usage(cli: Cli) -> Result<()> {
    let fields: Vec<Field> = if cli.all {
        vec![
            Field::Replicas,
            Field::Btree,
            Field::Compression,
            Field::RebalanceWork,
            Field::Devices,
        ]
    } else if cli.fields.is_empty() {
        vec![Field::RebalanceWork]
    } else {
        cli.fields
    };

    let name_mode = cli.device_names.name_mode();
    let filesystems: Result<Vec<FsUsage>> = cli
        .mountpoints
        .iter()
        .map(|path| fs_usage_collect(path, &fields, name_mode))
        .collect();
    let filesystems = filesystems?;

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&FsUsageRoot { filesystems })?
        );
    } else {
        for fs in &filesystems {
            let mut out = Printbuf::new();
            out.set_human_readable(cli.human_readable);
            fs_usage_to_text(&mut out, fs);
            print!("{}", out);
        }
    }

    Ok(())
}

// ──────────────────────────── Data model ─────────────────────────────────────
//
// Sectors are the single unit tracked internally (the ioctl's native unit,
// and what `Printbuf::units_sectors` already expects). JSON fields carry a
// `_bytes` suffix and convert at serialize time via `ser_bytes`/`ser_bytes32`
// so there is one source number per quantity, not a sectors/bytes pair.

const SECTOR_BYTES: u64 = 512;

fn sectors_to_bytes(sectors: u64) -> u64 {
    sectors.saturating_mul(SECTOR_BYTES)
}

fn ser_bytes<S: Serializer>(sectors: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(sectors_to_bytes(*sectors))
}

fn ser_bytes32<S: Serializer>(sectors: &u32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(sectors_to_bytes(*sectors as u64))
}

#[derive(Serialize)]
struct FsUsageRoot {
    filesystems: Vec<FsUsage>,
}

#[derive(Serialize)]
struct FsUsage {
    mountpoint: String,
    uuid: String,
    fields: Vec<&'static str>,
    #[serde(rename = "capacity_bytes", serialize_with = "ser_bytes")]
    capacity: u64,
    #[serde(rename = "used_bytes", serialize_with = "ser_bytes")]
    used: u64,
    #[serde(rename = "online_reserved_bytes", serialize_with = "ser_bytes")]
    online_reserved: u64,
    replicas_summary: ReplicasSummary,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    replicas: Vec<ReplicaUsage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    persistent_reserved: Vec<PersistentReserved>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    compression: Vec<CompressionUsage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    btree: Vec<BtreeUsage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rebalance_work: Vec<RebalanceEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reconcile_work: Vec<ReconcileWork>,
    devices: Vec<DeviceUsage>,
}

#[derive(Serialize)]
struct ReplicasSummary {
    replicated: Vec<DurabilityUsage>,
    erasure_coded: Vec<EcUsage>,
    #[serde(rename = "cached_bytes", serialize_with = "ser_bytes")]
    cached: u64,
    #[serde(rename = "reserved_bytes", serialize_with = "ser_bytes")]
    reserved: u64,
}

#[derive(Serialize)]
struct DurabilityUsage {
    durability: u32,
    degraded: u32,
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
}

#[derive(Serialize)]
struct EcUsage {
    data: u8,
    parity: u8,
    degraded: u32,
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
}

#[derive(Serialize)]
struct ReplicaUsage {
    data_type: String,
    required: u8,
    replicas: u8,
    durability: u32,
    degraded: u32,
    devices: Vec<String>,
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
}

#[derive(Serialize)]
struct PersistentReserved {
    replicas: u8,
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
}

#[derive(Serialize)]
struct CompressionUsage {
    compression_type: String,
    extents: u64,
    #[serde(rename = "compressed_bytes", serialize_with = "ser_bytes")]
    compressed_sectors: u64,
    #[serde(rename = "uncompressed_bytes", serialize_with = "ser_bytes")]
    uncompressed_sectors: u64,
    average_extent_bytes: u64,
}

#[derive(Serialize)]
struct BtreeUsage {
    btree: String,
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
}

#[derive(Serialize)]
struct ReconcileWork {
    work_type: String,
    #[serde(rename = "data_bytes", serialize_with = "ser_bytes")]
    data_sectors: u64,
    #[serde(rename = "metadata_bytes", serialize_with = "ser_bytes")]
    metadata_sectors: u64,
}

#[derive(Serialize)]
struct RebalanceEntry {
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
}

#[derive(Serialize)]
struct DeviceUsage {
    label: Option<String>,
    device_index: u32,
    device: String,
    state: String,
    #[serde(rename = "capacity_bytes", serialize_with = "ser_bytes")]
    capacity: u64,
    #[serde(rename = "used_bytes", serialize_with = "ser_bytes")]
    used: u64,
    #[serde(rename = "hidden_bytes", serialize_with = "ser_bytes")]
    hidden: u64,
    used_percent: u64,
    #[serde(rename = "leaving_bytes", serialize_with = "ser_bytes")]
    leaving: u64,
    #[serde(rename = "bucket_size_bytes", serialize_with = "ser_bytes32")]
    bucket_size: u32,
    buckets: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_types: Option<Vec<DeviceDataTypeUsage>>,
}

#[derive(Serialize)]
struct DeviceDataTypeUsage {
    data_type: String,
    #[serde(rename = "bytes", serialize_with = "ser_bytes")]
    sectors: u64,
    buckets: u64,
    #[serde(rename = "fragmented_bytes", serialize_with = "ser_bytes")]
    fragmented: u64,
}

// ──────────────────────────── Formatting helpers ─────────────────────────────

fn printbuf_to_string(f: impl FnOnce(&mut Printbuf)) -> String {
    let mut out = Printbuf::new();
    f(&mut out);
    out.to_string()
}

fn data_type_name(t: data_type) -> String {
    printbuf_to_string(|out| prt_data_type(out, t))
}

fn compression_type_name(t: bcachefs_kernel::c::bch_compression_type) -> String {
    printbuf_to_string(|out| prt_compression_type(out, t))
}

fn reconcile_type_name(t: bcachefs_kernel::c::bch_reconcile_accounting_type) -> String {
    printbuf_to_string(|out| prt_reconcile_type(out, t))
}

fn accounting_types_for_fields(fields: &[Field]) -> u32 {
    let has = |f: Field| -> bool { fields.contains(&f) };

    let mut accounting_types: u32 =
        disk_accounting_type::replicas.bit() | disk_accounting_type::persistent_reserved.bit();

    if has(Field::Compression) {
        accounting_types |= disk_accounting_type::compression.bit();
    }
    if has(Field::Btree) {
        accounting_types |= disk_accounting_type::btree.bit();
    }
    if has(Field::RebalanceWork) {
        let version_reconcile = u32::from(metadata_version::reconcile) as u64;
        if bcachefs_kernel_version() < version_reconcile {
            accounting_types |= disk_accounting_type::rebalance_work.bit();
        } else {
            accounting_types |= disk_accounting_type::reconcile_work.bit();
            accounting_types |= disk_accounting_type::dev_leaving.bit();
        }
    }

    accounting_types
}

// ──────────────────────────── Single decode pass ─────────────────────────────

fn fs_usage_collect(path: &str, fields: &[Field], name_mode: DeviceNameMode) -> Result<FsUsage> {
    let handle =
        BcachefsHandle::open(path).map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;

    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path, name_mode)?;

    let result = handle
        .query_accounting(accounting_types_for_fields(fields))
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;

    let mut sorted: Vec<&AccountingEntry> = result.entries.iter().collect();
    sorted.sort_by_key(|a| a.pos);

    let uuid = uuid::Uuid::from_bytes(handle.uuid());
    let devices = collect_dev_contexts(&handle, &devs)?
        .into_iter()
        .map(|d| build_device_usage(d, fields.contains(&Field::Devices)))
        .collect();

    Ok(FsUsage {
        mountpoint: path.to_string(),
        uuid: uuid.hyphenated().to_string(),
        fields: fields.iter().map(|f| f.as_str()).collect(),
        capacity: result.capacity,
        used: result.used,
        online_reserved: result.online_reserved,
        replicas_summary: build_replicas_summary(&sorted, &devs),
        replicas: build_replicas(&sorted, &devs, fields.contains(&Field::Replicas)),
        persistent_reserved: build_persistent_reserved(&sorted, fields.contains(&Field::Replicas)),
        compression: build_compression(&sorted, fields.contains(&Field::Compression)),
        btree: build_btree(&sorted, fields.contains(&Field::Btree)),
        rebalance_work: build_rebalance_work(&sorted, fields.contains(&Field::RebalanceWork)),
        reconcile_work: build_reconcile_work(&sorted, fields.contains(&Field::RebalanceWork)),
        devices,
    })
}

fn dev_list_names(dev_list: &[u8], devs: &[DevInfo]) -> Vec<String> {
    dev_list
        .iter()
        .map(|&dev_idx| {
            if dev_idx == c::BCH_SB_MEMBER_INVALID as u8 {
                "none".to_string()
            } else if let Some(d) = devs.iter().find(|d| d.idx == dev_idx as u32) {
                d.dev.clone()
            } else {
                dev_idx.to_string()
            }
        })
        .collect()
}

fn build_replicas_summary(sorted: &[&AccountingEntry], devs: &[DevInfo]) -> ReplicasSummary {
    let mut replicated: DurabilityMatrix = Vec::new();
    let mut ec_configs: Vec<EcConfig> = Vec::new();
    let mut cached: u64 = 0;
    let mut reserved: u64 = 0;

    for entry in sorted {
        match entry.pos.decode() {
            DiskAccountingKind::PersistentReserved { .. } => {
                reserved += entry.counter(0);
            }
            DiskAccountingKind::Replicas {
                data_type,
                nr_devs,
                nr_required,
                devs: dev_list,
            } => {
                if data_type == data_type::cached {
                    cached += entry.counter(0);
                    continue;
                }

                let dev_list = &dev_list[..nr_devs as usize];
                let d = replicas_durability(nr_devs, nr_required, dev_list, devs);

                if nr_required > 1 {
                    ec_config_add(
                        &mut ec_configs,
                        nr_required,
                        nr_devs,
                        d.degraded,
                        entry.counter(0),
                    );
                } else {
                    durability_matrix_add(
                        &mut replicated,
                        d.durability,
                        d.degraded,
                        entry.counter(0),
                    );
                }
            }
            _ => {}
        }
    }

    let replicated = replicated
        .iter()
        .enumerate()
        .flat_map(|(durability, row)| {
            row.iter()
                .enumerate()
                .filter_map(move |(degraded, &sectors)| {
                    (sectors != 0).then(|| DurabilityUsage {
                        durability: durability as u32,
                        degraded: degraded as u32,
                        sectors,
                    })
                })
        })
        .collect();

    ec_configs.sort_by_key(|c| (c.nr_data, c.nr_parity));
    let erasure_coded = ec_configs
        .iter()
        .flat_map(|cfg| {
            cfg.degraded
                .iter()
                .enumerate()
                .filter_map(move |(degraded, &sectors)| {
                    (sectors != 0).then(|| EcUsage {
                        data: cfg.nr_data,
                        parity: cfg.nr_parity,
                        degraded: degraded as u32,
                        sectors,
                    })
                })
        })
        .collect();

    ReplicasSummary {
        replicated,
        erasure_coded,
        cached,
        reserved,
    }
}

fn build_replicas(
    sorted: &[&AccountingEntry],
    devs: &[DevInfo],
    include: bool,
) -> Vec<ReplicaUsage> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            let DiskAccountingKind::Replicas {
                data_type,
                nr_devs,
                nr_required,
                devs: dev_list,
            } = entry.pos.decode()
            else {
                return None;
            };

            let sectors = entry.counter(0);
            if sectors == 0 {
                return None;
            }

            let dev_list = &dev_list[..nr_devs as usize];
            let dur = replicas_durability(nr_devs, nr_required, dev_list, devs);

            Some(ReplicaUsage {
                data_type: data_type_name(data_type),
                required: nr_required,
                replicas: nr_devs,
                durability: dur.durability,
                degraded: dur.degraded,
                devices: dev_list_names(dev_list, devs),
                sectors,
            })
        })
        .collect()
}

fn build_persistent_reserved(
    sorted: &[&AccountingEntry],
    include: bool,
) -> Vec<PersistentReserved> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            let DiskAccountingKind::PersistentReserved { nr_replicas } = entry.pos.decode() else {
                return None;
            };
            let sectors = entry.counter(0);
            (sectors != 0).then(|| PersistentReserved {
                replicas: nr_replicas,
                sectors,
            })
        })
        .collect()
}

fn build_compression(sorted: &[&AccountingEntry], include: bool) -> Vec<CompressionUsage> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            let DiskAccountingKind::Compression { compression_type } = entry.pos.decode() else {
                return None;
            };

            let extents = entry.counter(0);
            let uncompressed_sectors = entry.counter(1);
            let compressed_sectors = entry.counter(2);
            let average_extent_bytes = if extents > 0 {
                (uncompressed_sectors << 9) / extents
            } else {
                0
            };

            Some(CompressionUsage {
                compression_type: compression_type_name(compression_type),
                extents,
                compressed_sectors,
                uncompressed_sectors,
                average_extent_bytes,
            })
        })
        .collect()
}

fn build_btree(sorted: &[&AccountingEntry], include: bool) -> Vec<BtreeUsage> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            let DiskAccountingKind::Btree { id } = entry.pos.decode() else {
                return None;
            };
            Some(BtreeUsage {
                btree: btree::types::btree_id_str(id).to_string(),
                sectors: entry.counter(0),
            })
        })
        .collect()
}

fn build_rebalance_work(sorted: &[&AccountingEntry], include: bool) -> Vec<RebalanceEntry> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter(|e| e.pos.accounting_type() == Some(disk_accounting_type::rebalance_work))
        .map(|entry| RebalanceEntry {
            sectors: entry.counter(0),
        })
        .collect()
}

fn build_reconcile_work(sorted: &[&AccountingEntry], include: bool) -> Vec<ReconcileWork> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            let DiskAccountingKind::ReconcileWork { work_type } = entry.pos.decode() else {
                return None;
            };
            Some(ReconcileWork {
                work_type: reconcile_type_name(work_type),
                data_sectors: entry.counter(0),
                metadata_sectors: entry.counter(1),
            })
        })
        .collect()
}

// ──────────────────────────── Replicas summary shaping ───────────────────────
//
// These matrices exist only to lay the text table out in columns; the model
// above already carries the decoded (durability, degraded, sectors) tuples,
// so text rendering pivots that flat list back into a grid instead of
// touching accounting entries again.

pub struct Durability {
    pub durability: u32,
    pub degraded: u32,
}

/// How much durability a replicas entry has, and how much of it is gone.
///
/// A device is gone if it isn't in @devs or is in it and offline. Both matter:
/// fs_get_devices() keeps listing a hot-removed device, whose dev-N/block
/// symlink is left dangling, so absence is not the only way to be missing. Its
/// durability still counts towards the total either way - what was lost was
/// lost from something.
pub fn replicas_durability(
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

        if !dev.is_some_and(|d| d.online) {
            degraded += dev_durability;
        }
        durability += dev_durability;
    }

    if nr_required > 1 {
        durability = (nr_devs - nr_required + 1) as u32;
    }

    Durability {
        durability,
        degraded,
    }
}

/// How many more devices this replicas entry can lose before its data becomes
/// unreadable.
///
/// One unit of durability has to survive for the data to be readable at all, so
/// it's what's left over after that: zero means the next device to go takes
/// this data with it, and negative means some of it has already gone. The
/// erasure-coded case needs no special handling - replicas_durability() has
/// already collapsed nr_devs/nr_required into an equivalent durability.
///
/// A filesystem's answer is the minimum over its entries, which is why this is
/// per-entry: the worst-off data decides, not the average.
pub fn replicas_spare_redundancy(
    nr_devs: u8,
    nr_required: u8,
    dev_list: &[u8],
    devs: &[DevInfo],
) -> i32 {
    let d = replicas_durability(nr_devs, nr_required, dev_list, devs);

    d.durability as i32 - d.degraded as i32 - 1
}

/// Durability x degraded matrix: matrix[durability][degraded] = sectors
type DurabilityMatrix = Vec<Vec<u64>>;

fn durability_matrix_add(
    matrix: &mut DurabilityMatrix,
    durability: u32,
    degraded: u32,
    sectors: u64,
) {
    while matrix.len() <= durability as usize {
        matrix.push(Vec::new());
    }
    let row = &mut matrix[durability as usize];
    while row.len() <= degraded as usize {
        row.push(0);
    }
    row[degraded as usize] += sectors;
}

fn matrix_from_flat(items: &[DurabilityUsage]) -> DurabilityMatrix {
    let mut matrix = DurabilityMatrix::new();
    for it in items {
        durability_matrix_add(&mut matrix, it.durability, it.degraded, it.sectors);
    }
    matrix
}

/// EC entries grouped by stripe config: (nr_data, nr_parity) → [degraded] = sectors
struct EcConfig {
    nr_data: u8,
    nr_parity: u8,
    degraded: Vec<u64>,
}

fn ec_config_add(
    configs: &mut Vec<EcConfig>,
    nr_required: u8,
    nr_devs: u8,
    degraded: u32,
    sectors: u64,
) {
    let nr_parity = nr_devs - nr_required;
    let cfg = match configs
        .iter_mut()
        .find(|c| c.nr_data == nr_required && c.nr_parity == nr_parity)
    {
        Some(c) => c,
        None => {
            configs.push(EcConfig {
                nr_data: nr_required,
                nr_parity,
                degraded: Vec::new(),
            });
            configs.last_mut().unwrap()
        }
    };
    while cfg.degraded.len() <= degraded as usize {
        cfg.degraded.push(0);
    }
    cfg.degraded[degraded as usize] += sectors;
}

fn ec_configs_from_flat(items: &[EcUsage]) -> Vec<EcConfig> {
    let mut configs = Vec::new();
    for it in items {
        ec_config_add(
            &mut configs,
            it.data,
            it.data + it.parity,
            it.degraded,
            it.sectors,
        );
    }
    configs.sort_by_key(|c| (c.nr_data, c.nr_parity));
    configs
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
    if max_degraded == 0 {
        return;
    }

    out.aligned(|sub| {
        prt_degraded_header(sub, max_degraded);

        for (dur, row) in matrix.iter().enumerate() {
            if row.is_empty() {
                continue;
            }
            write!(sub, "{}x:\t", dur).unwrap();
            prt_sector_row(sub, row);
        }
    });
}

fn ec_configs_to_text(out: &mut Printbuf, configs: &[EcConfig]) {
    let max_degraded = configs.iter().map(|c| c.degraded.len()).max().unwrap_or(0);
    if max_degraded == 0 {
        return;
    }

    out.aligned(|sub| {
        prt_degraded_header(sub, max_degraded);

        for cfg in configs {
            write!(sub, "{}+{}:\t", cfg.nr_data, cfg.nr_parity).unwrap();
            prt_sector_row(sub, &cfg.degraded);
        }
    });
}

// ──────────────────────────── Device usage ───────────────────────────────────

struct DevContext {
    info: DevInfo,
    usage: Option<DevUsage>,
    leaving: u64,
}

fn dev_leaving_sectors(entries: &[AccountingEntry], dev_idx: u32) -> u64 {
    entries
        .iter()
        .find_map(|e| match e.pos.decode() {
            DiskAccountingKind::DevLeaving { dev } if dev == dev_idx => Some(e.counter(0)),
            _ => None,
        })
        .unwrap_or(0)
}

fn collect_dev_contexts(handle: &BcachefsHandle, devs: &[DevInfo]) -> Result<Vec<DevContext>> {
    // Query dev_leaving accounting if available
    let dev_leaving_map = match handle.query_accounting(disk_accounting_type::dev_leaving.bit()) {
        Ok(result) => result.entries,
        Err(_) => Vec::new(),
    };

    let mut dev_ctxs: Vec<DevContext> = Vec::new();
    for dev in devs {
        let usage = if dev.online {
            Some(
                handle
                    .dev_usage(dev.idx)
                    .map_err(|e| anyhow!("getting usage for device {}: {}", dev.idx, e))?,
            )
        } else {
            None
        };
        let leaving = dev_leaving_sectors(&dev_leaving_map, dev.idx);
        dev_ctxs.push(DevContext {
            info: dev.clone(),
            usage,
            leaving,
        });
    }

    // Sort by label, then dev name, then idx
    dev_ctxs.sort_by(|a, b| {
        a.info
            .label
            .cmp(&b.info.label)
            .then(a.info.dev.cmp(&b.info.dev))
            .then(a.info.idx.cmp(&b.info.idx))
    });

    Ok(dev_ctxs)
}

fn build_device_usage(d: DevContext, include_data_types: bool) -> DeviceUsage {
    let Some(usage) = &d.usage else {
        return DeviceUsage {
            label: d.info.label,
            device_index: d.info.idx,
            device: d.info.dev,
            state: "offline".to_string(),
            capacity: 0,
            used: 0,
            hidden: 0,
            used_percent: 0,
            leaving: d.leaving,
            bucket_size: 0,
            buckets: 0,
            data_types: None,
        };
    };

    let hidden = usage.hidden_sectors();
    let capacity = usage.capacity_sectors() - hidden;
    let used = usage.used_sectors() - hidden;
    let used_percent = if usage.nr_buckets > 0 {
        usage.used_buckets() * 100 / usage.nr_buckets
    } else {
        0
    };
    let data_types = include_data_types.then(|| {
        usage
            .iter_typed()
            .map(|(dt_type, dt)| {
                let sectors = if data_type_is_empty(dt_type) {
                    dt.buckets * usage.bucket_size as u64
                } else {
                    dt.sectors
                };
                DeviceDataTypeUsage {
                    data_type: data_type_name(dt_type),
                    sectors,
                    buckets: dt.buckets,
                    fragmented: dt.fragmented,
                }
            })
            .collect()
    });

    DeviceUsage {
        label: d.info.label,
        device_index: d.info.idx,
        device: d.info.dev,
        state: bcachefs_kernel::sb::members::member_state_str(usage.state).to_string(),
        capacity,
        used,
        hidden,
        used_percent,
        leaving: d.leaving,
        bucket_size: usage.bucket_size,
        buckets: usage.nr_buckets,
        data_types,
    }
}

// ──────────────────────────── Text rendering ──────────────────────────────────
//
// Everything below prints from the already-decoded `FsUsage` model — no
// function here touches an `AccountingEntry` or `DevUsage` directly.

fn fs_usage_to_text(out: &mut Printbuf, fs: &FsUsage) {
    writeln!(out, "Filesystem: {}", fs.uuid).unwrap();

    out.aligned(|sub| {
        write!(sub, "Size:\t").unwrap();
        sub.units_sectors(fs.capacity);
        write!(sub, "\r\n").unwrap();

        write!(sub, "Used:\t").unwrap();
        sub.units_sectors(fs.used);
        write!(sub, "\r\n").unwrap();

        write!(sub, "Online reserved:\t").unwrap();
        sub.units_sectors(fs.online_reserved);
        write!(sub, "\r\n").unwrap();
    });

    replicas_summary_to_text(out, &fs.replicas_summary);

    if fs.fields.contains(&"replicas") {
        replicas_detail_to_text(out, &fs.persistent_reserved, &fs.replicas);
    }

    compression_to_text(out, &fs.compression);
    btree_to_text(out, &fs.btree);
    rebalance_work_to_text(out, &fs.rebalance_work);
    reconcile_work_to_text(out, &fs.reconcile_work);

    devices_to_text(out, &fs.devices, fs.fields.contains(&"devices"));
}

fn replicas_summary_to_text(out: &mut Printbuf, summary: &ReplicasSummary) {
    let matrix = matrix_from_flat(&summary.replicated);
    let ec_configs = ec_configs_from_flat(&summary.erasure_coded);
    let has_ec = !ec_configs.is_empty();

    writeln!(out).unwrap();
    if has_ec {
        writeln!(out, "Replicated:").unwrap();
    }
    durability_matrix_to_text(out, &matrix);

    if has_ec {
        write!(out, "\nErasure coded (data+parity):\n").unwrap();
        ec_configs_to_text(out, &ec_configs);
    }

    if summary.cached > 0 || summary.reserved > 0 {
        out.aligned(|sub| {
            if summary.cached > 0 {
                write!(sub, "cached:\t").unwrap();
                sub.units_sectors(summary.cached);
                write!(sub, "\r\n").unwrap();
            }
            if summary.reserved > 0 {
                write!(sub, "reserved:\t").unwrap();
                sub.units_sectors(summary.reserved);
                write!(sub, "\r\n").unwrap();
            }
        });
    }
}

fn replicas_detail_to_text(
    out: &mut Printbuf,
    persistent_reserved: &[PersistentReserved],
    replicas: &[ReplicaUsage],
) {
    out.aligned(|sub| {
        write!(
            sub,
            "\nData type\tRequired/total\tDurability\tDevices\tUsage\n"
        )
        .unwrap();

        for r in persistent_reserved {
            write!(sub, "reserved:\t1/{}\t\t[]\t ", r.replicas).unwrap();
            sub.units_sectors(r.sectors);
            write!(sub, "\r\n").unwrap();
        }

        for r in replicas {
            write!(
                sub,
                "{}:\t{}/{}\t{}\t[{}]\t",
                r.data_type,
                r.required,
                r.replicas,
                r.durability,
                r.devices.join(" "),
            )
            .unwrap();
            sub.units_sectors(r.sectors);
            write!(sub, "\r\n").unwrap();
        }
    });
}

fn compression_to_text(out: &mut Printbuf, compr: &[CompressionUsage]) {
    if compr.is_empty() {
        return;
    }
    out.aligned(|sub| {
        write!(sub, "\nCompression:\n").unwrap();
        write!(
            sub,
            "type\tcompressed\runcompressed\raverage extent size\r\n"
        )
        .unwrap();

        for c in compr {
            write!(sub, "{}\t", c.compression_type).unwrap();
            sub.units_sectors(c.compressed_sectors);
            write!(sub, "\r").unwrap();
            sub.units_sectors(c.uncompressed_sectors);
            write!(sub, "\r").unwrap();
            sub.units_u64(c.average_extent_bytes);
            write!(sub, "\r\n").unwrap();
        }
    });
}

fn btree_to_text(out: &mut Printbuf, btrees: &[BtreeUsage]) {
    if btrees.is_empty() {
        return;
    }
    out.aligned(|sub| {
        write!(sub, "\nBtree usage:\n").unwrap();
        for b in btrees {
            write!(sub, "{}:\t", b.btree).unwrap();
            sub.units_sectors(b.sectors);
            write!(sub, "\r\n").unwrap();
        }
    });
}

fn rebalance_work_to_text(out: &mut Printbuf, rebalance: &[RebalanceEntry]) {
    if rebalance.is_empty() {
        return;
    }
    write!(out, "\nPending rebalance work:\n").unwrap();
    for r in rebalance {
        out.units_sectors(r.sectors);
        out.newline();
    }
}

fn reconcile_work_to_text(out: &mut Printbuf, reconcile: &[ReconcileWork]) {
    if reconcile.is_empty() {
        return;
    }
    out.aligned(|sub| {
        write!(sub, "\nPending reconcile:\tdata\rmetadata\r\n").unwrap();
        for r in reconcile {
            write!(sub, "{}:\t", r.work_type).unwrap();
            sub.units_sectors(r.data_sectors);
            write!(sub, "\r").unwrap();
            sub.units_sectors(r.metadata_sectors);
            write!(sub, "\r\n").unwrap();
        }
    });
}

fn devices_to_text(out: &mut Printbuf, devices: &[DeviceUsage], detailed: bool) {
    out.newline();

    if detailed {
        for d in devices {
            dev_usage_full_to_text(out, d);
        }
        return;
    }

    let has_leaving = devices.iter().any(|d| d.leaving != 0);

    out.aligned(|sub| {
        write!(sub, "Device label\tDevice\tState\tSize\rUsed\rUse%\r").unwrap();
        if has_leaving {
            write!(sub, "Leaving\r").unwrap();
        }
        sub.newline();

        for d in devices {
            let label = d.label.as_deref().unwrap_or("(no label)");
            write!(
                sub,
                "{} (device {}):\t{}\t",
                label, d.device_index, d.device
            )
            .unwrap();

            if d.state == "offline" {
                write!(sub, "offline\t-\r-\r-\r").unwrap();
                if has_leaving {
                    write!(sub, "\r").unwrap();
                }
                sub.newline();
                continue;
            }

            write!(sub, "{}\t", d.state).unwrap();
            sub.units_sectors(d.capacity);
            write!(sub, "\r").unwrap();
            sub.units_sectors(d.used);
            write!(sub, "\r{:>2}%\r", d.used_percent).unwrap();

            if d.leaving > 0 {
                sub.units_sectors(d.leaving);
                write!(sub, "\r").unwrap();
            }

            sub.newline();
        }
    });
}

fn dev_usage_full_to_text(out: &mut Printbuf, d: &DeviceUsage) {
    let label = d.label.as_deref().unwrap_or("(no label)");
    let Some(data_types) = &d.data_types else {
        out.aligned(|sub| {
            writeln!(
                sub,
                "{} (device {}):\t{}\toffline\tusage unavailable",
                label, d.device_index, d.device
            )
            .unwrap();
        });
        return;
    };

    out.aligned(|sub| {
        writeln!(
            sub,
            "{} (device {}):\t{}\t{}\t{:>2}%",
            label, d.device_index, d.device, d.state, d.used_percent
        )
        .unwrap();

        {
            let sub = &mut *sub.indent(2);
            write!(sub, "\tdata\rbuckets\rfragmented\r\n").unwrap();

            for dt in data_types {
                write!(sub, "{}:\t", dt.data_type).unwrap();
                sub.units_sectors(dt.sectors);
                write!(sub, "\r{}\r", dt.buckets).unwrap();

                if dt.fragmented > 0 {
                    sub.units_sectors(dt.fragmented);
                }
                write!(sub, "\r\n").unwrap();
            }

            write!(sub, "capacity:\t").unwrap();
            sub.units_sectors(d.capacity);
            write!(sub, "\r{}\r\n", d.buckets).unwrap();

            write!(sub, "bucket size:\t").unwrap();
            sub.units_sectors(d.bucket_size as u64);
            write!(sub, "\r\n").unwrap();
        }
    });
    out.newline();
}

pub const CMD: super::CmdDef = typed_cmd!("usage", "Show filesystem disk usage", Cli, fs_usage);
