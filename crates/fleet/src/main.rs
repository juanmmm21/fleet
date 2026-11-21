//! Binario `fleet`: la CLI de fleet.
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

use fleet::args::{self, USAGE};
use fleet::client::{Client, DEFAULT_URL};
use fleet::execute;

fn main() -> ExitCode {
    let command = match args::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("fleet: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let url = std::env::var("FLEET_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_owned());
    let client = Client::new(&url).for_node(std::env::var("FLEET_NODE").ok());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let result = execute(command, &client, &mut out);
    if let Err(err) = out.flush() {
        eprintln!("fleet: cannot write the output: {err}");
        return ExitCode::FAILURE;
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("fleet: {err}");
            ExitCode::FAILURE
        }
    }
}
