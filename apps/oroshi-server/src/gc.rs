//! `oroshi-server gc` (ADR 0005, 0011, 0025) and `oroshi-server nix drop` (ADR 0026), both against
//! the live database under `gc.lock`.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use oroshi_store::gc::{Policy, Report};
use oroshi_store::{BlobStore, Database};

use crate::config::Config;

/// The lock shared by `oroshi-server gc` and `oroshi-server nix drop`; the server holds `oroshi.lock`.
pub(crate) fn lock(root: &Path) -> Result<File, String> {
    crate::server::check_state_dir(root).map_err(|error| error.to_string())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("gc.lock"))
        .map_err(|error| format!("gc lock: {error}"))?;
    file.try_lock()
        .map_err(|error| format!("gc lock: {error}"))?;
    Ok(file)
}

/// Open the database and blob store of a running server without migrating or sweeping.
pub(crate) fn open(root: &Path) -> Result<(Database, BlobStore), String> {
    let database = Database::open_existing(root).map_err(|error| error.to_string())?;
    let blobs = BlobStore::open_existing(root).map_err(|error| format!("blob store: {error}"))?;
    Ok((database, blobs))
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs() as i64)
}

/// Run one GC and write its `gc-run` line; returns whether it succeeded.
pub fn run(config: &Config) -> bool {
    let start = Instant::now();
    let policy = Policy {
        now: now(),
        retention: config.retention.days.saturating_mul(86_400) as i64,
        orphan_grace: config.gc.orphan_grace as i64,
        max_bytes: config.storage.max_bytes,
        gc_target_ratio: config.storage.gc_target_ratio,
    };
    let mut report = Report::default();
    let result = collect(&config.storage.dir, &policy, &mut report);
    let deleted_blobs = report.swept.blobs + report.swept.orphan_files;
    let dur_us = start.elapsed().as_micros() as u64;
    match &result {
        Ok(()) => tracing::info!(
            event = "gc-run",
            deleted_entries = report.deleted_entries,
            deleted_blobs,
            freed_bytes = report.swept.freed_bytes,
            dur_us
        ),
        Err(error) => tracing::error!(
            event = "gc-run",
            err = %error,
            deleted_entries = report.deleted_entries,
            deleted_blobs,
            freed_bytes = report.swept.freed_bytes,
            dur_us
        ),
    }
    result.is_ok()
}

/// A failure before the database is open cannot be counted in `gc_state`.
fn collect(root: &Path, policy: &Policy, report: &mut Report) -> Result<(), String> {
    let _lock = lock(root)?;
    let (database, blobs) = open(root)?;
    let result = database
        .collect(&blobs, policy, report)
        .map_err(|error| error.to_string());
    let recorded = database
        .record_gc_run(result.is_ok(), report.swept.freed_bytes, policy.now)
        .map_err(|error| error.to_string());
    result.and(recorded)
}

/// `oroshi-server nix drop`: a refusal is returned as the command's error (`fatal`, ADR 0025).
pub fn drop_narinfo(config: &Config, store_path: &str, with_referrers: bool) -> Result<(), String> {
    let name = oroshi_nix::parse_store_path(store_path)
        .map(|_| &store_path["/nix/store/".len()..])
        .map_err(|error| format!("invalid store path: {error}"))?;
    let _lock = lock(&config.storage.dir)?;
    let database =
        Database::open_existing(&config.storage.dir).map_err(|error| error.to_string())?;
    let referrers =
        database
            .drop_narinfo(store_path, with_referrers)
            .map_err(|error| match error {
                oroshi_store::DatabaseError::Referenced(..) if !with_referrers => {
                    format!("{error} (drop them too with --with-referrers)")
                }
                _ => error.to_string(),
            })?;
    tracing::info!(event = "narinfo-dropped", name, referrers);
    Ok(())
}
