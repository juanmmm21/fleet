//! `fleet`: la CLI de fleet. Habla con `fleetd` por su API y nada más.
#![forbid(unsafe_code)]

pub mod args;
pub mod client;
pub mod output;
pub mod tui;

use std::io::{self, Write};
use std::path::Path;

use args::{Command, RunArgs};
use client::{Client, ClientError, NewTask};
use fleet_core::{RepoRef, Task, TaskId};

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("cannot write the output: {0}")]
    Io(#[from] io::Error),
    #[error("cannot use the repository {path}: {source}")]
    Repo {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Ejecuta una orden ya leída contra `client`, escribiendo en `out`.
pub fn execute(command: Command, client: &Client, out: &mut impl Write) -> Result<(), CliError> {
    match command {
        Command::Version => writeln!(out, "{}", fleet_core::version_line("fleet"))?,
        Command::Help => writeln!(
            out,
            "fleet — the fleet command-line client\n\n{}",
            args::USAGE
        )?,
        Command::Run(run) => run_task(run, client, out)?,
        Command::List { json } => {
            let tasks = client.list()?;
            if json {
                print_json(out, &tasks)?;
            } else {
                output::task_table(out, &tasks)?;
            }
        }
        Command::Show { id, json } => {
            let task = client.get(&id)?;
            if json {
                print_json(out, &task)?;
            } else {
                output::task_detail(out, &task)?;
            }
        }
        Command::Log { id, follow } => {
            write!(out, "{}", client.log(&id)?)?;
            if follow {
                follow_task(&id, client, out)?;
            }
        }
        Command::Diff { id } => write!(out, "{}", client.diff(&id)?)?,
        Command::Answer { id, answer } => {
            let task = client.answer(&id, &answer)?;
            writeln!(out, "task {} is back with the agent", task.id)?;
        }
        Command::Merge { id } => {
            client.merge(&id)?;
            writeln!(
                out,
                "integrating {id}: rebase, tests and push (follow with `fleet log {id} --follow`)"
            )?;
        }
        Command::Drop { id } => {
            client.drop_task(&id)?;
            writeln!(out, "dropped {id}; its worktree and branch are gone")?;
        }
        Command::Cancel { id } => {
            client.cancel(&id)?;
            writeln!(out, "cancelling {id}")?;
        }
        Command::Tui => tui::run(client)?,
    }
    Ok(())
}

/// Qué repo se manda. En esta máquina, su ruta absoluta y real; para otro nodo, su nombre,
/// porque allí la carpeta es otra (ADR 0002): el del checkout si existe aquí, o el nombre tal
/// cual se escribió.
fn repo_for(repo: &Path, remote: bool) -> Result<String, CliError> {
    let raw = repo.to_string_lossy().into_owned();
    if remote && !repo.exists() && RepoRef::parse(&raw).is_ok_and(|r| matches!(r, RepoRef::Name(_)))
    {
        return Ok(raw);
    }
    let real = repo.canonicalize().map_err(|source| CliError::Repo {
        path: raw.clone(),
        source,
    })?;
    if !remote {
        return Ok(real.to_string_lossy().into_owned());
    }
    real.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| CliError::Repo {
            path: raw,
            source: io::Error::new(io::ErrorKind::InvalidInput, "the repository has no name"),
        })
}

fn run_task(run: RunArgs, client: &Client, out: &mut impl Write) -> Result<(), CliError> {
    let remote = run.node.is_some() || client.node().is_some();
    let task = client.create(&NewTask {
        repo: repo_for(&run.repo, remote)?,
        prompt: run.prompt,
        agent: run.agent,
        base: run.base,
        node: run.node,
    })?;
    if run.json {
        print_json(out, &task)?;
    } else if remote && client.node() != Some(task.node.as_str()) {
        writeln!(
            out,
            "task {} created on node {} (follow with `FLEET_NODE={} fleet log {} --follow`)",
            task.id, task.node, task.node, task.id
        )?;
    } else {
        writeln!(
            out,
            "task {} created on branch {} (follow with `fleet log {} --follow`)",
            task.id, task.branch, task.id
        )?;
    }
    Ok(())
}

/// Sigue la salida del agente de la tarea hasta que se queda quieta.
fn follow_task(id: &TaskId, client: &Client, out: &mut impl Write) -> Result<(), CliError> {
    let mut write_error = None;
    let mut already_resting = None;
    let on_open = || {
        let task = client.get(id)?;
        let resting = output::is_resting(task.state);
        if resting {
            already_resting = Some(task);
        }
        Ok(!resting)
    };
    client.follow(on_open, |event| {
        let result = match event.name.as_str() {
            "output" if event.data.get("id").and_then(|v| v.as_str()) == Some(id.as_str()) => {
                let line = event
                    .data
                    .get("line")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let prefix = match event.data.get("stream").and_then(|v| v.as_str()) {
                    Some("stderr") => "[stderr] ",
                    _ => "",
                };
                writeln!(out, "{prefix}{line}").map(|()| true)
            }
            "task" => match event
                .data
                .get("task")
                .cloned()
                .map(serde_json::from_value::<Task>)
            {
                Some(Ok(task)) if &task.id == id && output::is_resting(task.state) => {
                    output::resting_line(out, &task).map(|()| false)
                }
                _ => Ok(true),
            },
            _ => Ok(true),
        };
        match result {
            Ok(keep_going) => keep_going,
            Err(err) => {
                write_error = Some(err);
                false
            }
        }
    })?;
    if let Some(err) = write_error {
        return Err(err.into());
    }
    if let Some(task) = already_resting {
        output::resting_line(out, &task)?;
    }
    Ok(())
}

fn print_json<T: serde::Serialize>(out: &mut impl Write, value: &T) -> Result<(), CliError> {
    let text =
        serde_json::to_string_pretty(value).map_err(|err| CliError::Io(io::Error::other(err)))?;
    writeln!(out, "{text}")?;
    Ok(())
}
