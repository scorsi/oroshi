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
