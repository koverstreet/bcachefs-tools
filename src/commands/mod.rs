use clap::Subcommand;

pub mod cmd_completions;
pub mod cmd_list;
pub mod cmd_mount;
pub mod cmd_subvolume;
pub mod logger;

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
    #[command(visible_aliases = ["subvol"])]
    Subvolume(cmd_subvolume::Cli),
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
