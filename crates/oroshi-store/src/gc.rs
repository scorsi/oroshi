//! Garbage collection of Nix paths and blobs (ADR 0005, 0024).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OptionalExtension};

use crate::{BlobStore, Database, DatabaseError};

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
        self.write(|transaction| delete_marked(transaction, path))
    }

    /// Sweep after the row deletes (ADR 0005, 0024): `nar` rows no narinfo uses, registered
    /// at least `grace` seconds before `now`; `blob` rows no `nar` row uses, with their files;
    /// files under `blobs/` without a row and files in `tmp/`, modified at least `grace`
    /// seconds before `now`. `blobs` must be opened with `BlobStore::open_existing` by a
    /// second process. A file already gone is not an error.
    pub fn sweep(&self, blobs: &BlobStore, now: i64, grace: i64) -> Result<Swept, DatabaseError> {
        let mut swept = Swept::default();
        self.sweep_into(blobs, now, grace, &mut swept)?;
        Ok(swept)
    }

    /// `sweep`, keeping in `swept` what was removed before a failure.
    fn sweep_into(
        &self,
        blobs: &BlobStore,
        now: i64,
        grace: i64,
        swept: &mut Swept,
    ) -> Result<(), DatabaseError> {
        let cutoff = now - grace;
        swept.nars = self.writer()?.execute(
            "DELETE FROM nar WHERE registered_at <= ?1
             AND NOT EXISTS (SELECT 1 FROM narinfo WHERE nar_url = nar.url)",
            [cutoff],
        )?;
        let unused: Vec<[u8; 32]> = self
            .reader()?
            .prepare(
                "SELECT sha256 FROM blob
                 WHERE NOT EXISTS (SELECT 1 FROM nar WHERE blob_sha256 = blob.sha256)",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for hash in unused {
            if let Some(size) = self.write(|transaction| sweep_blob(transaction, blobs, &hash))? {
                swept.blobs += 1;
                swept.freed_bytes += size;
            }
        }
        for directory in entries(&blobs.root.join("blobs/sha256"))? {
            if !directory.is_dir() {
                continue;
            }
            for file in entries(&directory)? {
                let hash = file.file_name().and_then(|name| parse_hex(name.to_str()?));
                if let Some(size) =
                    self.write(|transaction| sweep_orphan(transaction, &file, hash, cutoff))?
                {
                    swept.orphan_files += 1;
                    swept.freed_bytes += size;
                }
            }
        }
        for file in entries(&blobs.root.join("tmp"))? {
            if older_file(&file, cutoff)?.is_some() && remove(&file)? {
                swept.temp_files += 1;
            }
        }
        Ok(())
    }

    /// One `oroshi gc` run (ADR 0005): mark, delete the expired then the capped paths,
    /// sweep. `report` keeps the counts reached when an error stops the run.
    pub fn collect(
        &self,
        blobs: &BlobStore,
        policy: &Policy,
        report: &mut Report,
    ) -> Result<(), DatabaseError> {
        let plan = mark(&self.gc_snapshot()?, policy);
        for path in plan.expired.iter().chain(&plan.capped) {
            if self.delete_marked(path)? {
                report.deleted_entries += 1;
            }
        }
        self.sweep_into(blobs, policy.now, policy.orphan_grace, &mut report.swept)
    }

    /// Count one finished run in `gc_state`; freed bytes count whatever the result.
    pub fn record_gc_run(&self, ok: bool, freed_bytes: u64, now: i64) -> Result<(), DatabaseError> {
        self.writer()?.execute(
            "UPDATE gc_state SET runs_success = runs_success + ?1, runs_error = runs_error + ?2,
             freed_bytes = freed_bytes + ?3,
             last_success_at = CASE WHEN ?1 THEN ?4 ELSE last_success_at END",
            rusqlite::params![ok, !ok, freed_bytes, now],
        )?;
        Ok(())
    }

    /// The persisted GC counters, read by the server's metrics (ADR 0011).
    pub fn gc_state(&self) -> Result<GcState, DatabaseError> {
        Ok(self.reader()?.query_row(
            "SELECT runs_success, runs_error, freed_bytes, last_success_at FROM gc_state",
            [],
            |row| {
                Ok(GcState {
                    runs_success: row.get(0)?,
                    runs_error: row.get(1)?,
                    freed_bytes: row.get(2)?,
                    last_success_at: row.get(3)?,
                })
            },
        )?)
    }
}

/// Counts of one `oroshi gc` run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub deleted_entries: usize,
    pub swept: Swept,
}

/// The single `gc_state` row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcState {
    pub runs_success: u64,
    pub runs_error: u64,
    pub freed_bytes: u64,
    pub last_success_at: Option<i64>,
}

/// What one sweep removed; `freed_bytes` counts blob rows and orphan files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub nars: usize,
    pub blobs: usize,
    pub orphan_files: usize,
    pub temp_files: usize,
    pub freed_bytes: u64,
}

/// Delete an unused blob row and unlink its file before the commit (ADR 0024).
fn sweep_blob(
    transaction: &Connection,
    blobs: &BlobStore,
    hash: &[u8; 32],
) -> Result<Option<u64>, DatabaseError> {
    let size: Option<u64> = transaction
        .query_row(
            "SELECT size FROM blob WHERE sha256 = ?1
             AND NOT EXISTS (SELECT 1 FROM nar WHERE blob_sha256 = ?1)",
            [hash.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    if size.is_some() {
        transaction.execute("DELETE FROM blob WHERE sha256 = ?1", [hash.as_slice()])?;
        remove(&blobs.blob_path(hash))?;
    }
    Ok(size)
}

/// Unlink a file under `blobs/` that has no row and is older than the cutoff. Checked under
/// the write lock, so a registration of the same content commits first or fails cleanly.
fn sweep_orphan(
    transaction: &Connection,
    file: &Path,
    hash: Option<[u8; 32]>,
    cutoff: i64,
) -> Result<Option<u64>, DatabaseError> {
    if let Some(hash) = hash
        && transaction
            .query_row(
                "SELECT 1 FROM blob WHERE sha256 = ?1",
                [hash.as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
    {
        return Ok(None);
    }
    Ok(match older_file(file, cutoff)? {
        Some(size) if remove(file)? => Some(size),
        _ => None,
    })
}

/// Paths in `directory`, which may be missing.
fn entries(directory: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries.map(|entry| Ok(entry?.path())).collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// The size of a regular file modified at or before `cutoff` (Unix seconds).
fn older_file(path: &Path, cutoff: i64) -> io::Result<Option<u64>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |age| age.as_secs() as i64);
    Ok((metadata.is_file() && modified <= cutoff).then_some(metadata.len()))
}

/// Unlink a file; `false` when it was already gone.
fn remove(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The hash named by a blob file, written by `hex`.
fn parse_hex(name: &str) -> Option<[u8; 32]> {
    let mut hash = [0; 32];
    if name.len() != 64 || !name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&name[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(hash)
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
            if (protected || users.contains_key(&nar.url))
                && let Some(blob) = blobs.get_mut(&nar.blob)
            {
                blob.0 += 1;
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
