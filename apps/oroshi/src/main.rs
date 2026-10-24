#![forbid(unsafe_code)]

pub mod config;
mod log;

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, PartialEq)]
enum Command {
    Version,
    ConfigCheck(PathBuf),
}

fn command(args: &[OsString]) -> Result<Command, &'static str> {
    match args {
        [version] if version == OsStr::new("--version") => Ok(Command::Version),
        [config, check, path] if config == "config" && check == "check" && path != "--config" => {
            Ok(Command::ConfigCheck(path.into()))
        }
        [config, check, flag, path]
            if config == "config" && check == "check" && flag == "--config" =>
        {
            Ok(Command::ConfigCheck(path.into()))
        }
        _ => Err("usage: oroshi --version | oroshi config check [--config] <path>"),
    }
}

fn run(args: &[OsString]) -> Result<ExitCode, String> {
    match command(args)? {
        Command::Version => println!("oroshi {}", env!("CARGO_PKG_VERSION")),
        Command::ConfigCheck(path) => {
            let result = (|| {
                let input = std::fs::read_to_string(&path)
                    .map_err(|error| format!("config {}: {error}", path.display()))?;
                config::Config::parse(&input)
            })();
            let config = match result {
                Ok(config) => config,
                Err(error) => {
                    log::error("config-invalid", &error);
                    return Ok(ExitCode::FAILURE);
                }
            };
            log::init(&config.log.level)?;
            tracing::info!(event = "start", version = env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    log::install_panic_hook();
    match run(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
        Ok(code) => code,
        Err(error) => {
            log::error("fatal", &error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, &'static str> {
        command(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn accepts_version() {
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
    }

    #[test]
    fn accepts_config_check_with_positional_or_flagged_path() {
        for args in [
            vec!["config", "check", "example.toml"],
            vec!["config", "check", "--config", "example.toml"],
        ] {
            assert_eq!(
                parse(&args),
                Ok(Command::ConfigCheck("example.toml".into()))
            );
        }
    }

    #[test]
    fn refuses_missing_unknown_or_extra_arguments() {
        for args in [
            vec![],
            vec!["serve"],
            vec!["--version", "extra"],
            vec!["config", "check"],
            vec!["config", "check", "--config"],
            vec!["config", "check", "file", "extra"],
            vec!["config", "check", "--config", "file", "extra"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
}
