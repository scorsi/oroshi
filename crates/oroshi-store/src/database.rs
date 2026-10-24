use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use oroshi_nix::{NarInfo, ParseError, parse_store_path};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::Blob;

const MIGRATIONS: &[&str] = &[include_str!("schema.sql")];

#[derive(Debug)]
pub enum DatabaseError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    InvalidStorePath(ParseError),
    UnsupportedSchema { found: i64, supported: usize },
    WriterPoisoned,
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
            Self::WriterPoisoned => f.write_str("database writer lock poisoned"),
        }
    }
}

impl std::error::Error for DatabaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidStorePath(error) => Some(error),
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
}

impl Database {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let directory = root.as_ref().join("db");
        fs::create_dir_all(&directory)?;
        let path = directory.join("oroshi.sqlite");
        let mut writer = Connection::open(&path)?;
        // Refuse incompatible databases before changing their persistent journal mode.
        check_version(&writer, MIGRATIONS.len())?;
        let mode: String = writer.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if mode != "wal" {
            return Err(io::Error::other("SQLite did not enable WAL mode").into());
        }
        writer.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
        migrate(&mut writer, MIGRATIONS)?;
        Ok(Self {
            path,
            writer: Mutex::new(writer),
        })
    }

    /// Commit a parsed narinfo, its existing NAR link and all reference edges atomically.
    /// The NAR must already be registered by its upload. Foreign keys refuse absent
    /// dependencies; inserting the narinfo first permits its self-edge (ADR 0005).
    /// Existing paths are refused; validation and signature merge belong to O1 task 11.
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
        transaction.execute(
            "INSERT INTO narinfo (store_path, store_hash, nar_url, content, source, last_accessed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                info.store_path, path.hash, info.url, info.to_string(), source, accessed_at
            ],
        )?;
        for reference in &info.references {
            transaction.execute(
                "INSERT INTO reference (referrer, reference) VALUES (?1, ?2)",
                rusqlite::params![info.store_path, reference],
            )?;
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

    /// Each reader has an independent, read-only connection and snapshot.
    pub fn reader(&self) -> Result<Connection, DatabaseError> {
        let reader = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(reader)
    }
}

fn check_version(connection: &Connection, supported: usize) -> Result<usize, DatabaseError> {
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found < 0 || found > supported as i64 {
        return Err(DatabaseError::UnsupportedSchema { found, supported });
    }
    Ok(found as usize)
}

fn migrate(connection: &mut Connection, migrations: &[&str]) -> Result<(), DatabaseError> {
    let version = check_version(connection, migrations.len())?;
    for (index, sql) in migrations.iter().enumerate().skip(version) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", index + 1)?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
