//! Ayudantes compartidos por los tests de integración: repos de git de verdad.

use std::path::Path;
use std::process::Command;

pub fn git(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(cwd)
        // Identidad de prueba solo para estos repos temporales
        .args([
            "-c",
            "user.name=fleet-test",
            "-c",
            "user.email=fleet-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn commit_file(
    repo: &Path,
    name: &str,
    content: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    std::fs::write(repo.join(name), content)?;
    git(repo, &["add", name])?;
    git(repo, &["commit", "--quiet", "-m", &format!("add {name}")])?;
    git(repo, &["rev-parse", "HEAD"])
}
