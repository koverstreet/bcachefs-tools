use std::path::{Path, PathBuf};

fn unix_epoch_mtime() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_nanos(0)
}

#[derive(Debug, Clone, PartialEq)]
struct MakefileDependencyRule<'a> {
    obj: &'a str,
    deps: Vec<&'a str>,
}

impl<'a> MakefileDependencyRule<'a> {
    fn new(text: &'a str) -> Option<Self> {
        let (obj, text) = text.split_once(':')?;
        let deps = text
            .split_terminator(&[' ', '\\'][..])
            .filter_map(|e| {
                let e = e.trim();
                if !e.is_empty() {
                    return Some(e);
                }
                None
            })
            .collect();
        Some(Self { obj, deps })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DepfileError;

impl From<std::io::Error> for DepfileError {
    fn from(_: std::io::Error) -> Self {
        Self {}
    }
}

#[derive(Debug, Clone)]
struct CompileJob<'a> {
    bs: &'a BuildSystem,
    src: PathBuf,
}

impl<'a> CompileJob<'a> {
    fn new(bs: &'a BuildSystem, src: PathBuf) -> Self {
        Self { bs, src }
    }

    fn get_absolute_headerish_path(&self, path: &str) -> Option<PathBuf> {
        let mut abspath = PathBuf::new();
        abspath.push(path);
        if abspath.is_absolute() {
            return Some(abspath);
        }
        for custom_include_directory in self.bs.custom_include_directories() {
            let mut abspath = PathBuf::new();
            abspath.push(custom_include_directory);
            abspath.push(path);
            if abspath.exists() {
                return Some(abspath);
            }
        }
        None
    }

    fn objfile_path(&self) -> PathBuf {
        let mut p = PathBuf::new();
        p.push(std::env::var("OUT_DIR").unwrap());
        p.push("source-root");
        p.push(
            self.src
                .with_extension("o")
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap(),
        );
        p
    }

    fn objfile_mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.objfile_path())
            .ok()
            .and_then(|m| m.modified().ok())
    }

    fn depfile_path(&self) -> PathBuf {
        self.objfile_path().with_extension("d")
    }

    fn emit_rerun_if_changed_metadata(&self) -> Result<(), DepfileError> {
        // println!(
        //     "cargo:rerun-if-changed={}/*.c",
        //     self.src.parent().unwrap().to_string_lossy()
        // );
        // println!(
        //     "cargo:rerun-if-changed={}/**/*.c",
        //     self.src.parent().unwrap().to_string_lossy()
        // );
        let depfile_str = std::fs::read_to_string(self.depfile_path())?;
        let depfile = MakefileDependencyRule::new(&depfile_str).ok_or(DepfileError)?;

        let mut had_errors = false;
        for dep in &depfile.deps {
            if let Some(dep) = self.get_absolute_headerish_path(dep) {
                println!("cargo:rerun-if-changed={}", dep.to_string_lossy());
            } else {
                had_errors = true;
            }
        }
        if had_errors {
            Err(DepfileError)
        } else {
            Ok(())
        }
    }

    fn is_outdated(&self) -> bool {
        let Some(obj_mtime) = self.objfile_mtime() else {
            return true;
        };
        let Ok(depfile_str) = std::fs::read_to_string(self.depfile_path()) else {
            return true;
        };
        let Some(depfile) = MakefileDependencyRule::new(&depfile_str) else {
            return true;
        };

        let _ = self.emit_rerun_if_changed_metadata();

        let is_dep_older_than_obj = |&dep| {
            matches!(self
            .get_absolute_headerish_path(dep)
            .and_then(|dep| std::fs::metadata(dep).ok())
            .and_then(|m| m.modified().ok()), Some(dep_mtime) if dep_mtime < obj_mtime)
        };

        !depfile.deps.iter().all(is_dep_older_than_obj)
    }

    fn compile(&self) {
        if !self.is_outdated() {
            return;
        }
        std::fs::create_dir_all(self.depfile_path().parent().unwrap()).unwrap();
        let res = self
            .bs
            .get_compile_rule_stencil()
            .clone()
            .file(&self.src)
            .flag("-MMD")
            .flag("-MF")
            .flag(self.depfile_path())
            .compile_intermediates();
        assert_eq!(res.len(), 1);
        let real_obj = &res[0];
        if std::fs::metadata(self.objfile_path()).is_ok() {
            std::fs::remove_file(self.objfile_path()).unwrap();
        }
        std::fs::hard_link(real_obj, self.objfile_path()).unwrap();
        self.emit_rerun_if_changed_metadata().unwrap();
    }
}

#[derive(Debug, Clone)]
struct BuildSystem {
    custom_include_directories: Vec<&'static str>,
    compile_rule_stencil: cc::Build,
}

impl BuildSystem {
    fn new() -> Self {
        println!("cargo:rerun-if-changed=.bcachefs_revision");

        let VERSION_STRING = format!("\"{}\"", env!("CARGO_PKG_VERSION"));
        let mut custom_include_directories = vec![".", "c_src", "include", "raid"];
        custom_include_directories.extend([
            "/usr/include/blkid",
            "/usr/include/uuid",
            "/usr/include/x86_64-linux-gnu",
        ]);
        let stencil_compile_rule = cc::Build::new()
            .std("gnu11")
            .opt_level(2)
            .debug(true)
            .flag("-MMD")
            .pic(true)
            .flag("-Wall")
            .flag("-Wno-pointer-sign")
            .flag("-Wno-deprecated-declarations")
            .flag("-fno-strict-aliasing")
            .flag("-fno-delete-null-pointer-checks")
            .includes(&custom_include_directories)
            .define("_FILE_OFFSET_BITS", "64")
            .define("_GNU_SOURCE", None)
            .define("_LGPL_SOURCE", None)
            .define("RCU_MEMBARRIER", None)
            .define("ZSTD_STATIC_LINKING_ONLY", None)
            .define("FUSE_USE_VERSION", "35")
            .define("NO_BCACHEFS_CHARDEV", None)
            .define("NO_BCACHEFS_FS", None)
            .define("NO_BCACHEFS_SYSFS", None)
            .define("CONFIG_UNICODE", None)
            .define("VERSION_STRING", VERSION_STRING.as_str())
            .define("__SANE_USERSPACE_TYPES__", None)
            .flag("-Wno-unused-but-set-variable")
            .flag("-Wno-stringop-overflow")
            .flag("-Wno-zero-length-bounds")
            .flag("-Wno-missing-braces")
            .flag("-Wno-zero-length-array")
            .flag("-Wno-shift-overflow")
            .flag("-Wno-enum-conversion")
            .flag("-Wno-gnu-variable-sized-type-not-at-end")
            .clone();
        Self {
            custom_include_directories,
            compile_rule_stencil: stencil_compile_rule,
        }
    }

    fn lib_sources() -> Vec<std::path::PathBuf> {
        glob_c_source_files()
    }

    fn custom_include_directories(&self) -> &[&'static str] {
        &self.custom_include_directories
    }

    fn get_compile_rule_stencil(&self) -> &cc::Build {
        assert!(self.compile_rule_stencil.get_files().next().is_none());
        &self.compile_rule_stencil
    }

    fn build(&self) {
        let mut objects = vec![];
        for source in Self::lib_sources() {
            let compile_job = CompileJob::new(self, source);
            compile_job.compile();
            objects.push(compile_job.objfile_path());
        }

        println!("cargo:rustc-link-arg=-Wl,--whole-archive");
        for object in objects {
            println!("cargo:rustc-link-arg={}", object.to_string_lossy());
        }
        println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");
    }
}

fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with("."))
        .unwrap_or(false)
}

fn is_within_cargo_target_dir(entry: &walkdir::DirEntry) -> bool {
    entry
        .path()
        .starts_with(concat!(env!("CARGO_MANIFEST_DIR"), "/target"))
}

fn is_c_source_file(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_file()
        && entry
            .file_name()
            .to_str()
            .map(|s| s.ends_with(".c"))
            .unwrap_or(false)
}

fn glob_c_source_files() -> Vec<std::path::PathBuf> {
    let walker = walkdir::WalkDir::new(env!("CARGO_MANIFEST_DIR")).into_iter();
    walker
        .filter_entry(|e| !is_hidden(e) && !is_within_cargo_target_dir(e))
        .filter_map(|e| {
            let entry = e.unwrap();
            if is_c_source_file(&entry) {
                return Some(entry.into_path());
            }
            None
        })
        .collect()
}

fn main() {
    BuildSystem::new().build();

    println!("cargo:rustc-link-lib=urcu");
    println!("cargo:rustc-link-lib=zstd");
    println!("cargo:rustc-link-lib=blkid");
    println!("cargo:rustc-link-lib=uuid");
    println!("cargo:rustc-link-lib=sodium");
    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=lz4");
    println!("cargo:rustc-link-lib=zstd");
    println!("cargo:rustc-link-lib=udev");
    println!("cargo:rustc-link-lib=keyutils");
    println!("cargo:rustc-link-lib=aio");

    if std::env::var("BCACHEFS_FUSE").is_ok() {
        println!("cargo:rustc-link-lib=fuse3");
    }
}
