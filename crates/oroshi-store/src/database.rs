use std::fmt;
#[cfg(test)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use oroshi_nix::{NarInfo, ParseError, parse_store_path};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::Blob;

const MIGRATIONS: &[&str] = &[
    include_str!("schema.sql"),
    // Anonymous NAR reads update by nar_url under the writer lock; GC deletes check references.
    "CREATE INDEX narinfo_nar_url ON narinfo (nar_url);
     CREATE INDEX reference_reference ON reference (reference);",
];
/// Idle read-only connections kept for reuse; extra concurrent readers are closed after use.
const POOLED_READERS: usize = 16;

#[derive(Debug)]
pub enum DatabaseError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    InvalidStorePath(ParseError),
    UnsupportedSchema { found: i64, supported: usize },
    WriterPoisoned,
    NarinfoConflict,
    MissingReference,
    MissingNar,
    FileHashMismatch,
    FileSizeMismatch,
    InvalidNarinfo(ParseError),
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "database filesystem error: {error}"),
            Self::Sqlite(error) => write!(f, "database SQLite error: {error}"),
            Self::InvalidStorePath(error) => write!(f, "invalid narinfo store path: {error}"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "unsupported database schema version {found}; binary supports versions 0..={supported}"
            ),
            Self::NarinfoConflict => f.write_str("conflicting narinfo metadata"),
            Self::MissingReference => f.write_str("missing narinfo reference"),
            Self::MissingNar => f.write_str("narinfo URL names no registered NAR"),
            Self::FileHashMismatch => {
                f.write_str("narinfo FileHash differs from the registered NAR")
            }
            Self::FileSizeMismatch => {
                f.write_str("narinfo FileSize differs from the registered NAR")
            }
            Self::InvalidNarinfo(error) => write!(f, "invalid stored narinfo: {error}"),
            Self::WriterPoisoned => f.write_str("database writer lock poisoned"),
        }
    }
}

impl std::error::Error for DatabaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidStorePath(error) | Self::InvalidNarinfo(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for DatabaseError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for DatabaseError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Open once at startup and share across serving threads. Calls are synchronous;
/// async callers must use `spawn_blocking` (ADR 0002).
pub struct Database {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

/// A pooled read-only connection, returned to the pool on drop.
pub struct Reader<'a> {
    pool: &'a Mutex<Vec<Connection>>,
    connection: Option<Connection>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Reader<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection.as_mut().unwrap()
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let connection = self.connection.take().unwrap();
        // A connection left inside a transaction would pin an old snapshot for its next user.
        if connection.is_autocommit()
            && let Ok(mut pool) = self.pool.lock()
            && pool.len() < POOLED_READERS
        {
            pool.push(connection);
        }
    }
}

impl Database {
    /// Open the database and return the versions committed during startup.
    pub fn open(root: impl AsRef<Path>) -> Result<(Self, Vec<usize>), DatabaseError> {
        let directory = root.as_ref().join("db");
        crate::create_dir_all_durable(&directory, &mut crate::sync_directory)?;
        let path = directory.join("oroshi.sqlite");
        let mut writer = Connection::open(&path)?;
        // Refuse incompatible databases before changing their persistent journal mode.
        check_version(&writer, MIGRATIONS.len())?;
        let mode: String = writer.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if mode != "wal" {
            return Err(io::Error::other("SQLite did not enable WAL mode").into());
        }
        writer.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
        let applied = migrate(&mut writer, MIGRATIONS)?;
        Ok((
            Self {
                path,
                writer: Mutex::new(writer),
                readers: Mutex::default(),
            },
            applied,
        ))
    }

    /// Total bytes of registered blobs, counting each content hash once.
    pub fn stored_bytes(&self) -> Result<u64, DatabaseError> {
        Ok(self
            .reader()?
            .query_row("SELECT coalesce(sum(size), 0) FROM blob", [], |row| {
                row.get(0)
            })?)
    }

    /// Size of a registered content hash, independent of its protocol URL.
    pub fn blob_size(&self, hash: &[u8; 32]) -> Result<Option<u64>, DatabaseError> {
        Ok(self
            .reader()?
            .query_row(
                "SELECT size FROM blob WHERE sha256 = ?1",
                [hash.as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Register a published blob and its NAR URL atomically; duplicates are no-ops.
    /// Call only after the blob's hash has been verified against its URL.
    pub fn register_nar(
        &self,
        url: &str,
        blob: &Blob,
        created_at: i64,
    ) -> Result<(), DatabaseError> {
        let mut writer = self.writer()?;
        let transaction = writer.transaction()?;
        transaction.execute(
            "INSERT INTO blob (sha256, size, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (sha256) DO NOTHING",
            rusqlite::params![blob.hash.as_slice(), blob.size, created_at],
        )?;
        transaction.execute(
            "INSERT INTO nar (url, blob_sha256) VALUES (?1, ?2)
             ON CONFLICT (url) DO NOTHING",
            rusqlite::params![url, blob.hash.as_slice()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Commit a parsed narinfo, its existing NAR link and all reference edges atomically.
    /// The NAR must already be registered by its upload, with the narinfo's FileHash and
    /// FileSize; both are checked inside the transaction. Foreign keys refuse absent
    /// dependencies; inserting the narinfo first permits its self-edge (ADR 0005).
    /// Existing paths merge signatures only; all other metadata is immutable.
    /// `accessed_at` is Unix time in seconds.
    pub fn commit_narinfo(
        &self,
        info: &NarInfo,
        source: &str,
        accessed_at: i64,
    ) -> Result<(), DatabaseError> {
        let path = parse_store_path(&info.store_path).map_err(DatabaseError::InvalidStorePath)?;
        let mut writer = self.writer()?;
        let transaction = writer.transaction()?;
        let blob: Option<(Vec<u8>, u64)> = transaction
            .query_row(
                "SELECT blob.sha256, blob.size FROM nar JOIN blob ON nar.blob_sha256 = blob.sha256
                 WHERE nar.url = ?1",
                [&info.url],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (hash, size) = blob.ok_or(DatabaseError::MissingNar)?;
        if info.file_hash.as_ref().map(|hash| hash.as_slice()) != Some(hash.as_slice()) {
            return Err(DatabaseError::FileHashMismatch);
        }
        if info.file_size != Some(size) {
            return Err(DatabaseError::FileSizeMismatch);
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT content FROM narinfo WHERE store_hash = ?1",
                [path.hash],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(content) = existing {
            let mut stored: NarInfo = content.parse().map_err(DatabaseError::InvalidNarinfo)?;
            stored
                .merge_signatures(info)
                .map_err(|_| DatabaseError::NarinfoConflict)?;
            transaction.execute(
                "UPDATE narinfo SET content = ?2 WHERE store_hash = ?1",
                rusqlite::params![path.hash, stored.to_string()],
            )?;
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "INSERT INTO narinfo (store_path, store_hash, nar_url, content, source, last_accessed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                info.store_path, path.hash, info.url, info.to_string(), source, accessed_at
            ],
        )?;
        for reference in &info.references {
            transaction
                .execute(
                    "INSERT INTO reference (referrer, reference) VALUES (?1, ?2)",
                    rusqlite::params![info.store_path, reference],
                )
                .map_err(|error| match error {
                    rusqlite::Error::SqliteFailure(ref failure, _)
                        if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
                    {
                        DatabaseError::MissingReference
                    }
                    _ => DatabaseError::Sqlite(error),
                })?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Fetch the wire content serialized by `NarInfo` at commit time.
    pub fn narinfo(&self, store_hash: &str) -> Result<Option<String>, DatabaseError> {
        Ok(self
            .reader()?
            .query_row(
                "SELECT content FROM narinfo WHERE store_hash = ?1",
                [store_hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Resolve a registered NAR URL to its immutable blob and stored size.
    pub fn nar_blob(&self, url: &str) -> Result<Option<Blob>, DatabaseError> {
        Ok(self
            .reader()?
            .query_row(
                "SELECT blob.sha256, blob.size FROM nar JOIN blob ON nar.blob_sha256 = blob.sha256
             WHERE nar.url = ?1",
                [url],
                |row| {
                    Ok(Blob {
                        hash: row.get(0)?,
                        size: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Record a narinfo read by store hash, returning the number of updated rows.
    /// `now` is Unix time in seconds. The persisted timestamp (initially the
    /// upload time) limits updates to once per hour, including across restarts.
    pub fn record_narinfo_access(
        &self,
        store_hash: &str,
        now: i64,
    ) -> Result<usize, DatabaseError> {
        Ok(self.writer()?.execute(
            "UPDATE narinfo SET last_accessed_at = ?2
             WHERE store_hash = ?1 AND ?2 - last_accessed_at >= 3600",
            rusqlite::params![store_hash, now],
        )?)
    }

    /// Record a NAR read for every linked narinfo, with the same per-entry limit.
    /// `now` is Unix time in seconds; absent URLs update no rows.
    pub fn record_nar_access(&self, nar_url: &str, now: i64) -> Result<usize, DatabaseError> {
        Ok(self.writer()?.execute(
            "UPDATE narinfo SET last_accessed_at = ?2
             WHERE nar_url = ?1 AND ?2 - last_accessed_at >= 3600",
            rusqlite::params![nar_url, now],
        )?)
    }

    /// Serialize writes through the store's sole writer connection.
    pub fn writer(&self) -> Result<MutexGuard<'_, Connection>, DatabaseError> {
        self.writer
            .lock()
            .map_err(|_| DatabaseError::WriterPoisoned)
    }

    /// Each reader has an independent, read-only connection and snapshot,
    /// reused from a small pool to avoid an open and schema parse per request.
    pub fn reader(&self) -> Result<Reader<'_>, DatabaseError> {
        let pooled = self
            .readers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let connection = match pooled {
            Some(connection) => connection,
            None => {
                let reader =
                    Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                reader.execute_batch("PRAGMA foreign_keys = ON;")?;
                reader
            }
        };
        Ok(Reader {
            pool: &self.readers,
            connection: Some(connection),
        })
    }
}

fn check_version(connection: &Connection, supported: usize) -> Result<usize, DatabaseError> {
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found < 0 || found > supported as i64 {
        return Err(DatabaseError::UnsupportedSchema { found, supported });
    }
    Ok(found as usize)
}

fn migrate(connection: &mut Connection, migrations: &[&str]) -> Result<Vec<usize>, DatabaseError> {
    let version = check_version(connection, migrations.len())?;
    let transaction = connection.transaction()?;
    for (index, sql) in migrations.iter().enumerate().skip(version) {
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", index + 1)?;
    }
    transaction.commit()?;
    Ok((version + 1..=migrations.len()).collect())
}

#[cfg(test)]
mod tests;
