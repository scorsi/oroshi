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
            "usage: oroshi --version | oroshi config check [--config] <path> | oroshi serve --config <path>"
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
    let path = std::env::temp_dir().join(format!("oroshi-serve-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        format!("[server]\nlisten = '{}'\n", occupied.local_addr().unwrap()),
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
    std::fs::write(
        &config,
        "[server]\nlisten = '127.0.0.1:0'\n[metrics]\nlisten = '127.0.0.1:0'\n",
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
        let events: Vec<_> = lines
            .iter()
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
