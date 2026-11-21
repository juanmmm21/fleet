//! Argumentos comunes a los dos binarios mientras no tengan subcomandos propios.

/// Lo que pide la línea de órdenes de un binario de fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasicCommand {
    Version,
    Help,
}

/// Error al leer los argumentos: el binario lo enseña junto con la ayuda.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("missing command")]
    Missing,
    #[error("unknown argument `{0}`")]
    Unknown(String),
}

/// Lee los argumentos (sin el nombre del programa).
pub fn parse_basic<I, S>(args: I) -> Result<BasicCommand, ArgsError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let first = args.next().ok_or(ArgsError::Missing)?;
    let command = match first.as_str() {
        "-V" | "--version" => BasicCommand::Version,
        "-h" | "--help" => BasicCommand::Help,
        _ => return Err(ArgsError::Unknown(first)),
    };
    match args.next() {
        Some(extra) => Err(ArgsError::Unknown(extra)),
        None => Ok(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_and_help_flags() {
        assert_eq!(parse_basic(["--version"]), Ok(BasicCommand::Version));
        assert_eq!(parse_basic(["-V"]), Ok(BasicCommand::Version));
        assert_eq!(parse_basic(["--help"]), Ok(BasicCommand::Help));
    }

    #[test]
    fn rejects_missing_unknown_and_extra_arguments() {
        assert_eq!(parse_basic(Vec::<String>::new()), Err(ArgsError::Missing));
        assert_eq!(
            parse_basic(["run"]),
            Err(ArgsError::Unknown("run".to_owned()))
        );
        assert_eq!(
            parse_basic(["-V", "x"]),
            Err(ArgsError::Unknown("x".to_owned()))
        );
    }
}
