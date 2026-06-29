use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use clap::Parser;
use serde::Serialize;

use crate::wrappers::accounting::{
    data_type, data_type_is_empty, disk_accounting_type, AccountingEntry, DiskAccountingKind,
};
use crate::wrappers::handle::{BcachefsHandle, DevUsage};
use crate::wrappers::sysfs::{self, bcachefs_kernel_version, DevInfo};
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

    /// Print machine-readable JSON
    #[arg(long = "json")]
    json: bool,

    /// Filesystem mountpoints
    #[arg(default_value = ".")]
    mountpoints: Vec<String>,
}

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

    if cli.json {
        let filesystems: Result<Vec<_>> = cli
            .mountpoints
            .iter()
            .map(|path| fs_usage_to_json(path, &fields))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&FsUsageJsonRoot {
                filesystems: filesystems?,
            })?
        );
    } else {
        for path in &cli.mountpoints {
            let mut out = Printbuf::new();
            out.set_human_readable(cli.human_readable);
            fs_usage_to_text(&mut out, path, &fields)?;
            print!("{}", out);
        }
    }

    Ok(())
}

struct DevContext {
    info: DevInfo,
    usage: DevUsage,
    leaving: u64,
}

const SECTOR_BYTES: u64 = 512;

#[derive(Serialize)]
struct FsUsageJsonRoot {
    filesystems: Vec<FsUsageJson>,
}

#[derive(Serialize)]
struct FsUsageJson {
    mountpoint: String,
    filesystem: String,
    fields: Vec<&'static str>,
    capacity_sectors: u64,
    capacity_bytes: u64,
    used_sectors: u64,
    used_bytes: u64,
    online_reserved_sectors: u64,
    online_reserved_bytes: u64,
    replicas_summary: ReplicasSummaryJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    replicas: Vec<ReplicaUsageJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    persistent_reserved: Vec<PersistentReservedJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    compression: Vec<CompressionUsageJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    btree: Vec<BtreeUsageJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rebalance_work: Vec<SectorsJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reconcile_work: Vec<ReconcileWorkJson>,
    devices: Vec<DeviceUsageJson>,
}

#[derive(Serialize)]
struct ReplicasSummaryJson {
    replicated: Vec<DurabilityUsageJson>,
    erasure_coded: Vec<EcUsageJson>,
    cached_sectors: u64,
    cached_bytes: u64,
    reserved_sectors: u64,
    reserved_bytes: u64,
}

#[derive(Serialize)]
struct DurabilityUsageJson {
    durability: u32,
    degraded: u32,
    sectors: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct EcUsageJson {
    data: u8,
    parity: u8,
    degraded: u32,
    sectors: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct ReplicaUsageJson {
    data_type: String,
    required: u8,
    replicas: u8,
    durability: u32,
    degraded: u32,
    devices: Vec<String>,
    sectors: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct PersistentReservedJson {
    replicas: u8,
    sectors: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct CompressionUsageJson {
    compression_type: String,
    extents: u64,
    compressed_sectors: u64,
    compressed_bytes: u64,
    uncompressed_sectors: u64,
    uncompressed_bytes: u64,
    average_extent_bytes: u64,
}

#[derive(Serialize)]
struct BtreeUsageJson {
    btree: String,
    sectors: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct ReconcileWorkJson {
    work_type: String,
    data_sectors: u64,
    data_bytes: u64,
    metadata_sectors: u64,
    metadata_bytes: u64,
}

#[derive(Serialize)]
struct SectorsJson {
    sectors: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct DeviceUsageJson {
    label: Option<String>,
    device_index: u32,
    device: String,
    state: String,
    capacity_sectors: u64,
    capacity_bytes: u64,
    used_sectors: u64,
    used_bytes: u64,
    hidden_sectors: u64,
    hidden_bytes: u64,
    used_percent: u64,
    leaving_sectors: u64,
    leaving_bytes: u64,
    bucket_size_sectors: u32,
    bucket_size_bytes: u64,
    buckets: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_types: Option<Vec<DeviceDataTypeUsageJson>>,
}

#[derive(Serialize)]
struct DeviceDataTypeUsageJson {
    data_type: String,
    sectors: u64,
    bytes: u64,
    buckets: u64,
    fragmented_sectors: u64,
    fragmented_bytes: u64,
}

fn sectors_to_bytes(sectors: u64) -> u64 {
    sectors.saturating_mul(SECTOR_BYTES)
}

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

fn fs_usage_to_text(out: &mut Printbuf, path: &str, fields: &[Field]) -> Result<()> {
    let handle =
        BcachefsHandle::open(path).map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;

    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path)?;

    fs_usage_v1_to_text(out, &handle, &devs, fields)
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;

    devs_usage_to_text(out, &handle, &devs, fields)?;

    Ok(())
}

fn fs_usage_to_json(path: &str, fields: &[Field]) -> Result<FsUsageJson> {
    let handle =
        BcachefsHandle::open(path).map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;

    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path)?;
    let result = handle
        .query_accounting(accounting_types_for_fields(fields))
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;

    let mut sorted: Vec<&AccountingEntry> = result.entries.iter().collect();
    sorted.sort_by_key(|a| a.pos);

    let uuid = uuid::Uuid::from_bytes(handle.uuid());
    let devices = collect_dev_contexts(&handle, &devs)?
        .into_iter()
        .map(|d| device_usage_to_json(d, fields.contains(&Field::Devices)))
        .collect();

    Ok(FsUsageJson {
        mountpoint: path.to_string(),
        filesystem: uuid.hyphenated().to_string(),
        fields: fields.iter().map(|f| f.as_str()).collect(),
        capacity_sectors: result.capacity,
        capacity_bytes: sectors_to_bytes(result.capacity),
        used_sectors: result.used,
        used_bytes: sectors_to_bytes(result.used),
        online_reserved_sectors: result.online_reserved,
        online_reserved_bytes: sectors_to_bytes(result.online_reserved),
        replicas_summary: replicas_summary_to_json(&sorted, &devs),
        replicas: replicas_to_json(&sorted, &devs, fields.contains(&Field::Replicas)),
        persistent_reserved: persistent_reserved_to_json(
            &sorted,
            fields.contains(&Field::Replicas),
        ),
        compression: compression_to_json(&sorted, fields.contains(&Field::Compression)),
        btree: btree_to_json(&sorted, fields.contains(&Field::Btree)),
        rebalance_work: rebalance_work_to_json(&sorted, fields.contains(&Field::RebalanceWork)),
        reconcile_work: reconcile_work_to_json(&sorted, fields.contains(&Field::RebalanceWork)),
        devices,
    })
}

fn fs_usage_v1_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: &[Field],
) -> Result<(), errno::Errno> {
    let has = |f: Field| -> bool { fields.contains(&f) };
    let result = handle.query_accounting(accounting_types_for_fields(fields))?;

    // Sort entries by bpos
    let mut sorted: Vec<&AccountingEntry> = result.entries.iter().collect();
    sorted.sort_by_key(|a| a.pos);

    // Header
    let uuid = uuid::Uuid::from_bytes(handle.uuid());
    writeln!(out, "Filesystem: {}", uuid.hyphenated()).unwrap();

    out.aligned(|sub| {
        write!(sub, "Size:\t").unwrap();
        sub.units_sectors(result.capacity);
        write!(sub, "\r\n").unwrap();

        write!(sub, "Used:\t").unwrap();
        sub.units_sectors(result.used);
        write!(sub, "\r\n").unwrap();

        write!(sub, "Online reserved:\t").unwrap();
        sub.units_sectors(result.online_reserved);
        write!(sub, "\r\n").unwrap();
    });

    // Replicas summary
    replicas_summary_to_text(out, &sorted, devs);

    // Detailed replicas
    if has(Field::Replicas) {
        out.aligned(|sub| {
            write!(
                sub,
                "\nData type\tRequired/total\tDurability\tDevices\tUsage\n"
            )
            .unwrap();

            for entry in &sorted {
                match entry.pos.decode() {
                    DiskAccountingKind::PersistentReserved { nr_replicas } => {
                        let sectors = entry.counter(0);
                        if sectors == 0 {
                            continue;
                        }
                        write!(sub, "reserved:\t1/{}\t\t[]\t ", nr_replicas).unwrap();
                        sub.units_sectors(sectors);
                        write!(sub, "\r\n").unwrap();
                    }
                    DiskAccountingKind::Replicas {
                        data_type,
                        nr_devs,
                        nr_required,
                        devs: dev_list,
                    } => {
                        let sectors = entry.counter(0);
                        if sectors == 0 {
                            continue;
                        }

                        let dev_list = &dev_list[..nr_devs as usize];
                        let dur = replicas_durability(nr_devs, nr_required, dev_list, devs);

                        prt_data_type(sub, data_type);
                        write!(sub, ":\t{}/{}\t{}\t[", nr_required, nr_devs, dur.durability)
                            .unwrap();

                        prt_dev_list(sub, dev_list, devs);
                        write!(sub, "]\t").unwrap();

                        sub.units_sectors(sectors);
                        write!(sub, "\r\n").unwrap();
                    }
                    _ => {}
                }
            }
        });
    }

    // Compression
    if has(Field::Compression) {
        let compr: Vec<_> = sorted
            .iter()
            .filter(|e| e.pos.accounting_type() == Some(disk_accounting_type::compression))
            .collect();
        if !compr.is_empty() {
            out.aligned(|sub| {
                write!(sub, "\nCompression:\n").unwrap();
                write!(
                    sub,
                    "type\tcompressed\runcompressed\raverage extent size\r\n"
                )
                .unwrap();

                for entry in &compr {
                    if let DiskAccountingKind::Compression { compression_type } = entry.pos.decode()
                    {
                        prt_compression_type(sub, compression_type);
                        write!(sub, "\t").unwrap();

                        let nr_extents = entry.counter(0);
                        let sectors_uncompressed = entry.counter(1);
                        let sectors_compressed = entry.counter(2);

                        sub.units_sectors(sectors_compressed);
                        write!(sub, "\r").unwrap();
                        sub.units_sectors(sectors_uncompressed);
                        write!(sub, "\r").unwrap();

                        let avg = if nr_extents > 0 {
                            (sectors_uncompressed << 9) / nr_extents
                        } else {
                            0
                        };
                        sub.units_u64(avg);
                        write!(sub, "\r\n").unwrap();
                    }
                }
            });
        }
    }

    // Btree usage
    if has(Field::Btree) {
        let btrees: Vec<_> = sorted
            .iter()
            .filter(|e| e.pos.accounting_type() == Some(disk_accounting_type::btree))
            .collect();
        if !btrees.is_empty() {
            out.aligned(|sub| {
                write!(sub, "\nBtree usage:\n").unwrap();
                for entry in &btrees {
                    if let DiskAccountingKind::Btree { id } = entry.pos.decode() {
                        write!(sub, "{}:\t", btree::types::btree_id_str(id)).unwrap();
                        sub.units_sectors(entry.counter(0));
                        write!(sub, "\r\n").unwrap();
                    }
                }
            });
        }
    }

    // Rebalance / reconcile work
    if has(Field::RebalanceWork) {
        let rebalance: Vec<_> = sorted
            .iter()
            .filter(|e| e.pos.accounting_type() == Some(disk_accounting_type::rebalance_work))
            .collect();
        if !rebalance.is_empty() {
            write!(out, "\nPending rebalance work:\n").unwrap();
            for entry in &rebalance {
                out.units_sectors(entry.counter(0));
                out.newline();
            }
        }

        let reconcile: Vec<_> = sorted
            .iter()
            .filter(|e| e.pos.accounting_type() == Some(disk_accounting_type::reconcile_work))
            .collect();
        if !reconcile.is_empty() {
            out.aligned(|sub| {
                write!(sub, "\nPending reconcile:\tdata\rmetadata\r\n").unwrap();
                for entry in &reconcile {
                    if let DiskAccountingKind::ReconcileWork { work_type } = entry.pos.decode() {
                        prt_reconcile_type(sub, work_type);
                        write!(sub, ":\t").unwrap();
                        sub.units_sectors(entry.counter(0));
                        write!(sub, "\r").unwrap();
                        sub.units_sectors(entry.counter(1));
                        write!(sub, "\r\n").unwrap();
                    }
                }
            });
        }
    }

    Ok(())
}

fn dev_list_to_json(dev_list: &[u8], devs: &[DevInfo]) -> Vec<String> {
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

fn replicas_summary_to_json(sorted: &[&AccountingEntry], devs: &[DevInfo]) -> ReplicasSummaryJson {
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
                    (sectors != 0).then(|| DurabilityUsageJson {
                        durability: durability as u32,
                        degraded: degraded as u32,
                        sectors,
                        bytes: sectors_to_bytes(sectors),
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
                    (sectors != 0).then(|| EcUsageJson {
                        data: cfg.nr_data,
                        parity: cfg.nr_parity,
                        degraded: degraded as u32,
                        sectors,
                        bytes: sectors_to_bytes(sectors),
                    })
                })
        })
        .collect();

    ReplicasSummaryJson {
        replicated,
        erasure_coded,
        cached_sectors: cached,
        cached_bytes: sectors_to_bytes(cached),
        reserved_sectors: reserved,
        reserved_bytes: sectors_to_bytes(reserved),
    }
}

fn replicas_to_json(
    sorted: &[&AccountingEntry],
    devs: &[DevInfo],
    include: bool,
) -> Vec<ReplicaUsageJson> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            if let DiskAccountingKind::Replicas {
                data_type,
                nr_devs,
                nr_required,
                devs: dev_list,
            } = entry.pos.decode()
            {
                let sectors = entry.counter(0);
                if sectors == 0 {
                    return None;
                }

                let dev_list = &dev_list[..nr_devs as usize];
                let dur = replicas_durability(nr_devs, nr_required, dev_list, devs);

                Some(ReplicaUsageJson {
                    data_type: data_type_name(data_type),
                    required: nr_required,
                    replicas: nr_devs,
                    durability: dur.durability,
                    degraded: dur.degraded,
                    devices: dev_list_to_json(dev_list, devs),
                    sectors,
                    bytes: sectors_to_bytes(sectors),
                })
            } else {
                None
            }
        })
        .collect()
}

fn persistent_reserved_to_json(
    sorted: &[&AccountingEntry],
    include: bool,
) -> Vec<PersistentReservedJson> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            if let DiskAccountingKind::PersistentReserved { nr_replicas } = entry.pos.decode() {
                let sectors = entry.counter(0);
                (sectors != 0).then(|| PersistentReservedJson {
                    replicas: nr_replicas,
                    sectors,
                    bytes: sectors_to_bytes(sectors),
                })
            } else {
                None
            }
        })
        .collect()
}

fn compression_to_json(sorted: &[&AccountingEntry], include: bool) -> Vec<CompressionUsageJson> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            if let DiskAccountingKind::Compression { compression_type } = entry.pos.decode() {
                let extents = entry.counter(0);
                let uncompressed_sectors = entry.counter(1);
                let compressed_sectors = entry.counter(2);
                let average_extent_bytes = if extents > 0 {
                    (uncompressed_sectors << 9) / extents
                } else {
                    0
                };

                Some(CompressionUsageJson {
                    compression_type: compression_type_name(compression_type),
                    extents,
                    compressed_sectors,
                    compressed_bytes: sectors_to_bytes(compressed_sectors),
                    uncompressed_sectors,
                    uncompressed_bytes: sectors_to_bytes(uncompressed_sectors),
                    average_extent_bytes,
                })
            } else {
                None
            }
        })
        .collect()
}

fn btree_to_json(sorted: &[&AccountingEntry], include: bool) -> Vec<BtreeUsageJson> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            if let DiskAccountingKind::Btree { id } = entry.pos.decode() {
                let sectors = entry.counter(0);
                Some(BtreeUsageJson {
                    btree: btree::types::btree_id_str(id).to_string(),
                    sectors,
                    bytes: sectors_to_bytes(sectors),
                })
            } else {
                None
            }
        })
        .collect()
}

fn rebalance_work_to_json(sorted: &[&AccountingEntry], include: bool) -> Vec<SectorsJson> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter(|e| e.pos.accounting_type() == Some(disk_accounting_type::rebalance_work))
        .map(|entry| {
            let sectors = entry.counter(0);
            SectorsJson {
                sectors,
                bytes: sectors_to_bytes(sectors),
            }
        })
        .collect()
}

fn reconcile_work_to_json(sorted: &[&AccountingEntry], include: bool) -> Vec<ReconcileWorkJson> {
    if !include {
        return Vec::new();
    }

    sorted
        .iter()
        .filter_map(|entry| {
            if let DiskAccountingKind::ReconcileWork { work_type } = entry.pos.decode() {
                let data_sectors = entry.counter(0);
                let metadata_sectors = entry.counter(1);
                Some(ReconcileWorkJson {
                    work_type: reconcile_type_name(work_type),
                    data_sectors,
                    data_bytes: sectors_to_bytes(data_sectors),
                    metadata_sectors,
                    metadata_bytes: sectors_to_bytes(metadata_sectors),
                })
            } else {
                None
            }
        })
        .collect()
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

    Durability {
        durability,
        degraded,
    }
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

fn ec_configs_to_text(out: &mut Printbuf, configs: &mut [EcConfig]) {
    configs.sort_by_key(|c| (c.nr_data, c.nr_parity));

    let max_degraded = configs.iter().map(|c| c.degraded.len()).max().unwrap_or(0);
    if max_degraded == 0 {
        return;
    }

    out.aligned(|sub| {
        prt_degraded_header(sub, max_degraded);

        for cfg in configs.iter() {
            write!(sub, "{}+{}:\t", cfg.nr_data, cfg.nr_parity).unwrap();
            prt_sector_row(sub, &cfg.degraded);
        }
    });
}

fn replicas_summary_to_text(out: &mut Printbuf, sorted: &[&AccountingEntry], devs: &[DevInfo]) {
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

    if cached > 0 || reserved > 0 {
        out.aligned(|sub| {
            if cached > 0 {
                write!(sub, "cached:\t").unwrap();
                sub.units_sectors(cached);
                write!(sub, "\r\n").unwrap();
            }
            if reserved > 0 {
                write!(sub, "reserved:\t").unwrap();
                sub.units_sectors(reserved);
                write!(sub, "\r\n").unwrap();
            }
        });
    }
}

/// Print a device list like [sda sdb sdc].
fn prt_dev_list(out: &mut Printbuf, dev_list: &[u8], devs: &[DevInfo]) {
    for (i, &dev_idx) in dev_list.iter().enumerate() {
        if i > 0 {
            write!(out, " ").unwrap();
        }
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

fn collect_dev_contexts(handle: &BcachefsHandle, devs: &[DevInfo]) -> Result<Vec<DevContext>> {
    // Query dev_leaving accounting if available
    let dev_leaving_map = match handle.query_accounting(disk_accounting_type::dev_leaving.bit()) {
        Ok(result) => result.entries,
        Err(_) => Vec::new(),
    };

    let mut dev_ctxs: Vec<DevContext> = Vec::new();
    for dev in devs {
        let usage = handle
            .dev_usage(dev.idx)
            .map_err(|e| anyhow!("getting usage for device {}: {}", dev.idx, e))?;
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

fn device_usage_to_json(d: DevContext, include_data_types: bool) -> DeviceUsageJson {
    let hidden = d.usage.hidden_sectors();
    let capacity = d.usage.capacity_sectors() - hidden;
    let used = d.usage.used_sectors() - hidden;
    let state = bcachefs_kernel::sb::members::member_state_str(d.usage.state).to_string();
    let pct = if d.usage.nr_buckets > 0 {
        d.usage.used_buckets() * 100 / d.usage.nr_buckets
    } else {
        0
    };
    let data_types = include_data_types.then(|| {
        d.usage
            .iter_typed()
            .map(|(dt_type, dt)| {
                let sectors = if data_type_is_empty(dt_type) {
                    dt.buckets * d.usage.bucket_size as u64
                } else {
                    dt.sectors
                };
                DeviceDataTypeUsageJson {
                    data_type: data_type_name(dt_type),
                    sectors,
                    bytes: sectors_to_bytes(sectors),
                    buckets: dt.buckets,
                    fragmented_sectors: dt.fragmented,
                    fragmented_bytes: sectors_to_bytes(dt.fragmented),
                }
            })
            .collect()
    });

    DeviceUsageJson {
        label: d.info.label,
        device_index: d.info.idx,
        device: d.info.dev,
        state,
        capacity_sectors: capacity,
        capacity_bytes: sectors_to_bytes(capacity),
        used_sectors: used,
        used_bytes: sectors_to_bytes(used),
        hidden_sectors: hidden,
        hidden_bytes: sectors_to_bytes(hidden),
        used_percent: pct,
        leaving_sectors: d.leaving,
        leaving_bytes: sectors_to_bytes(d.leaving),
        bucket_size_sectors: d.usage.bucket_size,
        bucket_size_bytes: sectors_to_bytes(d.usage.bucket_size as u64),
        buckets: d.usage.nr_buckets,
        data_types,
    }
}

fn devs_usage_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: &[Field],
) -> Result<()> {
    let has = |f: Field| -> bool { fields.contains(&f) };
    let dev_ctxs = collect_dev_contexts(handle, devs)?;

    let has_leaving = dev_ctxs.iter().any(|d| d.leaving != 0);

    out.newline();

    if has(Field::Devices) {
        // Full per-device breakdown
        for d in &dev_ctxs {
            dev_usage_full_to_text(out, d);
        }
    } else {
        // Summary table
        out.aligned(|sub| {
            write!(sub, "Device label\tDevice\tState\tSize\rUsed\rUse%\r").unwrap();
            if has_leaving {
                write!(sub, "Leaving\r").unwrap();
            }
            sub.newline();

            for d in &dev_ctxs {
                let hidden = d.usage.hidden_sectors();
                let capacity = d.usage.capacity_sectors() - hidden;
                let used = d.usage.used_sectors() - hidden;
                let label = d.info.label.as_deref().unwrap_or("(no label)");
                let state = bcachefs_kernel::sb::members::member_state_str(d.usage.state);

                write!(
                    sub,
                    "{} (device {}):\t{}\t{}\t",
                    label, d.info.idx, d.info.dev, state
                )
                .unwrap();

                sub.units_sectors(capacity);
                write!(sub, "\r").unwrap();
                sub.units_sectors(used);

                let pct = if d.usage.nr_buckets > 0 {
                    d.usage.used_buckets() * 100 / d.usage.nr_buckets
                } else {
                    0
                };
                write!(sub, "\r{:>2}%\r", pct).unwrap();

                if d.leaving > 0 {
                    sub.units_sectors(d.leaving);
                    write!(sub, "\r").unwrap();
                }

                sub.newline();
            }
        });
    }

    Ok(())
}

fn dev_usage_full_to_text(out: &mut Printbuf, d: &DevContext) {
    let u = &d.usage;

    let label = d.info.label.as_deref().unwrap_or("(no label)");
    let state = bcachefs_kernel::sb::members::member_state_str(u.state);
    let pct = if u.nr_buckets > 0 {
        u.used_buckets() * 100 / u.nr_buckets
    } else {
        0
    };

    out.aligned(|sub| {
        writeln!(
            sub,
            "{} (device {}):\t{}\t{}\t{:>2}%",
            label, d.info.idx, d.info.dev, state, pct
        )
        .unwrap();

        {
            let sub = &mut *sub.indent(2);
            write!(sub, "\tdata\rbuckets\rfragmented\r\n").unwrap();

            for (dt_type, dt) in u.iter_typed() {
                prt_data_type(sub, dt_type);
                write!(sub, ":\t").unwrap();

                let sectors = if data_type_is_empty(dt_type) {
                    dt.buckets * u.bucket_size as u64
                } else {
                    dt.sectors
                };
                sub.units_sectors(sectors);

                write!(sub, "\r{}\r", dt.buckets).unwrap();

                if dt.fragmented > 0 {
                    sub.units_sectors(dt.fragmented);
                }
                write!(sub, "\r\n").unwrap();
            }

            write!(sub, "capacity:\t").unwrap();
            sub.units_sectors(u.capacity_sectors());
            write!(sub, "\r{}\r\n", u.nr_buckets).unwrap();

            write!(sub, "bucket size:\t").unwrap();
            sub.units_sectors(u.bucket_size as u64);
            write!(sub, "\r\n").unwrap();
        }
    });
    out.newline();
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

pub const CMD: super::CmdDef = typed_cmd!("usage", "Show filesystem disk usage", Cli, fs_usage);
