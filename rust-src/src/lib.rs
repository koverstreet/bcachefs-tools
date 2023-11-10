use clap::Subcommand;

pub mod key;
pub mod logger;
pub mod cmd_mount;
pub mod cmd_list;
pub mod cmd_completions;

#[derive(clap::Parser, Debug)]
#[command(name = "bcachefs")]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    List(cmd_list::Cli),
    Mount(cmd_mount::Cli),
    Completions(cmd_completions::Cli),
}

#[macro_export]
macro_rules! c_str {
    ($lit:expr) => {
        unsafe {
            std::ffi::CStr::from_ptr(concat!($lit, "\0").as_ptr() as *const std::os::raw::c_char)
                .to_bytes_with_nul()
                .as_ptr() as *const std::os::raw::c_char
        }
    };
}

#[macro_export]
macro_rules! transform_c_args {
    ($var:ident, $argc:expr, $argv:expr) => {
        // TODO: `OsStr::from_bytes` only exists on *nix
        use ::std::os::unix::ffi::OsStrExt;
        let $var: Vec<_> = (0..$argc)
        .map(|i| unsafe { ::std::ffi::CStr::from_ptr(*$argv.add(i as usize)) })
        .map(|i| ::std::ffi::OsStr::from_bytes(i.to_bytes()))
        .collect();
    };
}

#[derive(Debug)]
struct ErrnoError(errno::Errno);
impl std::fmt::Display for ErrnoError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}
impl std::error::Error for ErrnoError {}
