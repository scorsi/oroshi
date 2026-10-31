#![forbid(unsafe_code)]
//! `oroshi gc` and `oroshi nix drop` against a temporary state directory.

use std::path::PathBuf;
use std::process::{Command, Output};

use oroshi_nix::NarInfo;
use oroshi_store::{BlobStore, Database};

#[path = "fixtures/schema.rs"]
mod schema;

const FIXTURE: &str = include_str!(
    "../../../crates/oroshi-nix/tests/fixtures/narinfo/cache-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo"
);
const DAY: i64 = 86_400;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn store_path(i: u32) -> String {
    format!("/nix/store/{i:032}-p{i}")
}

struct State {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
}

/// A state directory initialised as the server would; `storage` adds `[storage]` keys.
fn state(storage: &str) -> State {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    std::fs::create_dir(&root).unwrap();
    drop(BlobStore::open(&root).unwrap());
    drop(Database::open(&root).unwrap());
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!("[storage]\ndir = {root:?}\n{storage}\n[retention]\ndays = 30\n"),
    )
    .unwrap();
    State {
        _dir: dir,
        root,
        config,
    }
}

impl State {
    fn handles(&self) -> (BlobStore, Database) {
        (
            BlobStore::open_existing(&self.root).unwrap(),
            Database::open_existing(&self.root).unwrap(),
        )
    }

    /// Upload path `i` with a NAR of `size` bytes, registered and last accessed at `at`.
    fn add(&self, i: u32, references: &[u32], at: i64, size: usize) -> [u8; 32] {
        self.add_nar(i, references, at, &vec![i as u8; size])
    }

    fn add_nar(&self, i: u32, references: &[u32], at: i64, nar: &[u8]) -> [u8; 32] {
        let (blobs, database) = self.handles();
        let blob = blobs.write(nar, u64::MAX, None).unwrap();
        let url = format!("nar/{}.nar", oroshi_nix::encode_base32(&blob.hash));
        database.register_nar(&url, &blob, at).unwrap();
        let mut info: NarInfo = FIXTURE.parse().unwrap();
        info.store_path = store_path(i);
        info.url = url;
        info.file_hash = Some(blob.hash);
        info.file_size = Some(blob.size);
        info.references = references.iter().map(|&r| store_path(r)).collect();
        database.commit_narinfo(&info, &[], "upload", at).unwrap();
        blob.hash
    }

    fn paths(&self) -> Vec<String> {
        let (_, database) = self.handles();
        let mut paths: Vec<String> = (0..10)
            .map(store_path)
            .filter(|path| {
                let hash = &path["/nix/store/".len()..][..32];
                database.narinfo(hash).unwrap().is_some()
            })
            .collect();
        paths.sort();
        paths
    }

    fn blob_exists(&self, hash: &[u8; 32]) -> bool {
        self.handles().0.open_blob(hash).is_ok()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    /// `args --config <config> rest`.
    fn run_with(&self, args: &[&str], rest: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oroshi"))
            .env_remove("RUST_LOG")
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .args(rest)
            .output()
            .unwrap()
    }

    fn drop(&self, path: u32, with_referrers: bool) -> Output {
        let path = store_path(path);
        if with_referrers {
            self.run_with(&["nix", "drop"], &["--with-referrers", &path])
        } else {
            self.run_with(&["nix", "drop"], &[&path])
        }
    }
}

fn log_lines(output: &Output) -> Vec<serde_json::Value> {
    let text = std::str::from_utf8(&output.stderr).unwrap();
    text.lines()
        .map(|line| {
            assert!(schema::valid(line), "{line}");
            serde_json::from_str(line).unwrap()
        })
        .collect()
}

fn gc_run(output: &Output) -> serde_json::Value {
    let lines = log_lines(output);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["event"], "gc-run");
    lines[0].clone()
}

#[test]
fn gc_removes_an_expired_closure_and_caps_the_rest() {
    // 150 bytes allowed: after expiry 200 bytes remain, the older fresh path goes.
    let state = state("max_bytes = 150\ngc_target_ratio = 1.0");
    let old = now() - 40 * DAY;
    let expired_dep = state.add(1, &[1], old, 10);
    let expired = state.add(2, &[1], old, 20);
    let older = state.add(3, &[], now() - DAY, 100);
    let fresh = state.add(4, &[], now(), 100);

    let output = state.run(&["gc"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    let line = gc_run(&output);
    assert_eq!(line["level"], "info");
    assert_eq!(line["deleted_entries"], 3);
    assert_eq!(line["deleted_blobs"], 3);
    assert_eq!(line["freed_bytes"], 130);
    assert!(line["dur_us"].is_u64());

    assert_eq!(state.paths(), [store_path(4)]);
    for hash in [expired_dep, expired, older] {
        assert!(!state.blob_exists(&hash));
    }
    assert!(state.blob_exists(&fresh));
    let gc_state = state.handles().1.gc_state().unwrap();
    assert_eq!((gc_state.runs_success, gc_state.runs_error), (1, 0));
    assert_eq!(gc_state.freed_bytes, 130);
    assert!(gc_state.last_success_at.unwrap() >= now() - 60);
}

#[test]
fn gc_keeps_a_fresh_closure_and_a_second_run_frees_nothing() {
    let state = state("");
    let dep = state.add(1, &[], now() - 40 * DAY, 10);
    let top = state.add(2, &[1], now(), 10);
    for runs in 1..=2 {
        let output = state.run(&["gc"]);
        assert!(output.status.success(), "{output:?}");
        let line = gc_run(&output);
        assert_eq!(line["deleted_entries"], 0);
        assert_eq!(line["freed_bytes"], 0);
        assert_eq!(state.handles().1.gc_state().unwrap().runs_success, runs);
    }
    assert_eq!(state.paths(), [store_path(1), store_path(2)]);
    assert!(state.blob_exists(&dep) && state.blob_exists(&top));
}

#[test]
fn a_concurrent_gc_is_refused_with_an_error_gc_run() {
    let state = state("");
    let expired = state.add(1, &[], now() - 40 * DAY, 10);
    let lock = std::fs::File::create(state.root.join("gc.lock")).unwrap();
    lock.try_lock().unwrap();
    let output = state.run(&["gc"]);
    assert_eq!(output.status.code(), Some(1));
    let line = gc_run(&output);
    assert_eq!(line["level"], "error");
    assert!(line["err"].as_str().unwrap().contains("gc lock"), "{line}");
    assert_eq!(line["deleted_entries"], 0);
    assert!(state.blob_exists(&expired));
    assert_eq!(state.paths(), [store_path(1)]);
    // The server's lock does not block gc.
    drop(lock);
    let server = std::fs::File::create(state.root.join("oroshi.lock")).unwrap();
    server.try_lock().unwrap();
    assert!(state.run(&["gc"]).status.success());
    assert!(state.paths().is_empty());
}

#[test]
fn gc_refuses_another_schema_version_without_creating_or_migrating() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    std::fs::create_dir_all(root.join("db")).unwrap();
    // An empty file is a SQLite database at user_version 0.
    std::fs::write(root.join("db/oroshi.sqlite"), b"").unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!("[storage]\ndir = {root:?}\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oroshi"))
        .env_remove("RUST_LOG")
        .args(["gc", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let line = gc_run(&output);
    assert_eq!(line["level"], "error");
    assert!(line["err"].as_str().unwrap().contains("schema version 0"));
    assert_eq!(std::fs::read(root.join("db/oroshi.sqlite")).unwrap(), b"");
    assert!(!root.join("LAYOUT").exists());
}

fn one_line(output: &Output) -> serde_json::Value {
    let lines = log_lines(output);
    assert_eq!(lines.len(), 1, "{lines:?}");
    lines[0].clone()
}

fn refused(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let line = one_line(output);
    assert_eq!(
        (&line["event"], &line["level"]),
        (&"fatal".into(), &"error".into())
    );
    line["err"].as_str().unwrap().to_owned()
}

#[test]
fn a_dropped_path_can_be_uploaded_again_with_another_nar() {
    let state = state("");
    let first = state.add(1, &[1], now(), 10);
    let output = state.drop(1, false);
    assert!(output.status.success(), "{output:?}");
    let line = one_line(&output);
    assert_eq!(line["event"], "narinfo-dropped");
    assert_eq!(line["level"], "info");
    assert_eq!(line["name"], store_path(1)["/nix/store/".len()..]);
    assert_eq!(line["referrers"], 0);
    assert!(state.paths().is_empty());
    // The NAR and blob are left to the sweep.
    assert!(state.blob_exists(&first));
    let second = state.add_nar(1, &[], now(), b"another NAR");
    assert_ne!(first, second);
    let content = state
        .handles()
        .1
        .narinfo(&format!("{:032}", 1))
        .unwrap()
        .unwrap();
    assert!(
        content.contains(&oroshi_nix::encode_base32(&second)),
        "{content}"
    );
}

#[test]
fn a_referenced_path_drops_only_with_its_referrers() {
    let state = state("");
    state.add(1, &[1], now(), 10);
    state.add(2, &[1], now(), 10);
    state.add(3, &[2], now(), 10);
    state.add(4, &[], now(), 10);
    let err = refused(&state.drop(1, false));
    assert!(
        err.contains(&store_path(2)) && err.contains("--with-referrers"),
        "{err}"
    );
    assert_eq!(state.paths().len(), 4);
    let output = state.drop(1, true);
    assert!(output.status.success(), "{output:?}");
    let line = one_line(&output);
    assert_eq!(line["event"], "narinfo-dropped");
    assert_eq!(line["referrers"], 2);
    assert_eq!(state.paths(), [store_path(4)]);
}

#[test]
fn rooted_absent_or_locked_drops_are_refused_without_dropping() {
    let state = state("");
    state.add(1, &[], now(), 10);
    state.add(2, &[1], now(), 10);
    let (_, database) = state.handles();
    database
        .set_root("release", &[store_path(2)].into())
        .unwrap();
    let err = refused(&state.drop(1, true));
    assert!(
        err.contains(&format!("{} (root release)", store_path(2))),
        "{err}"
    );
    let err = refused(&state.drop(2, false));
    assert!(err.contains("root release"), "{err}");
    assert!(refused(&state.drop(9, false)).contains("no narinfo"));
    let lock = std::fs::File::create(state.root.join("gc.lock")).unwrap();
    lock.try_lock().unwrap();
    assert!(database.delete_root("release").unwrap());
    assert!(refused(&state.drop(2, false)).contains("gc lock"));
    drop(lock);
    assert_eq!(state.paths(), [store_path(1), store_path(2)]);
    assert!(state.drop(2, false).status.success());
    assert_eq!(state.paths(), [store_path(1)]);
}
