#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

fn version_requested(mut args: impl Iterator<Item = OsString>) -> bool {
    args.next().as_deref() == Some(OsStr::new("--version")) && args.next().is_none()
}

fn main() -> ExitCode {
    if version_requested(std::env::args_os().skip(1)) {
        println!("oroshi {}", env!("CARGO_PKG_VERSION"));
        ExitCode::SUCCESS
    } else {
        eprintln!("usage: oroshi --version");
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_version() {
        assert!(version_requested([OsString::from("--version")].into_iter()));
    }

    #[test]
    fn refuses_missing_or_unknown_argument() {
        assert!(!version_requested(std::iter::empty()));
        assert!(!version_requested([OsString::from("serve")].into_iter()));
    }

    #[test]
    fn refuses_extra_argument() {
        assert!(!version_requested(
            ["--version", "extra"].map(OsString::from).into_iter()
        ));
    }
}
