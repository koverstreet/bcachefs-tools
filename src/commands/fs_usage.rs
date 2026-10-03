use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use clap::Parser;
use serde::Serialize;

use crate::commands::DeviceNameArgs;
use crate::wrappers::accounting::{
    data_type, data_type_is_empty, disk_accounting_type, AccountingEntry, DiskAccountingKind,
};
use crate::wrappers::handle::BcachefsHandle;
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
    if cli.json {
        let filesystems = cli
            .mountpoints
            .iter()
            .map(|path| fs_usage_collect(path, &fields, name_mode))
            .collect::<Result<Vec<_>>>()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&FsUsageRoot { filesystems })?
        );
    } else {
        for path in &cli.mountpoints {
            let fs = fs_usage_collect(path, &fields, name_mode)?;
            let mut out = Printbuf::new();
            out.set_human_readable(cli.human_readable);
            fs_usage_to_text(&mut out, &fs, &fields);
            print!("{}", out);
        }
    }

    Ok(())
}

const SECTOR_BYTES: u64 = 512;

fn bytes(sectors: u64) -> Result<u64> {
    sectors
        .checked_mul(SECTOR_BYTES)
        .ok_or_else(|| anyhow!("sector count exceeds the u64 byte range"))
}

fn add_bytes(total: &mut u64, amount: u64) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| anyhow!("accounting sum exceeds the u64 byte range"))?;
    Ok(())
}

#[derive(Serialize)]
struct FsUsageRoot {
    filesystems: Vec<FsUsage>,
}

#[derive(Default, Serialize)]
struct FsUsage {
    mountpoint: String,
    uuid: String,
    capacity_bytes: u64,
    used_bytes: u64,
    online_reserved_bytes: u64,
    free_bytes: Vec<u64>,
    free_now_bytes: Vec<u64>,
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

#[derive(Default, Serialize)]
struct ReplicasSummary {
    replicated: Vec<DurabilityUsage>,
    erasure_coded: Vec<EcUsage>,
    cached_bytes: u64,
    reserved_bytes: u64,
}

#[derive(Serialize)]
struct DurabilityUsage {
    durability: u32,
    degraded: u32,
    bytes: u64,
}

#[derive(Serialize)]
struct EcUsage {
    data: u8,
    parity: u8,
    degraded: u32,
    bytes: u64,
}

#[derive(Serialize)]
struct ReplicaUsage {
    data_type: String,
    required: u8,
    replicas: u8,
    durability: u32,
    degraded: u32,
    devices: Vec<String>,
    bytes: u64,
}

#[derive(Serialize)]
struct PersistentReserved {
    replicas: u8,
    bytes: u64,
}

#[derive(Serialize)]
struct CompressionUsage {
    compression_type: String,
    extents: u64,
    compressed_bytes: u64,
    uncompressed_bytes: u64,
    average_extent_bytes: u64,
}

#[derive(Serialize)]
struct BtreeUsage {
    btree: String,
    bytes: u64,
}

#[derive(Serialize)]
struct ReconcileWork {
    work_type: String,
    data_bytes: u64,
    metadata_bytes: u64,
}

#[derive(Serialize)]
struct RebalanceEntry {
    bytes: u64,
}

#[derive(Default, Serialize)]
struct DeviceUsage {
    label: Option<String>,
    device_index: u32,
    device: String,
    state: String,
    capacity_bytes: u64,
    used_bytes: u64,
    hidden_bytes: u64,
    used_percent: u64,
    leaving_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stripe_empty: Option<u64>,
    bucket_size_bytes: u64,
    buckets: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_types: Option<Vec<DeviceDataTypeUsage>>,
}

#[derive(Serialize)]
struct DeviceDataTypeUsage {
    data_type: String,
    #[serde(skip)]
    is_stripe: bool,
    bytes: u64,
    buckets: u64,
    fragmented_bytes: u64,
}

fn printbuf_to_string(f: impl FnOnce(&mut Printbuf)) -> String {
    let mut out = Printbuf::new();
    f(&mut out);
    out.to_string()
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
    let devs = sysfs::fs_get_devices(&sysfs::sysfs_path_from_fd(handle.sysfs_fd())?, name_mode)?;
    let types = accounting_types_for_fields(fields);
    let mut result = handle
        .query_accounting(types)
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;
    for extra in [
        disk_accounting_type::dev_leaving,
        disk_accounting_type::dev_stripe_frag,
    ] {
        if types & extra.bit() == 0 {
            if let Ok(extra) = handle.query_accounting(extra.bit()) {
                result.entries.extend(extra.entries);
            }
        }
    }
    result.entries.sort_by_key(|entry| entry.pos);
    let mut usage = FsUsage {
        mountpoint: path.to_string(),
        uuid: uuid::Uuid::from_bytes(handle.uuid())
            .hyphenated()
            .to_string(),
        capacity_bytes: bytes(result.capacity)?,
        used_bytes: bytes(result.used)?,
        online_reserved_bytes: bytes(result.online_reserved)?,
        free_bytes: result.free.into_iter().map(bytes).collect::<Result<_>>()?,
        free_now_bytes: result
            .free_now
            .into_iter()
            .map(bytes)
            .collect::<Result<_>>()?,
        devices: collect_devices(&handle, &devs, fields.contains(&Field::Devices))?,
        ..FsUsage::default()
    };
    collect_accounting(&mut usage, &result.entries, &devs, fields)?;
    Ok(usage)
}

fn collect_accounting(
    usage: &mut FsUsage,
    entries: &[AccountingEntry],
    devs: &[DevInfo],
    fields: &[Field],
) -> Result<()> {
    let has = |field| fields.contains(&field);
    for entry in entries {
        match entry.pos.decode() {
            DiskAccountingKind::PersistentReserved { nr_replicas } => {
                let count = bytes(entry.counter(0))?;
                add_bytes(&mut usage.replicas_summary.reserved_bytes, count)?;
                if has(Field::Replicas) && count != 0 {
                    usage.persistent_reserved.push(PersistentReserved {
                        replicas: nr_replicas,
                        bytes: count,
                    });
                }
            }
            DiskAccountingKind::Replicas {
                data_type,
                nr_devs,
                nr_required,
                devs: dev_list,
            } => {
                let count = bytes(entry.counter(0))?;
                let devices = &dev_list[..nr_devs as usize];
                let durability = replicas_durability(nr_devs, nr_required, devices, devs);
                let summary = &mut usage.replicas_summary;
                if data_type == data_type::cached {
                    add_bytes(&mut summary.cached_bytes, count)?;
                } else if nr_required > 1 {
                    let parity = nr_devs - nr_required;
                    if let Some(row) = summary.erasure_coded.iter_mut().find(|row| {
                        (row.data, row.parity, row.degraded)
                            == (nr_required, parity, durability.degraded)
                    }) {
                        add_bytes(&mut row.bytes, count)?;
                    } else {
                        summary.erasure_coded.push(EcUsage {
                            data: nr_required,
                            parity,
                            degraded: durability.degraded,
                            bytes: count,
                        });
                    }
                } else {
                    if let Some(row) = summary.replicated.iter_mut().find(|row| {
                        (row.durability, row.degraded)
                            == (durability.durability, durability.degraded)
                    }) {
                        add_bytes(&mut row.bytes, count)?;
                    } else {
                        summary.replicated.push(DurabilityUsage {
                            durability: durability.durability,
                            degraded: durability.degraded,
                            bytes: count,
                        });
                    }
                }
                if has(Field::Replicas) && count != 0 {
                    usage.replicas.push(ReplicaUsage {
                        data_type: printbuf_to_string(|out| prt_data_type(out, data_type)),
                        required: nr_required,
                        replicas: nr_devs,
                        durability: durability.durability,
                        degraded: durability.degraded,
                        devices: dev_list_names(devices, devs),
                        bytes: count,
                    });
                }
            }
            DiskAccountingKind::Compression { compression_type } if has(Field::Compression) => {
                let extents = entry.counter(0);
                let uncompressed_bytes = bytes(entry.counter(1))?;
                usage.compression.push(CompressionUsage {
                    compression_type: printbuf_to_string(|out| {
                        prt_compression_type(out, compression_type)
                    }),
                    extents,
                    compressed_bytes: bytes(entry.counter(2))?,
                    uncompressed_bytes,
                    average_extent_bytes: uncompressed_bytes.checked_div(extents).unwrap_or(0),
                });
            }
            DiskAccountingKind::Btree { id } if has(Field::Btree) => usage.btree.push(BtreeUsage {
                btree: btree::types::btree_id_str(id).to_string(),
                bytes: bytes(entry.counter(0))?,
            }),
            DiskAccountingKind::RebalanceWork if has(Field::RebalanceWork) => {
                usage.rebalance_work.push(RebalanceEntry {
                    bytes: bytes(entry.counter(0))?,
                })
            }
            DiskAccountingKind::ReconcileWork { work_type } if has(Field::RebalanceWork) => {
                usage.reconcile_work.push(ReconcileWork {
                    work_type: printbuf_to_string(|out| prt_reconcile_type(out, work_type)),
                    data_bytes: bytes(entry.counter(0))?,
                    metadata_bytes: bytes(entry.counter(1))?,
                })
            }
            DiskAccountingKind::DevLeaving { dev } => {
                if let Some(device) = usage
                    .devices
                    .iter_mut()
                    .find(|device| device.device_index == dev)
                {
                    device.leaving_bytes = bytes(entry.counter(0))?;
                }
            }
            DiskAccountingKind::DevStripeFrag { dev } => {
                if let Some(device) = usage
                    .devices
                    .iter_mut()
                    .find(|device| device.device_index == u32::from(dev))
                {
                    device.stripe_empty = Some(bytes(entry.counter(1))?);
                }
            }
            _ => {}
        }
    }
    usage
        .replicas_summary
        .replicated
        .sort_by_key(|row| (row.durability, row.degraded));
    usage
        .replicas_summary
        .erasure_coded
        .sort_by_key(|row| (row.data, row.parity, row.degraded));
    Ok(())
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

fn prt_degraded_row(out: &mut Printbuf, rows: impl Iterator<Item = (u32, u64)>) {
    let mut column = 0;
    for (degraded, bytes) in rows {
        while column < degraded {
            out.tab_rjust();
            column += 1;
        }
        if bytes != 0 {
            out.units_u64(bytes);
        }
        out.tab_rjust();
        column += 1;
    }
    out.newline();
}

fn collect_devices(
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    detailed: bool,
) -> Result<Vec<DeviceUsage>> {
    let mut devices = Vec::new();
    for dev in devs {
        let mut device = DeviceUsage {
            label: dev.label.clone(),
            device_index: dev.idx,
            device: dev.dev.clone(),
            state: "offline".to_string(),
            ..DeviceUsage::default()
        };
        if dev.online {
            let usage = handle
                .dev_usage(dev.idx)
                .map_err(|e| anyhow!("getting usage for device {}: {}", dev.idx, e))?;
            device.state = bcachefs_kernel::sb::members::member_state_str(usage.state).to_string();
            device.capacity_bytes = bytes(usage.capacity_sectors())?;
            device.hidden_bytes = bytes(usage.hidden_sectors())?;
            device.used_bytes = bytes(usage.used_sectors() - usage.hidden_sectors())?;
            device.used_percent = if usage.nr_buckets > 0 {
                usage.used_buckets() * 100 / usage.nr_buckets
            } else {
                0
            };
            device.bucket_size_bytes = bytes(u64::from(usage.bucket_size))?;
            device.buckets = usage.nr_buckets;
            if detailed {
                device.data_types = Some(
                    usage
                        .iter_typed()
                        .map(|(data_type, row)| {
                            Ok(DeviceDataTypeUsage {
                                data_type: printbuf_to_string(|out| prt_data_type(out, data_type)),
                                is_stripe: data_type == data_type::stripe,
                                bytes: bytes(if data_type_is_empty(data_type) {
                                    row.buckets * u64::from(usage.bucket_size)
                                } else {
                                    row.sectors
                                })?,
                                buckets: row.buckets,
                                fragmented_bytes: bytes(row.fragmented)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                );
            }
        }
        devices.push(device);
    }
    devices.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then(left.device.cmp(&right.device))
            .then(left.device_index.cmp(&right.device_index))
    });
    Ok(devices)
}

fn fs_usage_to_text(out: &mut Printbuf, fs: &FsUsage, fields: &[Field]) {
    writeln!(out, "Filesystem: {}", fs.uuid).unwrap();

    out.aligned(|sub| {
        write!(sub, "Size:\t").unwrap();
        sub.units_u64(fs.capacity_bytes);
        write!(sub, "\r\n").unwrap();

        write!(sub, "Used:\t").unwrap();
        sub.units_u64(fs.used_bytes);
        write!(sub, "\r\n").unwrap();

        write!(sub, "Online reserved:\t").unwrap();
        sub.units_u64(fs.online_reserved_bytes);
        write!(sub, "\r\n").unwrap();

        for (i, free) in fs.free_bytes.iter().enumerate() {
            if i > 0 && *free == 0 && fs.free_bytes[i - 1] == 0 {
                continue;
            }

            if i == 0 {
                write!(sub, "Free:\t").unwrap();
            } else {
                write!(sub, "  at {} replicas:\t", i + 1).unwrap();
            }

            sub.units_u64(*free);
            write!(sub, "\r").unwrap();
            if let Some(free_now) = fs.free_now_bytes.get(i) {
                sub.units_u64(*free_now);
            }
            write!(sub, "\r").unwrap();

            if i == 0 {
                write!(sub, "writable now").unwrap();
            }

            write!(sub, "\n").unwrap();
        }
    });

    replicas_summary_to_text(out, &fs.replicas_summary);

    if fields.contains(&Field::Replicas) {
        replicas_detail_to_text(out, &fs.persistent_reserved, &fs.replicas);
    }

    compression_to_text(out, &fs.compression);
    btree_to_text(out, &fs.btree);
    rebalance_work_to_text(out, &fs.rebalance_work);
    reconcile_work_to_text(out, &fs.reconcile_work);

    devices_to_text(out, &fs.devices, fields.contains(&Field::Devices));
}

fn replicas_summary_to_text(out: &mut Printbuf, summary: &ReplicasSummary) {
    let has_ec = !summary.erasure_coded.is_empty();
    writeln!(out).unwrap();
    if has_ec {
        writeln!(out, "Replicated:").unwrap();
    }
    let columns = summary
        .replicated
        .iter()
        .map(|row| row.degraded + 1)
        .max()
        .unwrap_or(0);
    if columns != 0 {
        out.aligned(|sub| {
            prt_degraded_header(sub, columns as usize);
            for rows in summary
                .replicated
                .chunk_by(|left, right| left.durability == right.durability)
            {
                write!(sub, "{}x:\t", rows[0].durability).unwrap();
                prt_degraded_row(sub, rows.iter().map(|row| (row.degraded, row.bytes)));
            }
        });
    }
    if has_ec {
        write!(out, "\nErasure coded (data+parity):\n").unwrap();
        out.aligned(|sub| {
            prt_degraded_header(
                sub,
                summary
                    .erasure_coded
                    .iter()
                    .map(|row| row.degraded as usize + 1)
                    .max()
                    .unwrap(),
            );
            for rows in summary
                .erasure_coded
                .chunk_by(|left, right| (left.data, left.parity) == (right.data, right.parity))
            {
                write!(sub, "{}+{}:\t", rows[0].data, rows[0].parity).unwrap();
                prt_degraded_row(sub, rows.iter().map(|row| (row.degraded, row.bytes)));
            }
        });
    }
    if summary.cached_bytes > 0 || summary.reserved_bytes > 0 {
        out.aligned(|sub| {
            for (name, count) in [
                ("cached", summary.cached_bytes),
                ("reserved", summary.reserved_bytes),
            ] {
                if count > 0 {
                    write!(sub, "{}:\t", name).unwrap();
                    sub.units_u64(count);
                    write!(sub, "\r\n").unwrap();
                }
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
            sub.units_u64(r.bytes);
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
            sub.units_u64(r.bytes);
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
            sub.units_u64(c.compressed_bytes);
            write!(sub, "\r").unwrap();
            sub.units_u64(c.uncompressed_bytes);
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
            sub.units_u64(b.bytes);
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
        out.units_u64(r.bytes);
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
            sub.units_u64(r.data_bytes);
            write!(sub, "\r").unwrap();
            sub.units_u64(r.metadata_bytes);
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

    let has_leaving = devices.iter().any(|d| d.leaving_bytes != 0);

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
            sub.units_u64(d.capacity_bytes.saturating_sub(d.hidden_bytes));
            write!(sub, "\r").unwrap();
            sub.units_u64(d.used_bytes);
            write!(sub, "\r{:>2}%\r", d.used_percent).unwrap();

            if d.leaving_bytes > 0 {
                sub.units_u64(d.leaving_bytes);
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

            // Zero included: no reusable space is a different answer from
            // don't know, and it's the one to read next to `fragmented`.
            let show_empty = d.stripe_empty.is_some();

            write!(sub, "\tdata\rbuckets\rfragmented").unwrap();
            if show_empty {
                write!(sub, "\rempty").unwrap();
            }
            write!(sub, "\r\n").unwrap();

            for dt in data_types {
                write!(sub, "{}:\t", dt.data_type).unwrap();
                sub.units_u64(dt.bytes);
                write!(sub, "\r{}\r", dt.buckets).unwrap();

                if dt.fragmented_bytes > 0 {
                    sub.units_u64(dt.fragmented_bytes);
                }

                if let Some(empty) = d.stripe_empty {
                    write!(sub, "\r").unwrap();
                    if dt.is_stripe {
                        sub.units_u64(empty);
                    }
                }
                write!(sub, "\r\n").unwrap();
            }

            write!(sub, "capacity:\t").unwrap();
            sub.units_u64(d.capacity_bytes);
            write!(sub, "\r{}\r\n", d.buckets).unwrap();

            write!(sub, "bucket size:\t").unwrap();
            sub.units_u64(d.bucket_size_bytes as u64);
            write!(sub, "\r\n").unwrap();
        }
    });
    out.newline();
}

pub const CMD: super::CmdDef = typed_cmd!("usage", "Show filesystem disk usage", Cli, fs_usage);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degraded_row_preserves_missing_and_zero_columns() {
        let mut out = Printbuf::new();
        prt_degraded_row(
            &mut out,
            [(1, 7 * SECTOR_BYTES), (3, 0), (4, 11 * SECTOR_BYTES)].into_iter(),
        );
        assert_eq!(out.as_str(), "\r3584\r\r\r5632\r\n");
    }

    #[test]
    fn accounting_aggregates_replicas_and_retains_device_counters() {
        let devs = vec![DevInfo {
            idx: 1,
            dev: "sdx".to_string(),
            label: None,
            failure_domain: None,
            durability: 1,
            online: true,
        }];
        let mut usage = FsUsage {
            devices: vec![fixture_device()],
            ..FsUsage::default()
        };
        let mut members = [0; std::mem::size_of::<c::bpos>()];
        members[..3].copy_from_slice(&[1, 2, 3]);
        let entries = [
            (
                DiskAccountingKind::Replicas {
                    data_type: data_type::user,
                    nr_devs: 2,
                    nr_required: 1,
                    devs: members,
                },
                vec![7],
            ),
            (
                DiskAccountingKind::Replicas {
                    data_type: data_type::btree,
                    nr_devs: 2,
                    nr_required: 1,
                    devs: members,
                },
                vec![11],
            ),
            (
                DiskAccountingKind::PersistentReserved { nr_replicas: 2 },
                vec![3],
            ),
            (DiskAccountingKind::DevLeaving { dev: 1 }, vec![5]),
            (DiskAccountingKind::DevStripeFrag { dev: 1 }, vec![19, 2]),
            (DiskAccountingKind::DevLeaving { dev: 9 }, vec![97]),
            (
                DiskAccountingKind::Compression {
                    compression_type: c::bch_compression_type(0),
                },
                vec![3, 2, 1],
            ),
            (
                DiskAccountingKind::Replicas {
                    data_type: data_type::user,
                    nr_devs: 3,
                    nr_required: 2,
                    devs: members,
                },
                vec![13],
            ),
        ]
        .into_iter()
        .map(|(kind, counters)| AccountingEntry {
            pos: kind.encode(),
            counters,
        })
        .collect::<Vec<_>>();
        collect_accounting(
            &mut usage,
            &entries,
            &devs,
            &[Field::Replicas, Field::Compression],
        )
        .unwrap();
        let summary = &usage.replicas_summary;
        assert_eq!(summary.replicated.len(), 1);
        assert_eq!(summary.replicated[0].bytes, 18 * SECTOR_BYTES);
        assert_eq!(summary.replicated[0].degraded, 1);
        assert_eq!(usage.replicas.len(), 3);
        assert_eq!(summary.erasure_coded[0].data, 2);
        assert_eq!(summary.erasure_coded[0].parity, 1);
        assert_eq!(summary.erasure_coded[0].degraded, 2);
        assert_eq!(summary.erasure_coded[0].bytes, 13 * SECTOR_BYTES);
        assert_eq!(summary.reserved_bytes, 3 * SECTOR_BYTES);
        assert_eq!(usage.devices[0].leaving_bytes, 5 * SECTOR_BYTES);
        assert_eq!(usage.devices[0].stripe_empty, Some(2 * SECTOR_BYTES));
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(
            json["replicas_summary"]["replicated"][0]["bytes"],
            18 * SECTOR_BYTES
        );
        assert_eq!(json["compression"][0]["average_extent_bytes"], 341);
        let mut compression = Printbuf::new();
        compression_to_text(&mut compression, &usage.compression);
        assert!(compression.as_str().contains("341"));
        let mut text = Printbuf::new();
        replicas_summary_to_text(&mut text, &usage.replicas_summary);
        assert!(text.as_str().contains("9216"));
        assert!(text.as_str().contains("-1x"));
        assert!(text.as_str().contains("2+1:"));
        assert!(text.as_str().contains("6656"));

        let mut summary = FsUsage::default();
        collect_accounting(&mut summary, &entries, &devs, &[]).unwrap();
        assert!(summary.replicas.is_empty());
        assert_eq!(
            summary.replicas_summary.replicated[0].bytes,
            18 * SECTOR_BYTES
        );
        let zero_entries = entries
            .iter()
            .map(|entry| AccountingEntry {
                pos: entry.pos.clone(),
                counters: vec![0, 0],
            })
            .collect::<Vec<_>>();
        let mut zero = FsUsage::default();
        collect_accounting(&mut zero, &zero_entries, &devs, &[Field::Compression]).unwrap();
        assert_eq!(zero.compression[0].average_extent_bytes, 0);
        assert_eq!(zero.replicas_summary.replicated.len(), 1);
        assert_eq!(zero.replicas_summary.erasure_coded.len(), 1);
        let mut text = Printbuf::new();
        replicas_summary_to_text(&mut text, &zero.replicas_summary);
        assert!(text.as_str().contains("2x:"));
        assert!(text.as_str().contains("2+1:"));
        assert!(zero.devices.is_empty());
    }

    #[test]
    fn byte_conversion_and_accounting_sums_reject_overflow() {
        let largest = u64::MAX / SECTOR_BYTES;
        assert_eq!(bytes(largest).unwrap(), largest * SECTOR_BYTES);
        assert!(bytes(largest + 1).is_err());
        let entries = [1, 2]
            .into_iter()
            .map(|nr_replicas| AccountingEntry {
                pos: DiskAccountingKind::PersistentReserved { nr_replicas }.encode(),
                counters: vec![largest],
            })
            .collect::<Vec<_>>();
        assert!(collect_accounting(&mut FsUsage::default(), &entries, &[], &[]).is_err());
    }

    fn fixture_device() -> DeviceUsage {
        DeviceUsage {
            label: Some("fixture".to_string()),
            device_index: 1,
            device: "sdx".to_string(),
            state: "rw".to_string(),
            capacity_bytes: 100 * SECTOR_BYTES,
            used_bytes: 40 * SECTOR_BYTES,
            hidden_bytes: 7 * SECTOR_BYTES,
            used_percent: 40,
            leaving_bytes: 0,
            stripe_empty: Some(0),
            bucket_size_bytes: 1 * SECTOR_BYTES,
            buckets: 100,
            data_types: Some(vec![DeviceDataTypeUsage {
                data_type: "user".to_string(),
                is_stripe: false,
                bytes: 40 * SECTOR_BYTES,
                buckets: 40,
                fragmented_bytes: 0,
            }]),
        }
    }

    #[test]
    fn device_capacity_preserves_total_and_summary_hides_reserved_space() {
        let device = fixture_device();

        let json = serde_json::to_value(&device).unwrap();
        assert_eq!(json["capacity_bytes"], 100 * SECTOR_BYTES);
        assert_eq!(json["hidden_bytes"], 7 * SECTOR_BYTES);

        let mut summary = Printbuf::new();
        devices_to_text(&mut summary, &[device], false);
        assert!(summary.as_str().contains("47616"));
        assert!(!summary.as_str().contains("51200"));

        let mut detailed = Printbuf::new();
        dev_usage_full_to_text(&mut detailed, &fixture_device());
        assert!(detailed.as_str().contains("51200"));
        assert!(!detailed.as_str().contains("47616"));
    }

    #[test]
    fn shared_model_keeps_uuid_free_space_and_single_byte_unit() {
        let usage = FsUsage {
            mountpoint: "/fixture".to_string(),
            uuid: "12345678-1234-5678-9abc-def012345678".to_string(),
            capacity_bytes: 200 * SECTOR_BYTES,
            used_bytes: 80 * SECTOR_BYTES,
            online_reserved_bytes: 10 * SECTOR_BYTES,
            free_bytes: vec![bytes(100).unwrap(), bytes(50).unwrap(), 0],
            free_now_bytes: vec![bytes(90).unwrap(), bytes(40).unwrap(), 0],
            replicas_summary: ReplicasSummary {
                replicated: Vec::new(),
                erasure_coded: Vec::new(),
                cached_bytes: 0,
                reserved_bytes: 0,
            },
            replicas: Vec::new(),
            persistent_reserved: Vec::new(),
            compression: Vec::new(),
            btree: Vec::new(),
            rebalance_work: Vec::new(),
            reconcile_work: Vec::new(),
            devices: Vec::new(),
        };
        let json = serde_json::to_value(&usage).unwrap();

        assert_eq!(json["uuid"], usage.uuid);
        assert_eq!(json["capacity_bytes"], 200 * SECTOR_BYTES);
        assert_eq!(json["used_bytes"], 80 * SECTOR_BYTES);
        assert_eq!(json["online_reserved_bytes"], 10 * SECTOR_BYTES);
        assert_eq!(json["free_bytes"][0], 100 * SECTOR_BYTES);
        assert_eq!(json["free_now_bytes"][0], 90 * SECTOR_BYTES);
        assert!(json.get("capacity").is_none());

        let mut text = Printbuf::new();
        fs_usage_to_text(&mut text, &usage, &[Field::Devices]);
        let text = text.to_string();
        assert!(text.contains(&usage.uuid));
        assert!(text.contains("Used:"));
        assert!(text.contains("Online reserved:"));
        assert!(text.contains("Free:"));
        assert!(text.contains("51200"));
        assert!(text.contains("46080"));
    }
}
