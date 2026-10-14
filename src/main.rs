#![allow(clippy::unusual_byte_groupings)]

mod app;
mod cli;
mod csv_writer;
mod logger;
mod model;
mod platform;
mod probe;
mod report;
mod shutdown;
mod smu;
mod smuctl;
#[cfg(test)]
mod test_support;
mod ui;

use anyhow::Result;
use cli::{Command, Options};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustinfo: 错误: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|arg| arg == csv_writer::HELPER) {
        return csv_writer::serve(&args[1..]);
    }
    let options = Options::parse(args)?;
    match options.command {
        Command::Tui => app::run_tui(options.interval.as_secs_f64()),
        Command::Dump => report::run_dump(),
        Command::Log(path) => app::run_log(path, options.interval.as_secs_f64()),
        Command::SmuProbe => smu::probe(),
        Command::SmuTable => smu::smu_table(),
        Command::Smuctl(args) => smuctl::run(&args),
        Command::RenderTest => app::render_test(),
        Command::Help => {
            cli::print_help();
            Ok(())
        }
        Command::Version => {
            println!("rustinfo {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
