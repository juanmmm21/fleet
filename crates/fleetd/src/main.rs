//! Binario `fleetd`: el demonio de fleet (motor de tareas y API HTTP en 127.0.0.1).
#![forbid(unsafe_code)]

use std::process::ExitCode;

use anyhow::Context;
use fleetd::api::{self, AppState};
use fleetd::args::{self, Command, ServeArgs, USAGE};
use fleetd::guard::HostPolicy;
use fleetd::{Engine, EngineConfig};
use tokio::signal::unix::{SignalKind, signal};

fn main() -> ExitCode {
    let command = match args::parse(std::env::args().skip(1), |key| std::env::var(key).ok()) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("fleetd: {err}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Version => {
            println!("{}", fleet_core::version_line("fleetd"));
            ExitCode::SUCCESS
        }
        Command::Help => {
            println!(
                "fleetd — the fleet daemon (task runner and HTTP API on 127.0.0.1)\n\n{USAGE}"
            );
            ExitCode::SUCCESS
        }
        Command::Serve(serve_args) => match run(serve_args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("fleetd: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run(serve_args: ServeArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    runtime.block_on(serve(serve_args))
}

async fn serve(serve_args: ServeArgs) -> anyhow::Result<()> {
    let engine = Engine::start(EngineConfig::new(serve_args.state_dir.clone()))
        .await
        .with_context(|| {
            format!(
                "cannot open the state in {}",
                serve_args.state_dir.display()
            )
        })?;
    let node = nix::unistd::gethostname()
        .context("cannot read the host name")?
        .to_string_lossy()
        .into_owned();
    let listener = tokio::net::TcpListener::bind(serve_args.listen)
        .await
        .with_context(|| format!("cannot listen on {}", serve_args.listen))?;
    eprintln!(
        "fleetd {} on http://{}/ (node {node}, state {})",
        fleet_core::VERSION,
        serve_args.listen,
        serve_args.state_dir.display()
    );
    let mut terminate = signal(SignalKind::terminate()).context("cannot watch SIGTERM")?;
    let (stopping_tx, stopping_rx) = tokio::sync::watch::channel(false);
    let shutdown = async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(err) = result {
                    eprintln!("fleetd: cannot watch Ctrl+C: {err}");
                }
            }
            _ = terminate.recv() => {}
        }
        eprintln!("fleetd: shutting down");
        stopping_tx.send_replace(true);
    };
    let state = AppState {
        engine,
        node,
        shutdown: stopping_rx,
        hosts: HostPolicy {
            extra: serve_args.allow_hosts.clone(),
        },
        repos_root: serve_args.repos_root.clone(),
    };
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("the HTTP server stopped")
}
