use std::collections::HashSet;

use proptest::prelude::*;

use super::*;

const NOW: i64 = 10_000;

fn policy(max_bytes: Option<u64>) -> Policy {
    Policy {
        now: NOW,
        retention: 1000,
        orphan_grace: 100,
        max_bytes,
        gc_target_ratio: 1.0,
    }
}

/// Each path gets its own NAR and blob of `size` bytes, registered long ago.
fn snapshot(paths: &[(&str, i64, u64)], edges: &[(&str, &str)], rooted: &[&str]) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for (i, &(name, accessed, size)) in paths.iter().enumerate() {
        let url = format!("nar/{name}.nar");
        snapshot.paths.push(MarkedPath {
            store_path: name.into(),
            last_accessed_at: accessed,
            nar_url: url.clone(),
        });
        snapshot.nars.push(MarkedNar {
            url,
            blob: [i as u8; 32],
            registered_at: 0,
        });
        snapshot.blobs.push(([i as u8; 32], size));
    }
    snapshot.edges = edges
        .iter()
        .map(|&(from, to)| (from.into(), to.into()))
        .collect();
    snapshot.rooted = rooted.iter().map(|&path| path.into()).collect();
    snapshot
}

fn names(paths: &[MarkedPath]) -> Vec<&str> {
    paths.iter().map(|path| path.store_path.as_str()).collect()
}

#[test]
fn expired_chain_goes_referrer_first_and_a_self_edge_does_not_keep_it() {
    let old = NOW - 2000;
    let snapshot = snapshot(
        &[("c", old, 1), ("a", old, 1), ("b", old, 1)],
        &[("a", "b"), ("b", "c"), ("c", "c")],
        &[],
    );
    let plan = mark(&snapshot, &policy(None));
    assert_eq!(names(&plan.expired), ["a", "b", "c"]);
    assert!(plan.capped.is_empty());
}

#[test]
fn recent_access_and_roots_keep_their_whole_closure() {
    let old = NOW - 2000;
    let snapshot = snapshot(
        &[
            ("fresh", NOW - 10, 1),
            ("dep", old, 1),
            ("pinned", old, 1),
            ("pinned-dep", old, 1),
            ("stale", old, 1),
        ],
        &[("fresh", "dep"), ("pinned", "pinned-dep"), ("stale", "dep")],
        &["pinned"],
    );
    let plan = mark(&snapshot, &policy(None));
    assert_eq!(names(&plan.expired), ["stale"]);
}

#[test]
fn cap_takes_least_recently_accessed_unreferenced_paths_until_the_target() {
    // a refers to b; b is the oldest but stays while a remains.
    let snapshot = snapshot(
        &[
            ("a", NOW - 30, 10),
            ("b", NOW - 100, 10),
            ("c", NOW - 50, 10),
        ],
        &[("a", "b")],
        &[],
    );
    assert_eq!(names(&mark(&snapshot, &policy(Some(30))).capped), [""; 0]);
    assert_eq!(names(&mark(&snapshot, &policy(Some(25))).capped), ["c"]);
    assert_eq!(
        names(&mark(&snapshot, &policy(Some(15))).capped),
        ["c", "a"]
    );
    assert_eq!(
        names(&mark(&snapshot, &policy(Some(0))).capped),
        ["c", "a", "b"]
    );
    let mut ratio = policy(Some(30));
    ratio.gc_target_ratio = 0.5;
    assert_eq!(names(&mark(&snapshot, &ratio).capped), ["c", "a"]);
}

#[test]
fn cap_counts_expiry_shared_blobs_and_nar_rows_inside_the_grace() {
    let mut snapshot = snapshot(
        &[
            ("old", NOW - 2000, 10),
            ("x", NOW - 50, 10),
            ("y", NOW - 40, 0),
        ],
        &[],
        &[],
    );
    // y's NAR shares x's blob: x frees nothing while y remains.
    snapshot.nars[2].blob = snapshot.nars[1].blob;
    // An unused NAR row inside the grace keeps its 5-byte blob.
    snapshot.nars.push(MarkedNar {
        url: "nar/upload.nar".into(),
        blob: [9; 32],
        registered_at: NOW - 10,
    });
    snapshot.blobs.push(([9; 32], 5));
    // An unused blob row is freed by the sweep whatever the cap.
    snapshot.blobs.push(([8; 32], 1000));
    // Left after expiry and sweep: the shared blob (10) and the in-grace blob (5).
    let plan = mark(&snapshot, &policy(Some(15)));
    assert_eq!(names(&plan.expired), ["old"]);
    assert!(plan.capped.is_empty());
    // Removing x alone frees nothing, so y goes too; the in-grace blob stays counted.
    assert_eq!(
        names(&mark(&snapshot, &policy(Some(14))).capped),
        ["x", "y"]
    );
    assert_eq!(names(&mark(&snapshot, &policy(Some(4))).capped), ["x", "y"]);
}

#[test]
fn rooted_closures_are_never_capped_and_the_pass_stops_without_candidates() {
    let snapshot = snapshot(
        &[("root", NOW, 10), ("dep", NOW, 10), ("other", NOW, 10)],
        &[("root", "dep")],
        &["root", "root"],
    );
    assert_eq!(names(&mark(&snapshot, &policy(Some(0))).capped), ["other"]);
}

#[test]
fn snapshot_reads_paths_nars_blobs_edges_and_roots() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap().0;
    let writer = database.writer().unwrap();
    writer
        .execute_batch(
            "INSERT INTO blob VALUES (zeroblob(32), 7, 1);
             INSERT INTO nar VALUES ('nar/a.nar', zeroblob(32), 5);
             INSERT INTO narinfo VALUES (
                 '/nix/store/00000000000000000000000000000000-a',
                 '00000000000000000000000000000000', 'nar/a.nar', '', 'upload', 9
             );
             INSERT INTO reference VALUES (
                 '/nix/store/00000000000000000000000000000000-a',
                 '/nix/store/00000000000000000000000000000000-a'
             );
             INSERT INTO root VALUES ('r', '/nix/store/00000000000000000000000000000000-a');",
        )
        .unwrap();
    drop(writer);
    let snapshot = database.gc_snapshot().unwrap();
    let path = "/nix/store/00000000000000000000000000000000-a".to_string();
    assert_eq!(
        snapshot.paths,
        [MarkedPath {
            store_path: path.clone(),
            last_accessed_at: 9,
            nar_url: "nar/a.nar".into()
        }]
    );
    assert_eq!(
        snapshot.nars,
        [MarkedNar {
            url: "nar/a.nar".into(),
            blob: [0; 32],
            registered_at: 5
        }]
    );
    assert_eq!(snapshot.blobs, [([0; 32], 7)]);
    assert_eq!(snapshot.edges, [(path.clone(), path.clone())]);
    assert_eq!(snapshot.rooted, [path]);
}

/// Registered bytes after a sweep that follows deleting everything but `remaining`.
fn bytes_after(snapshot: &Snapshot, policy: &Policy, remaining: &HashSet<&str>) -> u64 {
    let used: HashSet<&str> = snapshot
        .paths
        .iter()
        .filter(|path| remaining.contains(path.store_path.as_str()))
        .map(|path| path.nar_url.as_str())
        .collect();
    let kept: HashSet<[u8; 32]> = snapshot
        .nars
        .iter()
        .filter(|nar| {
            used.contains(nar.url.as_str()) || policy.now - nar.registered_at < policy.orphan_grace
        })
        .map(|nar| nar.blob)
        .collect();
    snapshot
        .blobs
        .iter()
        .filter(|(hash, _)| kept.contains(hash))
        .map(|(_, size)| size)
        .sum()
}

fn closure(snapshot: &Snapshot, seeds: impl Iterator<Item = String>) -> HashSet<String> {
    let mut reached = HashSet::new();
    let mut stack: Vec<String> = seeds.collect();
    while let Some(path) = stack.pop() {
        if reached.insert(path.clone()) {
            stack.extend(
                snapshot
                    .edges
                    .iter()
                    .filter(|(from, _)| *from == path)
                    .map(|(_, to)| to.clone()),
            );
        }
    }
    reached
}

prop_compose! {
    /// A Nix-like graph: references only point to earlier paths (a DAG) or to the path itself.
    fn graphs()(count in 1usize..14)(
        accessed in prop::collection::vec(0i64..200, count),
        nar_of in prop::collection::vec(0usize..count, count),
        blob_of in prop::collection::vec(0u8..6, count),
        registered in prop::collection::vec(0i64..200, count),
        sizes in prop::collection::vec(0u64..50, 6),
        edges in prop::collection::vec((0usize..count, 0usize..count), 0..count * 2),
        rooted in prop::collection::vec(0usize..count, 0..3),
        retention in 1i64..200,
        orphan_grace in 1i64..200,
        max_bytes in prop::option::of(0u64..300),
        ratio in 0.05f64..=1.0,
    ) -> (Snapshot, Policy) {
        let name = |i: usize| format!("p{i:02}");
        let mut snapshot = Snapshot::default();
        for i in 0..accessed.len() {
            snapshot.paths.push(MarkedPath {
                store_path: name(i),
                last_accessed_at: accessed[i],
                nar_url: format!("nar/{}", nar_of[i]),
            });
            snapshot.nars.push(MarkedNar {
                url: format!("nar/{i}"),
                blob: [blob_of[i]; 32],
                registered_at: registered[i],
            });
        }
        snapshot.blobs = (0..6).map(|b| ([b as u8; 32], sizes[b])).collect();
        let mut unique: Vec<_> = edges
            .into_iter()
            .map(|(a, b)| (a.max(b), a.min(b)))
            .collect();
        unique.sort();
        unique.dedup();
        snapshot.edges = unique.into_iter().map(|(a, b)| (name(a), name(b))).collect();
        snapshot.rooted = rooted.into_iter().map(name).collect();
        let policy = Policy { now: 200, retention, orphan_grace, max_bytes, gc_target_ratio: ratio };
        (snapshot, policy)
    }
}

proptest! {
    #[test]
    fn mark_never_breaks_a_closure((snapshot, policy) in graphs()) {
        let plan = mark(&snapshot, &policy);
        let rooted = closure(&snapshot, snapshot.rooted.iter().cloned());
        let live = closure(
            &snapshot,
            snapshot
                .paths
                .iter()
                .filter(|path| policy.now - path.last_accessed_at < policy.retention)
                .map(|path| path.store_path.clone())
                .chain(rooted.iter().cloned()),
        );
        for path in &plan.expired {
            prop_assert!(!live.contains(&path.store_path), "live {} expired", path.store_path);
        }
        // Every expired candidate is selected (a DAG has no cycle to skip).
        prop_assert_eq!(plan.expired.len(), snapshot.paths.len() - live.len());

        let mut remaining: HashSet<&str> =
            snapshot.paths.iter().map(|path| path.store_path.as_str()).collect();
        for (step, path) in plan.expired.iter().chain(&plan.capped).enumerate() {
            prop_assert!(!rooted.contains(&path.store_path));
            let referrer = snapshot.edges.iter().find(|(from, to)| {
                *to == path.store_path && from != to && remaining.contains(from.as_str())
            });
            prop_assert!(referrer.is_none(), "{} still referenced by {:?}", path.store_path, referrer);
            if step >= plan.expired.len() {
                let target = policy.gc_target_ratio * policy.max_bytes.unwrap() as f64;
                prop_assert!(bytes_after(&snapshot, &policy, &remaining) as f64 > target);
            }
            prop_assert!(remaining.remove(path.store_path.as_str()));
        }

        // The cap pass stops at the target or when no candidate is left.
        if let Some(max_bytes) = policy.max_bytes {
            let target = policy.gc_target_ratio * max_bytes as f64;
            let candidate_left = remaining.iter().any(|path| {
                !rooted.contains(*path)
                    && !snapshot.edges.iter().any(|(from, to)| {
                        to == path && from != to && remaining.contains(from.as_str())
                    })
            });
            prop_assert!(
                bytes_after(&snapshot, &policy, &remaining) as f64 <= target || !candidate_left
            );
        } else {
            prop_assert!(plan.capped.is_empty());
        }
    }
}

mod deletes {
    use std::time::Duration;

    use crate::database::tests::{edges, hello, libiconv, register_nar, stored_paths};
    use crate::{Database, DatabaseError};

    use super::*;

    /// libiconv (self-referencing) committed at time 1, its NAR registered.
    fn with_libiconv() -> (tempfile::TempDir, Database) {
        let root = tempfile::tempdir().unwrap();
        let database = Database::open(root.path()).unwrap().0;
        register_nar(&database, &libiconv());
        database
            .commit_narinfo(&libiconv(), &[], "upload", 1)
            .unwrap();
        (root, database)
    }

    fn marked(database: &Database, store_path: &str) -> MarkedPath {
        let snapshot = database.gc_snapshot().unwrap();
        snapshot
            .paths
            .into_iter()
            .find(|path| path.store_path == store_path)
            .unwrap()
    }

    #[test]
    fn an_expired_self_referencing_path_goes_with_its_self_edge_unless_referenced() {
        let (_root, database) = with_libiconv();
        register_nar(&database, &hello());
        database.commit_narinfo(&hello(), &[], "upload", 1).unwrap();
        let libiconv = marked(&database, &libiconv().store_path);
        let hello = marked(&database, &hello().store_path);

        assert!(!database.delete_marked(&libiconv).unwrap());
        assert_eq!(stored_paths(&database).len(), 2);
        assert!(database.delete_marked(&hello).unwrap());
        assert!(database.delete_marked(&libiconv).unwrap());
        assert!(stored_paths(&database).is_empty());
        assert!(edges(&database).is_empty());
        // Already gone: skipped, not an error.
        assert!(!database.delete_marked(&libiconv).unwrap());
    }

    #[test]
    fn a_rooted_or_accessed_path_is_skipped() {
        let (root, database) = with_libiconv();
        let path = marked(&database, &libiconv().store_path);
        database
            .writer()
            .unwrap()
            .execute("INSERT INTO root VALUES ('keep', ?1)", [&path.store_path])
            .unwrap();
        assert!(!database.delete_marked(&path).unwrap());
        database
            .writer()
            .unwrap()
            .execute("DELETE FROM root", [])
            .unwrap();

        // A read from the server between mark and delete saves the path.
        let server = Database::open(root.path()).unwrap().0;
        let store_hash = &path.store_path["/nix/store/".len()..][..32];
        assert_eq!(server.record_narinfo_access(store_hash, 10_000).unwrap(), 1);
        assert!(!database.delete_marked(&path).unwrap());
        assert_eq!(stored_paths(&database), [path.store_path]);
    }

    #[test]
    fn a_commit_before_the_delete_of_its_reference_keeps_both() {
        let (root, gc) = with_libiconv();
        let server = Database::open(root.path()).unwrap().0;
        let path = marked(&gc, &libiconv().store_path);
        register_nar(&server, &hello());
        server.commit_narinfo(&hello(), &[], "upload", 1).unwrap();
        assert!(!gc.delete_marked(&path).unwrap());
        assert_eq!(stored_paths(&gc).len(), 2);
        assert!(
            edges(&gc)
                .iter()
                .any(|(from, to)| *from == hello().store_path && *to == path.store_path)
        );
    }

    #[test]
    fn a_commit_waiting_on_the_delete_of_its_reference_fails_without_a_dangling_edge() {
        let (root, gc) = with_libiconv();
        let server = Database::open(root.path()).unwrap().0;
        let path = marked(&gc, &libiconv().store_path);
        register_nar(&server, &hello());
        let mut writer = gc.writer().unwrap();
        let transaction = writer.transaction().unwrap();
        std::thread::scope(|scope| {
            let upload = scope.spawn(|| server.commit_narinfo(&hello(), &[], "upload", 1));
            // The upload now waits on the delete's write lock (busy timeout).
            std::thread::sleep(Duration::from_millis(200));
            assert!(super::super::delete_marked(&transaction, &path).unwrap());
            transaction.commit().unwrap();
            assert!(matches!(
                upload.join().unwrap(),
                Err(DatabaseError::MissingReference)
            ));
        });
        drop(writer);
        assert!(stored_paths(&gc).is_empty());
        assert!(edges(&gc).is_empty());
    }
}

mod sweep {
    use std::fs;
    use std::io::Read;
    use std::time::Duration;

    use crate::database::tests::{libiconv, register_nar};
    use crate::{Blob, BlobStore, Database, DatabaseError};

    const GRACE: i64 = 3600;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A server (store and database) and a second process's `gc` handles on one directory.
    struct State {
        dir: tempfile::TempDir,
        server: (BlobStore, Database),
        gc: (BlobStore, Database),
    }

    fn state() -> State {
        let dir = tempfile::tempdir().unwrap();
        let server = (
            BlobStore::open(dir.path()).unwrap(),
            Database::open(dir.path()).unwrap().0,
        );
        let gc = (
            BlobStore::open_existing(dir.path()).unwrap(),
            Database::open_existing(dir.path()).unwrap(),
        );
        State { dir, server, gc }
    }

    impl State {
        fn sweep(&self, now: i64) -> super::Swept {
            self.gc.1.sweep(&self.gc.0, now, GRACE).unwrap()
        }

        fn blob_rows(&self) -> i64 {
            self.server
                .1
                .reader()
                .unwrap()
                .query_row("SELECT count(*) FROM blob", [], |row| row.get(0))
                .unwrap()
        }
    }

    #[test]
    fn a_blob_written_but_not_committed_survives_inside_the_grace_and_goes_after_it() {
        let state = state();
        let blob = state.server.0.write(&b"orphan"[..], 6, None).unwrap();
        let now = now();
        assert_eq!(state.sweep(now), super::Swept::default());
        assert!(state.server.0.open_blob(&blob.hash).is_ok());
        let swept = state.sweep(now + GRACE + 1);
        assert_eq!((swept.orphan_files, swept.freed_bytes), (1, 6));
        assert!(state.server.0.open_blob(&blob.hash).is_err());
    }

    #[test]
    fn an_in_flight_temp_file_survives_inside_the_grace() {
        let state = state();
        let temp = state.dir.path().join("tmp/upload");
        fs::write(&temp, b"partial").unwrap();
        // Opening the gc's store never sweeps tmp/, unlike the server's startup.
        drop(BlobStore::open_existing(state.dir.path()).unwrap());
        assert_eq!(state.sweep(now()).temp_files, 0);
        assert!(temp.exists());
        assert_eq!(state.sweep(now() + GRACE + 1).temp_files, 1);
        assert!(!temp.exists());
    }

    #[test]
    fn unused_nar_rows_go_after_their_registration_grace_with_their_blobs() {
        let state = state();
        // A path still in use keeps its NAR and blob rows (its file is not even present).
        register_nar(&state.server.1, &libiconv());
        state
            .server
            .1
            .commit_narinfo(&libiconv(), &[], "upload", 1)
            .unwrap();
        // An old blob uploaded again recently: the grace counts from the new upload.
        let blob = state.server.0.write(&b"nar"[..], 3, None).unwrap();
        state.server.1.register_nar("nar/a.nar", &blob, 0).unwrap();
        let now = now();
        state
            .server
            .1
            .register_nar("nar/a.nar", &blob, now)
            .unwrap();
        let swept = state.sweep(now + GRACE - 1);
        assert_eq!((swept.nars, swept.blobs), (0, 0));
        assert!(state.server.1.nar_blob("nar/a.nar").unwrap().is_some());

        let swept = state.sweep(now + GRACE);
        assert_eq!(
            (
                swept.nars,
                swept.blobs,
                swept.orphan_files,
                swept.freed_bytes
            ),
            (1, 1, 0, 3)
        );
        assert!(state.server.1.nar_blob("nar/a.nar").unwrap().is_none());
        assert!(state.server.0.open_blob(&blob.hash).is_err());
        assert_eq!(state.blob_rows(), 1);
        assert!(state.server.1.nar_blob(&libiconv().url).unwrap().is_some());
    }

    #[test]
    fn an_unused_blob_row_whose_file_is_already_gone_is_swept_without_error() {
        let state = state();
        let blob = state.server.0.write(&b"gone"[..], 4, None).unwrap();
        state.server.1.register_nar("nar/a.nar", &blob, 0).unwrap();
        fs::remove_file(crate::blob_path(state.dir.path(), &blob.hash)).unwrap();
        let swept = state.sweep(now() + GRACE);
        assert_eq!((swept.nars, swept.blobs), (1, 1));
        assert_eq!(state.blob_rows(), 0);
    }

    /// A blob row no NAR row uses, with its file: what a deduplicated upload reuses.
    fn unused_blob(state: &State) -> Blob {
        let blob = state.server.0.write(&b"shared"[..], 6, None).unwrap();
        state
            .server
            .1
            .register_nar("nar/old.nar", &blob, 0)
            .unwrap();
        state
            .server
            .1
            .writer()
            .unwrap()
            .execute("DELETE FROM nar", [])
            .unwrap();
        blob
    }

    #[test]
    fn a_deduplicated_registration_before_the_sweep_keeps_the_blob() {
        let state = state();
        let blob = unused_blob(&state);
        // The upload found the file in place (no rename) and registers before the sweep.
        assert_eq!(state.server.0.write(&b"shared"[..], 6, None).unwrap(), blob);
        state
            .server
            .1
            .register_nar("nar/new.nar", &blob, now())
            .unwrap();
        assert_eq!(state.sweep(now()).blobs, 0);
        assert!(state.server.0.open_blob(&blob.hash).is_ok());
    }

    #[test]
    fn a_deduplicated_registration_waiting_on_the_sweep_fails_and_its_retry_succeeds() {
        let state = state();
        let blob = unused_blob(&state);
        assert_eq!(state.server.0.write(&b"shared"[..], 6, None).unwrap(), blob);
        let mut writer = state.gc.1.writer().unwrap();
        let transaction = writer.transaction().unwrap();
        std::thread::scope(|scope| {
            let upload = scope.spawn(|| state.server.1.register_nar("nar/new.nar", &blob, now()));
            // The registration now waits on the sweep's write lock (busy timeout).
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(
                super::super::sweep_blob(&transaction, &state.gc.0, &blob.hash).unwrap(),
                Some(6)
            );
            transaction.commit().unwrap();
            assert!(matches!(
                upload.join().unwrap(),
                Err(DatabaseError::MissingBlob)
            ));
        });
        drop(writer);
        assert_eq!(state.blob_rows(), 0);
        assert!(state.server.0.open_blob(&blob.hash).is_err());
        // The client retries: the upload publishes the file again and registers it.
        assert_eq!(state.server.0.write(&b"shared"[..], 6, None).unwrap(), blob);
        state
            .server
            .1
            .register_nar("nar/new.nar", &blob, now())
            .unwrap();
        assert!(state.server.0.open_blob(&blob.hash).is_ok());
    }

    #[test]
    fn a_download_opened_before_the_unlink_streams_to_the_end() {
        let state = state();
        let content = vec![7; 200 * 1024];
        let blob = state
            .server
            .0
            .write(content.as_slice(), content.len() as u64, None)
            .unwrap();
        let mut download = state.server.0.open_blob(&blob.hash).unwrap();
        let mut start = [0; 1024];
        download.read_exact(&mut start).unwrap();
        assert_eq!(state.sweep(now() + GRACE + 1).orphan_files, 1);
        assert!(state.server.0.open_blob(&blob.hash).is_err());
        let mut rest = Vec::new();
        download.read_to_end(&mut rest).unwrap();
        assert_eq!(start.len() + rest.len(), content.len());
    }
}

mod roots {
    use crate::database::tests::{hello, libiconv, register_nar, stored_paths};
    use crate::{BlobStore, Database, DatabaseError};

    use super::*;

    #[test]
    fn a_rooted_closure_survives_expiry_and_the_cap_pass_while_the_rest_goes() {
        let root = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(root.path()).unwrap();
        let database = Database::open(root.path()).unwrap().0;
        let mut other = libiconv();
        other.store_path = "/nix/store/00000000000000000000000000000000-other".into();
        other.url = "nar/other.nar".into();
        other.file_hash = Some([9; 32]);
        other.references.clear();
        for info in [libiconv(), hello(), other.clone()] {
            register_nar(&database, &info);
            database.commit_narinfo(&info, &[], "upload", 1).unwrap();
        }
        let absent = ["/nix/store/11111111111111111111111111111111-absent".to_owned()];
        assert!(matches!(
            database.set_root("release", &absent.into()),
            Err(DatabaseError::MissingReference)
        ));
        database
            .set_root("release", &[hello().store_path].into())
            .unwrap();
        // Everything is expired and a zero cap wants every byte back.
        let mut policy = policy(Some(0));
        policy.now = 1 + policy.retention;
        let mut report = Report::default();
        database.collect(&blobs, &policy, &mut report).unwrap();
        assert_eq!(report.deleted_entries, 1);
        let mut kept = vec![hello().store_path, libiconv().store_path];
        kept.sort();
        assert_eq!(stored_paths(&database), kept);

        assert!(database.delete_root("release").unwrap());
        assert!(!database.delete_root("release").unwrap());
        let mut report = Report::default();
        database.collect(&blobs, &policy, &mut report).unwrap();
        assert_eq!(report.deleted_entries, 2);
        assert!(stored_paths(&database).is_empty());
    }
}
