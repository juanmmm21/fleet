//! Dónde corre una tarea con `"node": "auto"` (ADR 0002): lo de Xcode al Mac, lo de Ollama al
//! nodo con GPU y el resto al que tenga menos trabajo.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agent::AgentProvider;

/// Lo que importa de un nodo para repartir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapacity {
    pub name: String,
    /// `linux` o `macos` (`std::env::consts::OS`).
    pub os: String,
    /// Tiene GPU para Ollama (`fleetd --gpu`).
    pub gpu: bool,
    /// Tareas en `provisioning`, `agent` o `preflight`.
    pub live_tasks: usize,
    pub local: bool,
}

/// Qué pide una tarea a la máquina.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Xcode,
    Gpu,
    Anything,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("the task needs Xcode and no macOS node is online")]
    NoMac,
    #[error("the task runs on Ollama and no node with a GPU (`fleetd --gpu`) is online")]
    NoGpu,
    #[error("no fleet node is online")]
    NoNode,
}

/// Qué necesita una tarea: mira el checkout del repo si está en esta máquina.
#[must_use]
pub fn need(repo: Option<&Path>, agent: &AgentProvider) -> Need {
    if repo.is_some_and(needs_xcode) {
        Need::Xcode
    } else if matches!(agent, AgentProvider::Ollama { .. }) {
        Need::Gpu
    } else {
        Need::Anything
    }
}

/// Un proyecto de Xcode o un paquete de Swift para plataformas de Apple, arriba o un nivel
/// más abajo (`ios/App.xcodeproj`).
#[must_use]
pub fn needs_xcode(repo: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(repo) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_xcode_project(&path) || is_apple_package(&path) {
            return true;
        }
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden && path.is_dir() {
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            if inner.flatten().any(|entry| is_xcode_project(&entry.path())) {
                return true;
            }
        }
    }
    false
}

fn is_xcode_project(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "xcodeproj" || ext == "xcworkspace")
}

fn is_apple_package(path: &Path) -> bool {
    if path.file_name().is_none_or(|name| name != "Package.swift") {
        return false;
    }
    // Un paquete de Swift sin plataformas de Apple compila en Linux; con ellas, mejor el Mac
    std::fs::read_to_string(path).is_ok_and(|manifest| {
        [".iOS(", ".macOS(", ".watchOS(", ".tvOS(", ".visionOS("]
            .iter()
            .any(|platform| manifest.contains(platform))
    })
}

/// El nodo donde va la tarea, entre los que están en línea.
pub fn choose(need: Need, online: &[NodeCapacity]) -> Result<String, PlacementError> {
    let capable: Vec<&NodeCapacity> = match need {
        Need::Xcode => online.iter().filter(|node| node.os == "macos").collect(),
        Need::Gpu => online.iter().filter(|node| node.gpu).collect(),
        Need::Anything => online.iter().collect(),
    };
    // Menos tareas vivas; a igualdad, el propio (sin red de por medio) y luego por nombre
    capable
        .into_iter()
        .min_by(|a, b| {
            a.live_tasks
                .cmp(&b.live_tasks)
                .then(b.local.cmp(&a.local))
                .then(a.name.cmp(&b.name))
        })
        .map(|node| node.name.clone())
        .ok_or(match need {
            Need::Xcode => PlacementError::NoMac,
            Need::Gpu => PlacementError::NoGpu,
            Need::Anything => PlacementError::NoNode,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, os: &str, gpu: bool, live_tasks: usize, local: bool) -> NodeCapacity {
        NodeCapacity {
            name: name.to_owned(),
            os: os.to_owned(),
            gpu,
            live_tasks,
            local,
        }
    }

    fn fleet() -> Vec<NodeCapacity> {
        vec![
            node("pc", "linux", true, 2, true),
            node("mac", "macos", false, 0, false),
        ]
    }

    #[test]
    fn xcode_goes_to_the_mac_and_ollama_to_the_gpu() {
        assert_eq!(choose(Need::Xcode, &fleet()), Ok("mac".to_owned()));
        assert_eq!(choose(Need::Gpu, &fleet()), Ok("pc".to_owned()));
    }

    #[test]
    fn the_rest_goes_to_the_least_busy_and_ties_stay_home() {
        assert_eq!(choose(Need::Anything, &fleet()), Ok("mac".to_owned()));
        let even = vec![
            node("mac", "macos", false, 1, false),
            node("pc", "linux", true, 1, true),
        ];
        assert_eq!(choose(Need::Anything, &even), Ok("pc".to_owned()));
    }

    #[test]
    fn without_a_capable_node_it_says_why() {
        let pc_only = vec![node("pc", "linux", true, 0, true)];
        assert_eq!(choose(Need::Xcode, &pc_only), Err(PlacementError::NoMac));
        let mac_only = vec![node("mac", "macos", false, 0, true)];
        assert_eq!(choose(Need::Gpu, &mac_only), Err(PlacementError::NoGpu));
        assert_eq!(choose(Need::Anything, &[]), Err(PlacementError::NoNode));
    }

    #[test]
    fn spots_xcode_projects_and_apple_packages() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(plain.join("src"))?;
        std::fs::write(plain.join("Cargo.toml"), "[package]\n")?;
        assert!(!needs_xcode(&plain));

        let app = dir.path().join("app");
        std::fs::create_dir_all(app.join("ios/App.xcodeproj"))?;
        assert!(needs_xcode(&app));

        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join("App.xcworkspace"))?;
        assert!(needs_xcode(&workspace));

        let linux_package = dir.path().join("swift-linux");
        std::fs::create_dir_all(&linux_package)?;
        std::fs::write(
            linux_package.join("Package.swift"),
            "let package = Package(name: \"x\")\n",
        )?;
        assert!(!needs_xcode(&linux_package));
        std::fs::write(
            linux_package.join("Package.swift"),
            "let package = Package(name: \"x\", platforms: [.iOS(.v17)])\n",
        )?;
        assert!(needs_xcode(&linux_package));

        // Lo de dentro de `.git` o de carpetas ocultas no cuenta
        let hidden = dir.path().join("hidden");
        std::fs::create_dir_all(hidden.join(".cache/Old.xcodeproj"))?;
        assert!(!needs_xcode(&hidden));
        assert!(!needs_xcode(&dir.path().join("missing")));
        Ok(())
    }

    #[test]
    fn the_need_comes_from_the_repo_first_and_then_the_agent() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("App.xcodeproj"))?;
        let ollama = AgentProvider::Ollama {
            model: "qwen3-coder:30b".to_owned(),
        };
        let claude = AgentProvider::Claude { model: None };
        assert_eq!(need(Some(dir.path()), &ollama), Need::Xcode);
        assert_eq!(need(None, &ollama), Need::Gpu);
        assert_eq!(need(None, &claude), Need::Anything);
        Ok(())
    }
}
