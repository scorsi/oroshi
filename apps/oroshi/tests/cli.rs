#![forbid(unsafe_code)]

use std::process::Command;

#[test]
fn prints_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("oroshi {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_arguments_fail_with_usage() {
    for args in [vec![], vec!["serve"], vec!["--version", "extra"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .args(args)
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"usage: oroshi --version | oroshi config check [--config] <path>\n"
        );
    }
}

#[test]
fn checks_example_with_both_path_forms() {
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../nix/example.toml");
    for args in [
        vec!["config", "check", example],
        vec!["config", "check", "--config", example],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn config_check_reports_file_errors() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml/not-a-config");
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .args(["config", "check", path])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr).unwrap().contains(path));
}

#[test]
fn config_check_reports_validation_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .args([
            "config",
            "check",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unknown field")
    );
}
