use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Parser;

extern "C" {
    fn qcow2_to_raw(infd: i32, outfd: i32);
}

/// Convert a qcow2 image back to a raw device image
#[derive(Parser, Debug)]
#[command(about = "Convert qcow2 dump files back to raw device images")]
pub struct UndumpCli {
    /// Overwrite existing output files
    #[arg(short = 'f', long = "force")]
    force: bool,

    /// qcow2 files to convert
    #[arg(required = true)]
    files: Vec<String>,
}

pub fn cmd_undump(argv: Vec<String>) -> Result<()> {
    let cli = UndumpCli::parse_from(argv);
    let suffix = ".qcow2";

    struct FileEntry {
        input: String,
        output: String,
    }

    let mut entries = Vec::new();

    for f in &cli.files {
        if !f.ends_with(suffix) {
            return Err(anyhow!("{} not a qcow2 image?", f));
        }

        let output = f[..f.len() - suffix.len()].to_string();

        if !cli.force && Path::new(&output).exists() {
            return Err(anyhow!("{} already exists", output));
        }

        entries.push(FileEntry { input: f.clone(), output });
    }

    for e in &entries {
        let infile = std::fs::File::open(&e.input)
            .map_err(|err| anyhow!("{}: {}", e.input, err))?;

        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create(true);
        if !cli.force {
            open_opts.create_new(true);
        } else {
            open_opts.truncate(false);
        }

        let outfile = open_opts.open(&e.output)
            .map_err(|err| anyhow!("{}: {}", e.output, err))?;

        unsafe { qcow2_to_raw(infile.as_raw_fd(), outfile.as_raw_fd()) };
    }

    Ok(())
}
