#![forbid(unsafe_code)]

use std::process::Command;

#[path = "fixtures/schema.rs"]
mod schema;

fn log_line(bytes: &[u8]) -> serde_json::Value {
    let text = std::str::from_utf8(bytes).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.ends_with('\n'));
    let line = text.trim_end_matches('\n');
    assert!(schema::valid(line), "{line}");
    serde_json::from_str(line).unwrap()
}

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
            .env_remove("RUST_LOG")
            .args(args)
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let v = log_line(&output.stderr);
        assert_eq!(v["event"], "fatal");
        assert_eq!(v["level"], "error");
        assert_eq!(
            v["err"],
            "usage: oroshi --version | oroshi config check [--config] <path>"
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
            .env_remove("RUST_LOG")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stdout.is_empty());
        let v = log_line(&output.stderr);
        assert_eq!(v["event"], "start");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
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
    let v = log_line(&output.stderr);
    assert_eq!(v["event"], "config-invalid");
    assert!(v["err"].as_str().unwrap().contains(path));
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
    let v = log_line(&output.stderr);
    assert_eq!(v["event"], "config-invalid");
    assert!(v["err"].as_str().unwrap().contains("unknown field"));
}

#[test]
fn log_level_is_validated_even_with_rust_log_override() {
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .env("RUST_LOG", "trace")
        .args([
            "config",
            "check",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/invalid-log.toml"
            ),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let v = log_line(&output.stderr);
    assert_eq!(v["event"], "config-invalid");
    assert!(v["err"].as_str().unwrap().contains("log.level"));
}

#[test]
fn rust_log_filters_start_but_not_config_errors() {
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../nix/example.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .env("RUST_LOG", "off")
        .args(["config", "check", example])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .env("RUST_LOG", "off")
        .args(["config", "check", "missing.toml"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(log_line(&output.stderr)["event"], "config-invalid");
}
