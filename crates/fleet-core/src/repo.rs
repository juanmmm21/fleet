//! Cómo se nombra el repo de una tarea (ADR 0002): una ruta absoluta en la máquina que la
//! ejecuta, o solo el nombre del repo, que cada nodo busca en su raíz de repos. Así una tarea
//! para el Mac no necesita saber que allí la carpeta es `/Users/...` y no `/home/...`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepoRefError {
    #[error("the repo is empty")]
    Empty,
    #[error(
        "`{0}` is neither an absolute path nor a repo name (letters, digits, `.`, `_` and `-`)"
    )]
    Invalid(String),
}

/// Un repo tal como llega por la API.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum RepoRef {
    Path(PathBuf),
    Name(String),
}

impl RepoRef {
    pub fn parse(raw: &str) -> Result<Self, RepoRefError> {
        if raw.is_empty() {
            return Err(RepoRefError::Empty);
        }
        if raw.starts_with('/') {
            return Ok(Self::Path(PathBuf::from(raw)));
        }
        let valid = raw != "."
            && raw != ".."
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if valid {
            Ok(Self::Name(raw.to_owned()))
        } else {
            Err(RepoRefError::Invalid(raw.to_owned()))
        }
    }

    /// Ruta en esta máquina: la absoluta tal cual, o `<root>/<nombre>`.
    #[must_use]
    pub fn resolve(&self, root: &Path) -> PathBuf {
        match self {
            Self::Path(path) => path.clone(),
            Self::Name(name) => root.join(name),
        }
    }
}

impl TryFrom<String> for RepoRef {
    type Error = RepoRefError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<RepoRef> for String {
    fn from(repo: RepoRef) -> Self {
        match repo {
            RepoRef::Path(path) => path.to_string_lossy().into_owned(),
            RepoRef::Name(name) => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_stay_and_names_go_under_the_root() -> Result<(), RepoRefError> {
        let root = Path::new("/Users/juan/Desarrollo");
        assert_eq!(
            RepoRef::parse("/home/juan/Desarrollo/fleet")?.resolve(root),
            PathBuf::from("/home/juan/Desarrollo/fleet")
        );
        assert_eq!(
            RepoRef::parse("TunnelVision-iOS")?.resolve(root),
            PathBuf::from("/Users/juan/Desarrollo/TunnelVision-iOS")
        );
        assert_eq!(
            RepoRef::parse("my_repo.v2")?,
            RepoRef::Name("my_repo.v2".to_owned())
        );
        Ok(())
    }

    #[test]
    fn nothing_can_climb_out_of_the_root() {
        assert_eq!(RepoRef::parse(""), Err(RepoRefError::Empty));
        for bad in [
            ".",
            "..",
            "../etc",
            "a/b",
            "relative/path",
            "~/x",
            "name with space",
        ] {
            assert_eq!(
                RepoRef::parse(bad),
                Err(RepoRefError::Invalid(bad.to_owned())),
                "{bad}"
            );
        }
    }

    #[test]
    fn goes_through_json_as_a_plain_string() -> Result<(), Box<dyn std::error::Error>> {
        let repo: RepoRef = serde_json::from_str("\"fleet\"")?;
        assert_eq!(repo, RepoRef::Name("fleet".to_owned()));
        assert_eq!(serde_json::to_string(&repo)?, "\"fleet\"");
        assert!(serde_json::from_str::<RepoRef>("\"../x\"").is_err());
        Ok(())
    }
}
