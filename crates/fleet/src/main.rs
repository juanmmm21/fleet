//! Binario `fleet`: the fleet command-line client.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use fleet_core::cli::{BasicCommand, parse_basic};

const USAGE: &str = "usage: fleet [--version | --help]";

fn main() -> ExitCode {
    match parse_basic(std::env::args().skip(1)) {
        Ok(BasicCommand::Version) => {
            println!("{}", fleet_core::version_line("fleet"));
            ExitCode::SUCCESS
        }
        Ok(BasicCommand::Help) => {
            println!("fleet — the fleet command-line client\n\n{USAGE}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("fleet: {err}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
