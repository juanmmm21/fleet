//! Binario `fleetd`: the fleet daemon (task runner and HTTP API on 127.0.0.1).
#![forbid(unsafe_code)]

use std::process::ExitCode;

use fleet_core::cli::{BasicCommand, parse_basic};

const USAGE: &str = "usage: fleetd [--version | --help]";

fn main() -> ExitCode {
    match parse_basic(std::env::args().skip(1)) {
        Ok(BasicCommand::Version) => {
            println!("{}", fleet_core::version_line("fleetd"));
            ExitCode::SUCCESS
        }
        Ok(BasicCommand::Help) => {
            println!(
                "fleetd — the fleet daemon (task runner and HTTP API on 127.0.0.1)\n\n{USAGE}"
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("fleetd: {err}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
