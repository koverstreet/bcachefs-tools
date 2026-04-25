// Image create/update commands — converted from c_src/cmd_image.c.
//
// Uses a temporary second device for metadata, writes data sequentially to the
// primary device, then migrates metadata to the primary and drops the temp device.

use std::ffi::{CString, c_char, c_void};
use std::fmt::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::accounting::DiskAccountingKind;
use bch_bindgen::btree::{BtreeIter, BtreeTrans, lockrestart_do};
use bch_bindgen::c;
use crate::copy_fs::{CopyFsState, copy_fs};
use bch_bindgen::data::moving::MovingContext;
use bch_bindgen::fs::{Fs, btree_id_is_alloc, bucket_bytes, bucket_to_sector, dev_to_target,
                      writepoint_hashed};
use bch_bindgen::opt_set;
use bch_bindgen::printbuf::Printbuf;
use bch_bindgen::sb;
use bch_bindgen::{POS_MIN, SPOS_MAX, pos};
use clap::Parser;

use crate::commands::format::{
    take_opt_value, take_short_value, metadata_version_current, version_parse,
};
use crate::commands::format_util::DevOpts;
use crate::wrappers::bdev;
use crate::commands::opts::{bch_opt_lookup_negated, opts_usage_str, parse_opt_val};
use crate::key::Passphrase;
use crate::util::parse_human_size;
use crate::wrappers::super_io::SUPERBLOCK_SIZE_DEFAULT;
use crate::wrappers::sysfs;

const BCH_REPLICAS_MAX: u32 = 4;
const BTREE_MAX_DEPTH: u32 = 4;

extern "C" {
    fn strip_fs_alloc(c: *mut c::bch_fs);
}

// --- Core logic, converted from C ---

/// Recursively count allocated bytes in a directory tree.
fn count_input_size(dir: &std::fs::File) -> u64 {
    use std::os::unix::fs::MetadataExt;

    let mut bytes = 0u64;
    let path = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let entries = match std::fs::read_dir(&path) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "." || name_str == ".." || name_str == "lost+found" {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        bytes += meta.size();

        if meta.is_dir() {
            if let Ok(subdir) = std::fs::File::open(entry.path()) {
                bytes += count_input_size(&subdir);
            }
        }
    }

    bytes
}

/// Set data_allowed on both devices for image update:
/// dev 0 gets user data only, dev 1 gets journal+btree.
fn set_data_allowed_for_image_update(fs: &Fs) {
    let _lock = fs.sb_lock();

    let m0 = unsafe { fs.member_mut(0) };
    m0.set_member_data_allowed(1 << c::bch_data_type::BCH_DATA_user as u64);

    let m1 = unsafe { fs.member_mut(1) };
    m1.set_member_data_allowed(
        (1 << c::bch_data_type::BCH_DATA_journal as u64)
            | (1 << c::bch_data_type::BCH_DATA_btree as u64),
    );

    fs.write_super();
    drop(_lock);

    fs.dev_allocator_set_rw(0, true);
    fs.dev_allocator_set_rw(1, true);
}

/// Arguments passed through void* to the move predicate callback.
struct MoveBtreeArgs {
    move_alloc: bool,
    target: u16,
}

/// Predicate for btree node moves — decides whether to move each btree node.
unsafe extern "C" fn move_btree_pred(
    _trans: *mut c::btree_trans,
    arg: *mut c_void,
    btree: c::btree_id,
    k: c::bkey_s_c,
    _io_opts: *mut c::bch_inode_opts,
    data_opts: *mut c::data_update_opts,
) -> i32 {
    let args = &*(arg as *const MoveBtreeArgs);
    let opts = &mut *data_opts;

    opts.target = args.target;

    if (*k.k).type_ != c::bch_bkey_type::KEY_TYPE_btree_ptr_v2 as u8 {
        return 0;
    }

    if !args.move_alloc && btree_id_is_alloc(btree as u32) {
        return 0;
    }

    opts.write_flags |= c::bch_write_flags::BCH_WRITE_only_specified_devs;
    1
}

/// Move btree nodes to a target device. Flushes journal pins first.
fn move_btree(fs: &Fs, move_alloc: bool, target_dev: u32) -> Result<(), anyhow::Error> {
    fs.journal_flush_all_pins();

    let mut args = MoveBtreeArgs {
        move_alloc,
        target: dev_to_target(target_dev),
    };

    let mut ctxt = MovingContext::new(fs, writepoint_hashed(1), false);
    let btree_id_nr = c::btree_id::BTREE_ID_NR as u32;

    for btree in 0..btree_id_nr {
        if !move_alloc && btree_id_is_alloc(btree) {
            continue;
        }

        let btree_id = unsafe { std::mem::transmute::<u32, c::btree_id>(btree) };

        for level in 1..BTREE_MAX_DEPTH {
            unsafe {
                ctxt.move_data_btree(
                    POS_MIN,
                    SPOS_MAX,
                    Some(move_btree_pred),
                    &mut args as *mut MoveBtreeArgs as *mut c_void,
                    btree_id,
                    level,
                )
            }
            .map_err(|e| anyhow!("moving btree {}: {}", btree_id, e))?;
        }
    }

    Ok(())
}

/// Look up the last alloc key to find how many buckets are used.
fn get_nbuckets_used(fs: &Fs) -> Result<u64, anyhow::Error> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(
        &trans,
        c::btree_id::BTREE_ID_alloc,
        pos(0, u64::MAX),
        bch_bindgen::btree::BtreeIterFlags::empty(),
    );

    // Extract type and offset inside the closure to avoid lifetime escape
    let result: Result<(u8, u64), _> = lockrestart_do(&trans, || {
        let k = iter.peek_prev()?;
        match k {
            Some(k) => Ok((k.k.type_, k.k.p.offset)),
            None => Err(bch_bindgen::errcode::BchError::from_raw(-libc::ENOENT)),
        }
    });

    let (key_type, offset) = result
        .map_err(|e| anyhow!("error looking up last alloc key: {}", e))?;

    if key_type == c::bch_bkey_type::KEY_TYPE_alloc_v4 as u8 {
        Ok(offset + 1)
    } else {
        bail!("error looking up last alloc key: no alloc_v4 found")
    }
}

/// Print sector count formatted as human-readable bytes.
fn prt_sectors(out: &mut Printbuf, v: u64) {
    write!(out, "\t").ok();
    out.human_readable_u64(v << 9);
    write!(out, "\r\n").ok();
}

/// Print usage for a specific data type on a device.
fn print_data_type_usage(
    out: &mut Printbuf,
    ca: &c::bch_dev,
    usage: &c::bch_dev_usage_full,
    data_type: u32,
) {
    let d = &usage.d[data_type as usize];
    if d.buckets != 0 {
        bch_bindgen::accounting::prt_data_type(out, unsafe {
            std::mem::transmute::<u32, c::bch_data_type>(data_type)
        });
        prt_sectors(out, bucket_to_sector(ca, d.buckets));
    }
    if d.fragmented != 0 {
        bch_bindgen::accounting::prt_data_type(out, unsafe {
            std::mem::transmute::<u32, c::bch_data_type>(data_type)
        });
        write!(out, " fragmented").ok();
        prt_sectors(out, d.fragmented);
    }
}

/// Print image usage summary.
fn print_image_usage(fs: &Fs, keep_alloc: bool, nbuckets: u64) {
    let mut buf = Printbuf::new();
    buf.set_human_readable(true);

    let usage = fs.dev_usage_full_read(0);
    let ca = unsafe { &*fs.dev_raw(0) };

    print_data_type_usage(&mut buf, ca, &usage, c::bch_data_type::BCH_DATA_sb as u32);
    print_data_type_usage(&mut buf, ca, &usage, c::bch_data_type::BCH_DATA_journal as u32);
    print_data_type_usage(&mut buf, ca, &usage, c::bch_data_type::BCH_DATA_btree as u32);

    {
        let mut indented = buf.indent(2);

        let btree_id_nr = c::btree_id::BTREE_ID_NR as u32;
        for i in 0..btree_id_nr {
            if btree_id_is_alloc(i) && !keep_alloc {
                continue;
            }

            let acc_pos = DiskAccountingKind::Btree { id: i }.encode();
            let v = fs.accounting_mem_read(acc_pos.as_bpos(), 1);

            if v[0] != 0 {
                unsafe { c::bch2_btree_id_to_text(indented.as_raw(), std::mem::transmute::<u32, c::btree_id>(i)) };
                prt_sectors(&mut indented, v[0]);
            }
        }
    }

    // User data via replicas accounting
    let acc_pos = DiskAccountingKind::Replicas {
        data_type: c::bch_data_type::BCH_DATA_user,
        nr_devs: 1,
        nr_required: 1,
        devs: {
            let mut d = [0u8; 20];
            d[0] = 0; // dev 0
            d
        },
    }
    .encode();

    let v = fs.accounting_mem_read(acc_pos.as_bpos(), 1);
    write!(&mut buf, "user").ok();
    prt_sectors(&mut buf, v[0]);

    let user_idx = c::bch_data_type::BCH_DATA_user as usize;
    if usage.d[user_idx].fragmented != 0 {
        write!(&mut buf, "user fragmented").ok();
        prt_sectors(&mut buf, usage.d[user_idx].fragmented);
    }

    buf.tabstop_align();

    // Compression stats
    let mut compression_header = false;
    let comp_nr = c::bch_compression_type::BCH_COMPRESSION_TYPE_NR as u32;
    for i in 1..comp_nr {
        let acc_pos = DiskAccountingKind::Compression {
            compression_type: unsafe { std::mem::transmute::<u32, c::bch_compression_type>(i) },
        }
        .encode();

        let v = fs.accounting_mem_read(acc_pos.as_bpos(), 3);

        if v[0] == 0 {
            continue;
        }

        if !compression_header {
            write!(&mut buf, "compression type\tcompressed\runcompressed\rratio\r\n").ok();
            buf.indent_add(2);
        }
        compression_header = true;

        let sectors_uncompressed = v[1];
        let sectors_compressed = v[2];

        bch_bindgen::accounting::prt_compression_type(&mut buf, unsafe {
            std::mem::transmute::<u32, c::bch_compression_type>(i)
        });
        write!(&mut buf, "\t").ok();

        buf.human_readable_u64(sectors_compressed << 9);
        write!(&mut buf, "\r").ok();

        if i == c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible as u32 {
            buf.newline();
            continue;
        }

        buf.human_readable_u64(sectors_uncompressed << 9);
        if sectors_uncompressed > 0 {
            write!(&mut buf, "\r{}%\r\n", sectors_compressed * 100 / sectors_uncompressed).ok();
        } else {
            write!(&mut buf, "\r\n").ok();
        }
    }

    if compression_header {
        buf.tabstop_align();
        buf.indent_sub(2);
    }

    write!(&mut buf, "image size").ok();
    prt_sectors(&mut buf, bucket_to_sector(ca, nbuckets));

    buf.tabstop_align();
    print!("{}", buf);
}

/// Finalize image: move btree to primary device, truncate, strip alloc info.
fn finish_image(fs: &Fs, keep_alloc: bool, verbosity: u32) -> Result<(), anyhow::Error> {
    if verbosity > 1 {
        println!(
            "moving {}tree to primary device",
            if keep_alloc { "" } else { "non-alloc " }
        );
    }

    // Allow btree data on primary device
    {
        let _lock = fs.sb_lock();
        let m = unsafe { fs.member_mut(0) };
        let allowed = m.member_data_allowed() | (1 << c::bch_data_type::BCH_DATA_btree as u64);
        m.set_member_data_allowed(allowed);
        fs.write_super();
    }

    fs.dev_allocator_set_rw(0, true);

    move_btree(fs, keep_alloc, 0)
        .map_err(|e| anyhow!("migrating btree from temporary device: {}", e))?;

    fs.read_only();

    let nbuckets = get_nbuckets_used(fs)?;

    if verbosity > 0 {
        print_image_usage(fs, keep_alloc, nbuckets);
    }

    // Truncate image to used size
    let ca = unsafe { &*fs.dev_raw(0) };
    let image_size = nbuckets * bucket_bytes(ca);
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw((*ca.disk_sb.bdev).bd_fd) };
    rustix::fs::ftruncate(fd, image_size)
        .map_err(|e| anyhow!("truncate error: {}", e))?;

    // Lock sb and finalize
    let _lock = fs.sb_lock();

    if !keep_alloc {
        if verbosity > 1 {
            println!("Stripping alloc info");
        }
        unsafe { strip_fs_alloc(fs.raw) };
    }

    // Drop temp device
    unsafe { (*fs.raw).devs[1] = std::ptr::null_mut() };

    // Allow journal on primary device
    let m = unsafe { fs.member_mut(0) };
    let allowed = m.member_data_allowed() | (1 << c::bch_data_type::BCH_DATA_journal as u64);
    m.set_member_data_allowed(allowed);

    // Set nbuckets
    unsafe { fs.member_mut(0) }.nbuckets = nbuckets.to_le();

    // Set resize_on_mount for all online members
    let _ = fs.for_each_online_member(|ca| {
        let m = unsafe { fs.member_mut(ca.dev_idx as u32) };
        m.set_member_resize_on_mount(1);
        std::ops::ControlFlow::Continue(())
    });

    // Set small_image feature
    let disk_sb = unsafe { fs.disk_sb_mut() };
    disk_sb.sb_mut().features[0] |= (1u64 << c::bch_sb_feature::BCH_FEATURE_small_image as u64).to_le();

    // Resize members_v2 to contain only one device
    let mi: &c::bch_sb_field_members_v2 = disk_sb.field()
        .expect("members_v2 field missing");
    let member_bytes = u16::from_le(mi.member_bytes);
    let u64s = (std::mem::size_of::<c::bch_sb_field_members_v2>() as u32 + member_bytes as u32)
        .div_ceil(8) as u32;
    sb::sb_field_resize::<c::bch_sb_field_members_v2>(disk_sb, u64s);
    disk_sb.sb_mut().nr_devices = 1;
    disk_sb.sb_mut().set_sb_multi_device(0);

    fs.write_super();

    Ok(())
}

/// Create a new filesystem image from a directory.
fn image_create_inner(
    fs_opt_strs: c::bch_opt_strs,
    fs_opts: c::bch_opts,
    mut format_opts: c::format_opts,
    dev_opts: DevOpts,
    src_path: &str,
    keep_alloc: bool,
    verbosity: u32,
) -> Result<()> {
    let src_dir = std::fs::File::open(src_path)?;
    let meta = src_dir.metadata()?;
    if !meta.is_dir() {
        bail!("{} is not a directory", src_path);
    }

    let input_bytes = count_input_size(&src_dir);

    // Set up two devices: primary for data, temp for metadata
    let primary_path = dev_opts.path.to_string_lossy().into_owned();
    let metadata_path = format!("{}.metadata", primary_path);

    let mut devs = vec![dev_opts];
    let mut meta_dev = DevOpts::new(CString::new(metadata_path.as_str())?);

    // Check temp file doesn't exist
    if std::path::Path::new(&metadata_path).exists() {
        bail!("temporary metadata device {} already exists", metadata_path);
    }

    {
        let dev0_opts = &mut devs[0].opts;
        opt_set!(dev0_opts, data_allowed, (1u64 << c::bch_data_type::BCH_DATA_user as u64) as u8);
    }
    {
        let meta_opts = &mut meta_dev.opts;
        opt_set!(
            meta_opts,
            data_allowed,
            ((1u64 << c::bch_data_type::BCH_DATA_journal as u64)
                | (1u64 << c::bch_data_type::BCH_DATA_btree as u64)) as u8
        );
    }
    devs.push(meta_dev);

    // Open devices for format — both primary and metadata get the same
    // initial size. 64MB floor ensures the metadata device has enough
    // journal capacity for finish_image's btree migration.
    let target_size = std::cmp::max(input_bytes * 2, 64 << 20);
    for dev in &mut devs {
        dev.open(bdev::BLK_OPEN_CREAT, false).map_err(|e| {
            anyhow!("Error opening {}: {}", dev.path.to_string_lossy(), std::io::Error::from_raw_os_error(e))
        })?;
        if unsafe { libc::ftruncate(dev.fd, target_size as libc::off_t) } != 0 {
            bail!("ftruncate error: {}", std::io::Error::last_os_error());
        }
    }

    format_opts.no_sb_at_end = true;

    let sb = crate::commands::format_util::format(fs_opt_strs, fs_opts, format_opts, &mut devs);
    if sb.is_null() {
        bail!("format returned null");
    }

    if verbosity > 1 {
        let mut buf = Printbuf::new();
        buf.set_human_readable(true);
        unsafe {
            buf.sb_to_text(
                std::ptr::null_mut(),
                &*sb,
                false,
                1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32,
            );
        }
        print!("{}", buf);
    }

    // Collect paths before dropping devs — Fs::open needs O_EXCL and
    // the format fds are still holding it.
    let device_paths: Vec<PathBuf> = devs
        .iter()
        .map(|d| PathBuf::from(d.path.to_string_lossy().as_ref()))
        .collect();
    drop(devs);

    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, copygc_enabled, 0u8);
    opt_set!(opts, reconcile_enabled, 0u8);
    opt_set!(opts, nostart, 1u8);

    let fs = Fs::open(&device_paths, opts)
        .map_err(|e| anyhow!("error opening {}: {}", device_paths[0].display(), e))?;

    fs.set_loglevel(5 + verbosity.saturating_sub(1));

    // Delete temp metadata file now that it's open
    let _ = std::fs::remove_file(&metadata_path);

    fs.start().map_err(|e| anyhow!("starting fs: {}", e))?;

    let src_cstr = CString::new(src_path)?;
    let mut state = CopyFsState::new_copy();
    state.verbosity = verbosity;

    let src_file = std::fs::File::open(src_path)
        .map_err(|e| anyhow!("error opening {}: {}", src_path, e))?;

    let result = (|| -> Result<()> {
        use std::os::fd::AsFd;
        copy_fs(&fs, &mut state, src_file.as_fd(), &src_cstr)
            .map_err(|e| anyhow!("syncing data: {}", e))?;
        finish_image(&fs, keep_alloc, verbosity)?;
        Ok(())
    })();

    drop(src_file);

    if let Err(e) = result {
        for p in &device_paths {
            let _ = std::fs::remove_file(p);
        }
        return Err(e);
    }

    let ret = fs.exit();
    if ret != 0 {
        bail!(
            "error shutting down new filesystem: {}",
            bch_bindgen::errcode::BchError::from_raw(ret).msg()
        );
    }

    Ok(())
}

/// Update an existing filesystem image from a directory.
fn image_update_inner(
    src_path: &str,
    dst_image: &str,
    keep_alloc: bool,
    verbosity: u32,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let src_dir = std::fs::File::open(src_path)?;
    let meta = src_dir.metadata()?;
    if !meta.is_dir() {
        bail!("{} is not a directory", src_path);
    }

    let input_bytes = count_input_size(&src_dir);
    drop(src_dir);

    // Grow existing image to make room
    let dst_meta = std::fs::metadata(dst_image)?;
    let new_size = dst_meta.size() + input_bytes * 2;
    {
        let dst_file = std::fs::OpenOptions::new()
            .write(true)
            .open(dst_image)
            .map_err(|e| anyhow!("truncate error opening {}: {}", dst_image, e))?;
        dst_file.set_len(new_size)
            .map_err(|e| anyhow!("truncate error: {}", e))?;
    }

    // Open filesystem
    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, copygc_enabled, 0u8);
    opt_set!(opts, reconcile_enabled, 0u8);
    opt_set!(opts, nostart, 1u8);

    let fs = Fs::open(&[PathBuf::from(dst_image)], opts)
        .map_err(|e| anyhow!("error opening image: {}", e))?;

    fs.set_loglevel(5 + verbosity.saturating_sub(1));

    // Add temporary metadata device
    let metadata_path = format!("{}.metadata", dst_image);

    let mut dev_opts = DevOpts::new(CString::new(metadata_path.as_str())?);

    dev_opts.open(bdev::BLK_OPEN_CREAT, false).map_err(|e| {
        anyhow!("error opening {}: {}", metadata_path, std::io::Error::from_raw_os_error(e))
    })?;

    // Temp device needs enough space for btree nodes AND adequate journal
    // for the btree migration workload. With small bucket sizes, the
    // journal gets 1/128th of the device (min 8 buckets), which can be
    // far too little. 64MB floor ensures reasonable journal capacity.
    let metadata_dev_size = std::cmp::max(
        input_bytes,
        std::cmp::max(
            unsafe { (*fs.raw).opts.btree_node_size as u64 * c::BCH_MIN_NR_NBUCKETS as u64 },
            64 << 20,
        ),
    );

    if unsafe { libc::ftruncate(dev_opts.fd, metadata_dev_size as libc::off_t) } != 0 {
        bail!("ftruncate error: {}", std::io::Error::last_os_error());
    }

    let block_size = unsafe { (*fs.raw).opts.block_size as u32 };
    let btree_node_size = unsafe { (*fs.raw).opts.btree_node_size };
    let ret = crate::commands::format_util::format_for_device_add(
        &mut dev_opts, block_size, btree_node_size,
    );
    if ret != 0 {
        bail!("formatting metadata device: {}",
            bch_bindgen::errcode::BchError::from_raw(ret).msg());
    }

    fs.dev_add(&metadata_path)
        .map_err(|e| anyhow!("error adding metadata device: {}", e))?;

    set_data_allowed_for_image_update(&fs);

    fs.start().map_err(|e| anyhow!("starting fs: {}", e))?;

    if verbosity > 1 {
        println!("Moving btree to temp device");
    }
    move_btree(&fs, true, 1)
        .map_err(|e| anyhow!("migrating btree to temp device: {}", e))?;

    // Delete xattrs — they will be recreated
    if verbosity > 1 {
        println!("Deleting xattrs");
    }
    fs.btree_delete_range(
        c::btree_id::BTREE_ID_xattrs,
        POS_MIN,
        SPOS_MAX,
        c::btree_iter_update_trigger_flags::BTREE_ITER_all_snapshots,
    )
    .map_err(|e| anyhow!("deleting xattrs: {}", e))?;

    if verbosity > 1 {
        println!("Syncing data");
    }

    let src_cstr = CString::new(src_path)?;
    let mut state = CopyFsState::new_copy();
    state.verbosity = verbosity;

    let src_file = std::fs::File::open(src_path)
        .map_err(|e| anyhow!("error opening {}: {}", src_path, e))?;

    let result = (|| -> Result<()> {
        use std::os::fd::AsFd;
        copy_fs(&fs, &mut state, src_file.as_fd(), &src_cstr)
            .map_err(|e| anyhow!("syncing data: {}", e))?;
        finish_image(&fs, keep_alloc, verbosity)?;
        Ok(())
    })();

    drop(src_file);

    let exit_ret = fs.exit();
    let _ = std::fs::remove_file(&metadata_path);

    result?;
    if exit_ret != 0 {
        bail!(
            "error shutting down filesystem: {}",
            bch_bindgen::errcode::BchError::from_raw(exit_ret).msg()
        );
    }

    Ok(())
}

// --- Argument parsing (unchanged) ---

fn image_create_usage() {
    let fs_opts = opts_usage_str(
        c::opt_flags::OPT_FORMAT as u32 | c::opt_flags::OPT_FS as u32,
        c::opt_flags::OPT_DEVICE as u32,
    );
    let dev_opts = opts_usage_str(
        c::opt_flags::OPT_DEVICE as u32,
        c::opt_flags::OPT_FS as u32,
    );

    print!("\
bcachefs image create - create a filesystem image from a directory
Usage: bcachefs image create [OPTION]... <image>

Options:
      --source=path            Source directory (required)
  -a, --keep-alloc             Include allocation info in the filesystem
                               6.16+ regenerates alloc info on first rw mount
{fs_opts}\
      --replicas=#             Sets both data and metadata replicas
      --encrypted              Enable whole filesystem encryption (chacha20/poly1305)
      --passphrase_file=file   File containing passphrase used for encryption/decryption
      --no_passphrase          Don't encrypt master encryption key
  -L, --fs_label=label
  -U, --uuid=uuid
      --superblock_size=size
      --version=version        Create filesystem with specified on disk format version

Device specific options:
{dev_opts}\
      --fs_size=size           Size of filesystem on device
  -l, --label=label            Disk label

  -f, --force
  -q, --quiet                  Only print errors
  -v, --verbose                Verbose filesystem initialization
  -h, --help                   Display this help and exit

Report bugs to <linux-bcachefs@vger.kernel.org>
");
}

fn cmd_image_create(argv: Vec<String>) -> Result<()> {
    let opt_flags = c::opt_flags::OPT_FORMAT as u32
        | c::opt_flags::OPT_FS as u32
        | c::opt_flags::OPT_DEVICE as u32;

    let mut source: Option<String> = None;
    let mut keep_alloc = false;
    let mut encrypted = false;
    let mut no_passphrase = false;
    let mut passphrase_file: Option<String> = None;
    let mut fs_label: Option<String> = None;
    let mut uuid_bytes: Option<[u8; 16]> = None;
    let mut format_version: Option<u32> = None;
    let mut superblock_size: u32 = SUPERBLOCK_SIZE_DEFAULT;
    let mut verbosity: u32 = 1;

    let mut dev_label: Option<String> = None;
    let mut dev_fs_size: u64 = 0;
    let mut dev_opts: c::bch_opts = Default::default();

    let mut fs_opts: c::bch_opts = Default::default();
    let mut deferred_opts: Vec<(usize, String)> = Vec::new();

    let mut image_path: Option<String> = None;

    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];

        if arg == "--" {
            i += 1;
            if i < argv.len() {
                image_path = Some(argv[i].clone());
            }
            break;
        }

        if arg.starts_with("--") && arg.len() > 2 {
            let opt_part = &arg[2..];
            let (raw_name, inline_val) = match opt_part.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (opt_part, None),
            };
            let name = raw_name.replace('-', "_");

            if let Some((opt_id, opt, negated)) = bch_opt_lookup_negated(&name) {
                if opt.flags as u32 & opt_flags != 0 {
                    let val_str = if negated {
                        "0".to_string()
                    } else if let Some(v) = inline_val {
                        v.to_string()
                    } else if opt.type_ != c::opt_type::BCH_OPT_BOOL {
                        take_opt_value(None, &argv, &mut i, raw_name)?
                    } else {
                        "1".to_string()
                    };

                    match parse_opt_val(opt, &val_str)? {
                        None => deferred_opts.push((opt_id as usize, val_str)),
                        Some(v) => {
                            if opt.flags as u32 & c::opt_flags::OPT_DEVICE as u32 != 0 {
                                bch_bindgen::opts::opt_set_by_id(&mut dev_opts, opt_id, v);
                            } else if opt.flags as u32 & c::opt_flags::OPT_FS as u32 != 0 {
                                bch_bindgen::opts::opt_set_by_id(&mut fs_opts, opt_id, v);
                            }
                        }
                    }

                    i += 1;
                    continue;
                }
            }

            match name.as_str() {
                "source" => {
                    source = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "keep_alloc" => keep_alloc = true,
                "replicas" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let v: u32 = val.parse().map_err(|_| anyhow!("invalid replicas"))?;
                    if v == 0 || v > BCH_REPLICAS_MAX {
                        bail!("invalid replicas");
                    }
                    opt_set!(fs_opts, metadata_replicas, v as u8);
                    opt_set!(fs_opts, data_replicas, v as u8);
                }
                "encrypted" => encrypted = true,
                "passphrase_file" => {
                    passphrase_file =
                        Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "no_passphrase" => no_passphrase = true,
                "fs_label" => {
                    fs_label = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "uuid" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let u = uuid::Uuid::parse_str(&val).map_err(|_| anyhow!("Bad uuid"))?;
                    uuid_bytes = Some(*u.as_bytes());
                }
                "fs_size" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    dev_fs_size = parse_human_size(&val)?;
                }
                "superblock_size" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let size = parse_human_size(&val)?;
                    superblock_size = (size >> 9) as u32;
                }
                "label" => {
                    dev_label = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "version" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    format_version = Some(version_parse(&val)?);
                }
                "force" => {}
                "quiet" => verbosity = 0,
                "verbose" => verbosity = verbosity.saturating_add(1),
                "help" => {
                    image_create_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            match arg.as_bytes()[1] {
                b'L' => {
                    fs_label = Some(take_short_value(arg, &argv, &mut i, 'L')?);
                }
                b'l' => {
                    dev_label = Some(take_short_value(arg, &argv, &mut i, 'l')?);
                }
                b'U' => {
                    let val = take_short_value(arg, &argv, &mut i, 'U')?;
                    let u = uuid::Uuid::parse_str(&val).map_err(|_| anyhow!("Bad uuid"))?;
                    uuid_bytes = Some(*u.as_bytes());
                }
                b'a' => keep_alloc = true,
                b'f' => {}
                b'q' => verbosity = 0,
                b'v' => verbosity = verbosity.saturating_add(1),
                b'h' => {
                    image_create_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        // Positional: image path
        image_path = Some(arg.clone());
        i += 1;
    }

    let source = source.ok_or_else(|| {
        image_create_usage();
        anyhow!("--source is required")
    })?;

    let image_path = image_path.ok_or_else(|| {
        image_create_usage();
        anyhow!("please supply an image path")
    })?;

    // Handle encryption
    if passphrase_file.is_some() && !encrypted {
        bail!("--passphrase_file requires --encrypted");
    }
    if passphrase_file.is_some() && no_passphrase {
        bail!("--passphrase_file, --no_passphrase are incompatible");
    }

    let passphrase: Option<Passphrase> = if encrypted && !no_passphrase {
        Some(if let Some(ref path) = passphrase_file {
            Passphrase::new_from_file(path)?
        } else {
            Passphrase::new_from_prompt_twice()?
        })
    } else {
        None
    };

    // Determine format version
    let _ = process::Command::new("modprobe")
        .arg("bcachefs")
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status();

    let kernel_version = sysfs::bcachefs_kernel_version() as u32;
    let current_version = metadata_version_current();

    let version = format_version.unwrap_or_else(|| {
        if kernel_version > 0 {
            current_version.min(kernel_version)
        } else {
            current_version
        }
    });

    // Build C format_opts
    let label_cstr = fs_label.as_ref().map(|l| CString::new(l.as_str())).transpose()?;
    let dev_label_cstr = dev_label.as_ref().map(|l| CString::new(l.as_str())).transpose()?;
    let path_cstr = CString::new(image_path.as_str())?;

    let mut fmt_opts = c::format_opts {
        version,
        superblock_size,
        encrypted,
        ..Default::default()
    };
    if let Some(ref l) = label_cstr {
        fmt_opts.label = l.as_ptr() as *mut c_char;
    }
    if let Some(bytes) = uuid_bytes {
        fmt_opts.uuid.b = bytes;
    }
    if let Some(ref p) = passphrase {
        fmt_opts.passphrase = p.get().as_ptr() as *mut c_char;
    }

    // Build bch_opt_strs for deferred options
    let mut fs_opt_strs: c::bch_opt_strs = Default::default();
    for &(id, ref val) in &deferred_opts {
        let cstr = CString::new(val.as_str())?;
        fs_opt_strs.set(id, &cstr);
    }

    // Build DevOpts
    let mut d = DevOpts::new(path_cstr);
    d.fs_size = dev_fs_size;
    d.opts = dev_opts;
    d.label = dev_label_cstr;

    let result = image_create_inner(
        fs_opt_strs,
        fs_opts,
        fmt_opts,
        d,
        &source,
        keep_alloc,
        verbosity,
    );

    fs_opt_strs.free();

    result
}

/// Update a filesystem image, minimizing changes
#[derive(Parser, Debug)]
#[command(about = "Update a filesystem image, minimizing changes")]
pub struct ImageUpdateCli {
    /// Source directory
    #[arg(short = 's', long = "source")]
    source: String,

    /// Include allocation info in the filesystem
    #[arg(short = 'a', long = "keep-alloc")]
    keep_alloc: bool,

    /// Only print errors
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Verbose filesystem initialization
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Image file to update
    #[arg(required = true)]
    image: String,
}

fn cmd_image_update(cli: ImageUpdateCli) -> Result<()> {

    let verbosity: u32 = if cli.quiet {
        0
    } else {
        1 + cli.verbose as u32
    };

    image_update_inner(&cli.source, &cli.image, cli.keep_alloc, verbosity)
}

pub const CMD_CREATE: super::CmdDef = raw_cmd!("create", "Create a filesystem image", cmd_image_create);
pub const CMD_UPDATE: super::CmdDef = typed_cmd!("update", "Update a filesystem image", ImageUpdateCli, cmd_image_update);
pub const CMD: super::CmdDef = super::CmdDef {
    name: "image", about: "Filesystem image commands", aliases: &[],
    kind: super::CmdKind::Group { children: &[&CMD_CREATE, &CMD_UPDATE] },
};
