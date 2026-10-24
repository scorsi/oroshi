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

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let v = log_line(&output.stderr);
        assert_eq!(v["event"], "fatal");
        assert_eq!(v["level"], "error");
        assert_eq!(
            v["err"],
            "usage: oroshi --version | oroshi token hash | oroshi config check [--config] <path> | oroshi serve --config <path>"
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
fn config_check_stays_quiet_but_reports_errors_with_logging_off() {
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

#[test]
fn serve_emits_start_before_reporting_bind_failure() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\nlisten = '{}'\n[storage]\ndir = {:?}\n",
            occupied.local_addr().unwrap(),
            directory.path().join("state")
        ),
    )
    .unwrap();
    for filter in ["info", "off"] {
        let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .env("RUST_LOG", filter)
            .args(["serve", "--config"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let lines: Vec<_> = output
            .stderr
            .split_inclusive(|&b| b == b'\n')
            .map(log_line)
            .collect();
        if filter == "info" {
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0]["event"], "start");
            assert_eq!(lines[0]["version"], env!("CARGO_PKG_VERSION"));
        } else {
            assert_eq!(lines.len(), 1);
        }
        assert_eq!(lines.last().unwrap()["event"], "fatal");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn notify_socket_and_lifecycle_events_follow_sigterm_and_sigint() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::os::unix::net::UnixDatagram;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct Service(Child);
    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let directory =
        Directory(std::env::temp_dir().join(format!("oroshi-lifecycle-{}", std::process::id())));
    std::fs::create_dir(&directory.0).unwrap();
    let socket = directory.0.join("notify.sock");
    let receiver = UnixDatagram::bind(&socket).unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let config = directory.0.join("config.toml");
    let metrics_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        &config,
        format!("[server]\nlisten = '127.0.0.1:0'\n[metrics]\nlisten = '127.0.0.1:{metrics_port}'\n[storage]\ndir = '{}'\n", directory.0.join("state").display()),
    )
    .unwrap();
    for signal in ["-TERM", "-INT"] {
        let mut service = Service(
            Command::new(env!("CARGO_BIN_EXE_oroshi"))
                .env("RUST_LOG", "info")
                .env("NOTIFY_SOCKET", &socket)
                .args(["serve", "--config"])
                .arg(&config)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut buffer = [0; 64];
        let size = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..size], b"READY=1");
        assert!(
            Command::new("kill")
                .args([signal, &service.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let size = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..size], b"STOPPING=1");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = service.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "service failed to stop");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "{status}");
        let mut stderr = Vec::new();
        service
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        let lines: Vec<_> = stderr
            .split_inclusive(|&b| b == b'\n')
            .map(log_line)
            .collect();
        let migrations: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "migration-applied")
            .collect();
        if signal == "-TERM" {
            let versions: Vec<_> = migrations.iter().map(|line| &line["version"]).collect();
            assert_eq!(versions, [1, 2]);
        } else {
            assert!(migrations.is_empty());
        }
        let events: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] != "migration-applied")
            .map(|line| line["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            events,
            [
                "start",
                "listening",
                "metrics-listening",
                "ready",
                "stopping"
            ]
        );
        // Both ephemeral listeners must have been bound before readiness.
        for line in &lines[1..=2] {
            let addr: std::net::SocketAddr = line["addr"].as_str().unwrap().parse().unwrap();
            assert_ne!(addr.port(), 0);
            assert!(TcpStream::connect(addr).is_err());
        }
    }
}

#[test]
fn token_hash_hashes_stdin_bytes_exactly() {
    use std::io::Write;
    use std::process::Stdio;

    for (secret, expected) in [
        (
            b"abc".as_slice(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            b"abc\n".as_slice(),
            "edeaaff3f1774ad2888673770c6d64097e391bc362d7d6fb34982ddf0efd18cb",
        ),
        (
            b"".as_slice(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .args(["token", "hash"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(secret).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, format!("{expected}\n").as_bytes());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn token_hash_rejects_secret_arguments_without_echoing_them() {
    for args in [
        vec!["token", "hash", "private-secret"],
        vec!["token", "hash", "--secret", "private-secret"],
        vec!["token", "hash", "--secret=private-secret"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let line = log_line(&output.stderr);
        assert!(line["err"].as_str().unwrap().starts_with("usage:"));
        assert!(
            !String::from_utf8(output.stderr)
                .unwrap()
                .contains("private-secret")
        );
    }
}

#[test]
fn config_invalid_logs_never_echo_inline_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let secret = "token-looking-secret-0123456789";
    for input in [
        format!("[server]\nlisten = '{secret}'"),
        format!("[log]\nlevel = 'oroshi={secret}'"),
        format!("unexpected = '{secret}'"),
        format!("[nix]\ntrusted_keys = ['{secret}']"),
    ] {
        std::fs::write(&path, input).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .args(["config", "check", "--config"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(log_line(&output.stderr)["event"], "config-invalid");
        assert!(!String::from_utf8(output.stderr).unwrap().contains(secret));
    }
}

#[test]
fn store_open_failure_exits_without_ready_notification() {
    use std::os::unix::net::UnixDatagram;
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("notify.sock");
    let receiver = UnixDatagram::bind(&socket).unwrap();
    receiver.set_nonblocking(true).unwrap();
    for failure in ["layout", "database"] {
        let state = directory.path().join(failure);
        std::fs::create_dir(&state).unwrap();
        if failure == "layout" {
            std::fs::write(state.join("LAYOUT"), "unsupported").unwrap();
        } else {
            // A `db` file is refused earlier by the state-directory check; break the database itself.
            std::fs::create_dir_all(state.join("db/oroshi.sqlite")).unwrap();
        }
        let config = directory.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[server]\nlisten = '127.0.0.1:0'\n[storage]\ndir = {:?}\n",
                state
            ),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .env("NOTIFY_SOCKET", &socket)
            .env("RUST_LOG", "info")
            .args(["serve", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let lines: Vec<_> = output
            .stderr
            .split_inclusive(|&b| b == b'\n')
            .map(log_line)
            .collect();
        assert!(lines.iter().any(|line| line["event"] == "listening"));
        assert_eq!(lines.last().unwrap()["event"], "fatal");
        assert!(!lines.iter().any(|line| line["event"] == "ready"));
        let error = receiver.recv(&mut [0; 64]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }
}

#[test]
fn help_prints_usage_on_stdout_and_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert!(
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .starts_with("usage: oroshi ")
    );
}

#[test]
fn second_serve_exits_fatal_without_removing_in_flight_uploads() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(state.join("tmp")).unwrap();
    let lock = std::fs::File::create(state.join("oroshi.lock")).unwrap();
    lock.try_lock().unwrap();
    let upload = state.join("tmp/in-flight");
    std::fs::write(&upload, b"partial").unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!("[server]\nlisten = '127.0.0.1:0'\n[storage]\ndir = {state:?}"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .env("RUST_LOG", "off")
        .env_remove("NOTIFY_SOCKET")
        .args(["serve", "--config"])
        .arg(config)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let line = log_line(&output.stderr);
    assert_eq!(line["event"], "fatal");
    assert!(
        line["err"]
            .as_str()
            .unwrap()
            .contains("state directory lock")
    );
    assert_eq!(std::fs::read(upload).unwrap(), b"partial");
    assert!(!state.join("LAYOUT").exists());
}
