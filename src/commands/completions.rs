use clap::{Command, Parser};
use clap_complete::{generate, Generator, Shell};
use std::io;

/// Generate shell completions
#[derive(Parser, Debug)]
pub struct Cli {
    shell: Shell,
}

fn print_completions<G: Generator>(gen: G, cmd: &mut Command) {
    generate(gen, cmd, cmd.get_name().to_string(), &mut io::stdout());
}

fn completions(cli: Cli) {
    print_completions(cli.shell, &mut super::build_cli());
}

pub static CMD: super::CmdDef = {
    fn __cmd() -> clap::Command { <Cli as clap::CommandFactory>::command() }
    fn __run(argv: Vec<String>) -> std::process::ExitCode {
        completions(Cli::parse_from(argv));
        std::process::ExitCode::SUCCESS
    }
    super::CmdDef {
        name: "completions", about: "Generate shell completions", aliases: &[],
        kind: super::CmdKind::Typed { cmd: __cmd, run: __run },
    }
};
