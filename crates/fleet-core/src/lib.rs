//! Núcleo de fleet: tipos y lógica sin red que comparten el demonio y la CLI.
#![forbid(unsafe_code)]

// Grupos de procesos, señales y enlaces simbólicos: fleet corre en Linux y macOS.
#[cfg(not(unix))]
compile_error!("fleet only supports Unix-like systems (Linux and macOS)");

pub mod agent;
pub mod cli;
pub mod integrate;
pub mod overlap;
pub mod placement;
pub mod preflight;
pub mod process;
pub mod recovery;
pub mod repo;
pub mod runner;
pub mod store;
pub mod tailnet;
pub mod task;
pub mod transcript;
pub mod worktree;

pub use agent::{AgentError, AgentPrograms, AgentProvider, Invocation};
pub use integrate::{IntegrationError, IntegrationOutcome};
pub use overlap::Overlap;
pub use placement::{Need, NodeCapacity, PlacementError};
pub use preflight::{ActPlan, PreflightConfig, PreflightError, PreflightPlan, PreflightResult};
pub use process::{OutputLine, ProcessError, ProcessOutput, Stream, StreamEnd, StreamOptions};
pub use recovery::{Recovered, Recovery, RecoveryError};
pub use repo::{RepoRef, RepoRefError};
pub use runner::{AgentReport, RunConfig, RunError};
pub use store::{Store, StoreError};
pub use tailnet::{Machine, TailnetError, TailnetStatus};
pub use task::{NewTaskError, Task, TaskId, TaskIdError, TaskSpec, TaskState, TransitionError};
pub use transcript::{AgentOutcome, AgentSummary, Transcript};
pub use worktree::{
    ChangeKind, Diff, FileChange, Removal, Worktree, WorktreeConfig, WorktreeError,
};

/// Versión de fleet, la misma para los tres crates del workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Línea de versión que imprimen los binarios con `--version`.
#[must_use]
pub fn version_line(binary: &str) -> String {
    format!("{binary} {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_binary_and_the_workspace_version() {
        assert_eq!(version_line("fleetd"), format!("fleetd {VERSION}"));
        assert!(!VERSION.is_empty());
    }
}
