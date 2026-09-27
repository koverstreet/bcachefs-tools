//! Deterministic EC stripe deletion/open interleaving on disposable file images.
//! Run after building `bcachefs`: cargo test --release --bin bcachefs
//! ec_lifetime -- --ignored --nocapture --test-threads=1

use bcachefs_kernel::util::async_exec::{spawn, system_unbound};
use std::ffi::{c_char, c_int, CString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

unsafe extern "C" {
    fn rust_test_ec_lifetime(paths: *const *const c_char, nr: u32) -> c_int;
}

fn command(root: &Path, name: &str, cmd: &mut Command) {
    let out = cmd.output().expect("run bcachefs");
    fs::write(root.join(format!("{name}.stdout")), &out.stdout).unwrap();
    fs::write(root.join(format!("{name}.stderr")), &out.stderr).unwrap();
    assert!(
        out.status.success(),
        "{name}: {}; logs in {}",
        out.status,
        root.display()
    );
}

#[test]
#[ignore = "creates three sparse 1 GiB regular files and imports 32 MiB of EC data"]
fn ec_lifetime_deletion_rechecks_open_stripe() {
    let binary = std::env::var_os("BCACHEFS_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // The test executable lives in target/{profile}/deps.
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("bcachefs")
        });
    assert!(binary.is_file(), "build the bcachefs binary first");
    let parent = std::env::var_os("BCACHEFS_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root = parent.join(format!(
        "bcachefs-ec-lifetime-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    eprintln!("EC lifetime artifacts: {} (removed on success unless BCACHEFS_TEST_KEEP is set)", root.display());
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    let mut data = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(source.join("data.bin"))
        .unwrap();
    let chunk: Vec<u8> = (0..1024 * 1024)
        .map(|i| ((i * 131 + i / 251) % 256) as u8)
        .collect();
    for _ in 0..32 {
        data.write_all(&chunk).unwrap();
    }
    drop(data);
    let images: Vec<_> = (0..3)
        .map(|i| {
            let path = root.join(format!("member-{i}.img"));
            let f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            assert!(f.metadata().unwrap().is_file());
            f.set_len(1024 * 1024 * 1024).unwrap();
            path
        })
        .collect();
    let mut format = Command::new(&binary);
    format
        .args([
            "format",
            "--force",
            "--btree_node_size=65536",
            "--bucket_size=524288",
            "--block_size=4096",
            "--metadata_replicas=1",
            "--data_replicas=2",
            "--erasure_code=1",
            "--ec_max_data_blocks=2",
            "--compression=none",
            "--data_checksum=crc32c",
        ])
        .arg(format!("--source={}", source.display()));
    for (i, path) in images.iter().enumerate() {
        format
            .arg(format!("--failure_domain=fixture-member-{i}"))
            .arg(path);
    }
    command(&root, "format", &mut format);
    command(
        &root,
        "fsck-before",
        Command::new(&binary)
            .args(["fsck", "-K", "-f", "-n"])
            .args(&images),
    );

    // Filesystem calls need the shim's initialized current/RCU worker context.
    let paths: Vec<_> = images
        .iter()
        .map(|p| CString::new(p.as_os_str().as_bytes()).unwrap())
        .collect();
    let (tx, rx) = mpsc::channel();
    spawn(system_unbound(), async move {
        let raw: Vec<_> = paths.iter().map(|p| p.as_ptr()).collect();
        let ret = unsafe { rust_test_ec_lifetime(raw.as_ptr(), raw.len() as u32) };
        tx.send(ret).unwrap();
    })
    .unwrap();
    let ret = rx
        .recv_timeout(Duration::from_secs(120))
        .expect("EC lifetime test timed out");
    assert_eq!(
        ret,
        0,
        "EC lifetime regression failed; images preserved in {}",
        root.display()
    );
    command(
        &root,
        "fsck-after",
        Command::new(&binary)
            .args(["fsck", "-K", "-f", "-n"])
            .args(&images),
    );
    if std::env::var_os("BCACHEFS_TEST_KEEP").is_none() {
        fs::remove_dir_all(&root).unwrap();
    }
}
