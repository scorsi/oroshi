#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
mod gc;
mod log;
pub mod metrics;
mod nix;
mod server;

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: oroshi-server --version | oroshi-server token hash | oroshi-server config check [--config] <path> | oroshi-server serve --config <path> | oroshi-server gc --config <path> | oroshi-server nix drop --config <path> [--with-referrers] <store-path>";

#[derive(Debug, PartialEq)]
enum Command {
    Version,
    Help,
    TokenHash,
    ConfigCheck(PathBuf),
    Serve(PathBuf),
    Gc(PathBuf),
    NixDrop {
        config: PathBuf,
        store_path: String,
        with_referrers: bool,
    },
}

fn command(args: &[OsString]) -> Result<Command, &'static str> {
    match args {
        [help] if help == "--help" => Ok(Command::Help),
        [version] if version == OsStr::new("--version") => Ok(Command::Version),
        [token, hash] if token == "token" && hash == "hash" => Ok(Command::TokenHash),
        [serve, flag, path] if serve == "serve" && flag == "--config" => {
            Ok(Command::Serve(path.into()))
        }
        [gc, flag, path] if gc == "gc" && flag == "--config" => Ok(Command::Gc(path.into())),
        [nix, drop, flag, config, rest @ ..]
            if nix == "nix" && drop == "drop" && flag == "--config" =>
        {
            let (with_referrers, store_path) = match rest {
                [store_path] => (false, store_path),
                [option, store_path] if option == "--with-referrers" => (true, store_path),
                _ => return Err(USAGE),
            };
            Ok(Command::NixDrop {
                config: config.into(),
                store_path: store_path.to_str().ok_or(USAGE)?.to_owned(),
                with_referrers,
            })
        }
        [config, check, path] if config == "config" && check == "check" && path != "--config" => {
            Ok(Command::ConfigCheck(path.into()))
        }
        [config, check, flag, path]
            if config == "config" && check == "check" && flag == "--config" =>
        {
            Ok(Command::ConfigCheck(path.into()))
        }
        _ => Err(USAGE),
    }
}

async fn run(args: &[OsString]) -> Result<ExitCode, String> {
    let cmd = match command(args) {
        Ok(cmd) => cmd,
        Err(usage) => {
            log::error("fatal", usage);
            return Ok(ExitCode::from(2));
        }
    };
    let path = match &cmd {
        Command::ConfigCheck(path)
        | Command::Serve(path)
        | Command::Gc(path)
        | Command::NixDrop { config: path, .. } => path,
        Command::Help => {
            println!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        Command::Version => {
            println!("oroshi-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Command::TokenHash => {
            use sha2::{Digest, Sha256};
            use std::io::Read;

            let mut hash = Sha256::new();
            let mut buffer = [0; 4096];
            let mut stdin = std::io::stdin().lock();
            loop {
                let size = stdin
                    .read(&mut buffer)
                    .map_err(|error| format!("stdin: {error}"))?;
                if size == 0 {
                    break;
                }
                hash.update(&buffer[..size]);
            }
            println!("{:x}", hash.finalize());
            return Ok(ExitCode::SUCCESS);
        }
    };
    let result = (|| {
        let input = std::fs::read_to_string(path)
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
    match cmd {
        Command::Serve(_) => {
            log::init(&config.log.level)?;
            tracing::info!(event = "start", version = env!("CARGO_PKG_VERSION"));
            let _state_lock =
                server::lock_state(&config.storage.dir).map_err(|error| error.to_string())?;
            server::Server::bind(&config)
                .await
                .map_err(|error| error.to_string())?
                .run(&config)
                .await
                .map_err(|error| error.to_string())?;
        }
        Command::Gc(_) => {
            log::init(&config.log.level)?;
            if !gc::run(&config) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::NixDrop {
            store_path,
            with_referrers,
            ..
        } => {
            log::init(&config.log.level)?;
            gc::drop_narinfo(&config, &store_path, with_referrers)?;
        }
        _ => {}
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    log::install_panic_hook();
    match run(&std::env::args_os().skip(1).collect::<Vec<_>>()).await {
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
    fn accepts_serve_with_config_path() {
        assert_eq!(
            parse(&["serve", "--config", "example.toml"]),
            Ok(Command::Serve("example.toml".into()))
        );
    }

    #[test]
    fn accepts_gc_with_config_path() {
        assert_eq!(
            parse(&["gc", "--config", "example.toml"]),
            Ok(Command::Gc("example.toml".into()))
        );
    }

    #[test]
    fn accepts_nix_drop_with_or_without_referrers() {
        let path = "/nix/store/00000000000000000000000000000000-example";
        for (args, with_referrers) in [
            (vec!["nix", "drop", "--config", "example.toml", path], false),
            (
                vec![
                    "nix",
                    "drop",
                    "--config",
                    "example.toml",
                    "--with-referrers",
                    path,
                ],
                true,
            ),
        ] {
            assert_eq!(
                parse(&args),
                Ok(Command::NixDrop {
                    config: "example.toml".into(),
                    store_path: path.into(),
                    with_referrers,
                })
            );
        }
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
            vec!["serve", "example.toml"],
            vec!["serve", "--config"],
            vec!["serve", "--config", "file", "extra"],
            vec!["gc"],
            vec!["gc", "file"],
            vec!["gc", "--config", "file", "--dry-run"],
            vec!["nix", "drop", "--config", "file"],
            vec!["nix", "drop", "path"],
            vec!["nix", "drop", "--config", "file", "--dry-run", "path"],
            vec![
                "nix",
                "drop",
                "--config",
                "file",
                "path",
                "--with-referrers",
            ],
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
