//! Garbage collection of Nix paths and blobs (ADR 0005, 0024).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

use rusqlite::{Connection, OptionalExtension};

use crate::{Database, DatabaseError};

/// One narinfo as seen by the mark phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedPath {
    pub store_path: String,
    pub last_accessed_at: i64,
    pub nar_url: String,
}

/// One registered NAR URL and its blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedNar {
    pub url: String,
    pub blob: [u8; 32],
    pub registered_at: i64,
}

/// Everything the mark phase reads, taken in one read transaction.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub paths: Vec<MarkedPath>,
    pub nars: Vec<MarkedNar>,
    /// Size of every registered blob, used or not.
    pub blobs: Vec<([u8; 32], u64)>,
    /// `(referrer, reference)` edges, self-edges included.
    pub edges: Vec<(String, String)>,
    /// Store paths named by a root (duplicates allowed).
    pub rooted: Vec<String>,
}

/// Retention settings and the clock of one run; times and durations are in seconds.
#[derive(Clone, Debug)]
pub struct Policy {
    pub now: i64,
    pub retention: i64,
    pub orphan_grace: i64,
    pub max_bytes: Option<u64>,
    pub gc_target_ratio: f64,
}

/// Paths to delete, each list in referrer-before-reference order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Unrooted paths not accessed within the retention and not reachable from a live path.
    pub expired: Vec<MarkedPath>,
    /// Least recently accessed paths outside every root's closure, removed for the size cap.
    pub capped: Vec<MarkedPath>,
}

impl Database {
    /// Read the mark phase's inputs in one consistent snapshot.
    pub fn gc_snapshot(&self) -> Result<Snapshot, DatabaseError> {
        let mut reader = self.reader()?;
        let transaction = reader.transaction()?;
        let mut snapshot = Snapshot::default();
        {
            let select = |sql: &str| transaction.prepare(sql);
            snapshot.paths = select("SELECT store_path, last_accessed_at, nar_url FROM narinfo")?
                .query_map([], |row| {
                    Ok(MarkedPath {
                        store_path: row.get(0)?,
                        last_accessed_at: row.get(1)?,
                        nar_url: row.get(2)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            snapshot.nars = select("SELECT url, blob_sha256, registered_at FROM nar")?
                .query_map([], |row| {
                    Ok(MarkedNar {
                        url: row.get(0)?,
                        blob: row.get(1)?,
                        registered_at: row.get(2)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            snapshot.blobs = select("SELECT sha256, size FROM blob")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            snapshot.edges = select("SELECT referrer, reference FROM reference")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            snapshot.rooted = select("SELECT store_path FROM root")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
        }
        transaction.commit()?;
        Ok(snapshot)
    }
}

impl Database {
    /// Delete one marked path in its own write transaction, after re-checking that it is
    /// still unrooted, not accessed since the mark and referenced by no other narinfo
    /// (ADR 0005). Returns `false`, deleting nothing, when the re-check fails.
    pub fn delete_marked(&self, path: &MarkedPath) -> Result<bool, DatabaseError> {
        let mut writer = self.writer()?;
        let transaction = writer.transaction()?;
        let deleted = delete_marked(&transaction, path)?;
        transaction.commit()?;
        Ok(deleted)
    }
}

fn delete_marked(transaction: &Connection, path: &MarkedPath) -> Result<bool, DatabaseError> {
    let eligible = transaction
        .query_row(
            "SELECT last_accessed_at = ?2
                 AND NOT EXISTS (SELECT 1 FROM root WHERE store_path = ?1)
                 AND NOT EXISTS (
                     SELECT 1 FROM reference WHERE reference = ?1 AND referrer != ?1
                 )
             FROM narinfo WHERE store_path = ?1",
            rusqlite::params![path.store_path, path.last_accessed_at],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if eligible {
        // Outgoing edges first: the self-edge would otherwise block the narinfo's delete.
        transaction.execute(
            "DELETE FROM reference WHERE referrer = ?1",
            [&path.store_path],
        )?;
        transaction.execute(
            "DELETE FROM narinfo WHERE store_path = ?1",
            [&path.store_path],
        )?;
    }
    Ok(eligible)
}

/// The mark phase: a pure function of the snapshot and the policy (ADR 0005).
pub fn mark(snapshot: &Snapshot, policy: &Policy) -> Plan {
    let index: HashMap<&str, usize> = snapshot
        .paths
        .iter()
        .enumerate()
        .map(|(i, path)| (path.store_path.as_str(), i))
        .collect();
    let count = snapshot.paths.len();
    // Edges between known paths, self-edges excluded: they never block a deletion.
    let mut references = vec![Vec::new(); count];
    let mut referrers = vec![0usize; count];
    for (referrer, reference) in &snapshot.edges {
        if let (Some(&from), Some(&to)) =
            (index.get(referrer.as_str()), index.get(reference.as_str()))
            && from != to
        {
            references[from].push(to);
            referrers[to] += 1;
        }
    }
    let closure = |seeds: &mut dyn Iterator<Item = usize>| {
        let mut reached = vec![false; count];
        let mut stack: Vec<usize> = seeds.collect();
        while let Some(i) = stack.pop() {
            if !std::mem::replace(&mut reached[i], true) {
                stack.extend(&references[i]);
            }
        }
        reached
    };
    let rooted = closure(
        &mut snapshot
            .rooted
            .iter()
            .filter_map(|path| index.get(path.as_str()).copied()),
    );
    let live = closure(&mut (0..count).filter(|&i| {
        rooted[i] || policy.now - snapshot.paths[i].last_accessed_at < policy.retention
    }));

    let mut space = Space::new(snapshot, policy);
    // Removes path `i` and returns its references left without a referrer.
    let take = |i: usize, referrers: &mut [usize], space: &mut Space| {
        space.remove(&snapshot.paths[i].nar_url);
        for &to in &references[i] {
            referrers[to] -= 1;
        }
        references[i]
            .iter()
            .copied()
            .filter(|&to| referrers[to] == 0)
            .collect::<Vec<_>>()
    };

    // Expired paths have no live referrer (liveness is closed under references), so a
    // topological order over them alone empties the whole set; a cycle is never taken.
    let mut expired = Vec::new();
    let mut ready: BTreeSet<usize> = (0..count)
        .filter(|&i| !live[i] && referrers[i] == 0)
        .collect();
    while let Some(i) = ready.pop_first() {
        expired.push(snapshot.paths[i].clone());
        ready.extend(
            take(i, &mut referrers, &mut space)
                .into_iter()
                .filter(|&to| !live[to]),
        );
    }

    let mut capped = Vec::new();
    if let Some(max_bytes) = policy.max_bytes {
        let target = policy.gc_target_ratio * max_bytes as f64;
        let key = |i: usize| Reverse((snapshot.paths[i].last_accessed_at, i));
        // Expiry left exactly the live paths; each becomes unreferenced at most once.
        let candidate = |i: usize| live[i] && !rooted[i];
        let mut heap: BinaryHeap<_> = (0..count)
            .filter(|&i| candidate(i) && referrers[i] == 0)
            .map(key)
            .collect();
        while space.bytes as f64 > target
            && let Some(Reverse((_, i))) = heap.pop()
        {
            capped.push(snapshot.paths[i].clone());
            heap.extend(
                take(i, &mut referrers, &mut space)
                    .into_iter()
                    .filter(|&to| candidate(to))
                    .map(key),
            );
        }
    }
    Plan { expired, capped }
}

/// Registered bytes left once the sweep has run after the planned deletes.
struct Space {
    bytes: u64,
    /// Remaining narinfos per NAR URL.
    users: HashMap<String, usize>,
    /// The blob of each NAR URL that the sweep may delete once unused.
    sweepable: HashMap<String, [u8; 32]>,
    /// Kept NAR rows per blob, with its size.
    blobs: BTreeMap<[u8; 32], (usize, u64)>,
}

impl Space {
    fn new(snapshot: &Snapshot, policy: &Policy) -> Self {
        let mut users = HashMap::new();
        for path in &snapshot.paths {
            *users.entry(path.nar_url.clone()).or_insert(0) += 1;
        }
        let mut blobs: BTreeMap<[u8; 32], (usize, u64)> = snapshot
            .blobs
            .iter()
            .map(|&(hash, size)| (hash, (0, size)))
            .collect();
        let mut sweepable = HashMap::new();
        for nar in &snapshot.nars {
            // A NAR row inside the grace survives the sweep even when no narinfo uses it.
            let protected = policy.now - nar.registered_at < policy.orphan_grace;
            if protected || users.contains_key(&nar.url) {
                if let Some(blob) = blobs.get_mut(&nar.blob) {
                    blob.0 += 1;
                }
            }
            if !protected {
                sweepable.insert(nar.url.clone(), nar.blob);
            }
        }
        let bytes = blobs
            .values()
            .filter(|(kept, _)| *kept > 0)
            .map(|(_, size)| size)
            .sum();
        Self {
            bytes,
            users,
            sweepable,
            blobs,
        }
    }

    fn remove(&mut self, nar_url: &str) {
        let Some(users) = self.users.get_mut(nar_url) else {
            return;
        };
        *users -= 1;
        if *users > 0 {
            return;
        }
        if let Some(blob) = self
            .sweepable
            .get(nar_url)
            .and_then(|hash| self.blobs.get_mut(hash))
        {
            blob.0 -= 1;
            if blob.0 == 0 {
                self.bytes -= blob.1;
            }
        }
    }
}

#[cfg(test)]
mod tests;
