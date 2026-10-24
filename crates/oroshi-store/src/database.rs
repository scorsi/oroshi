use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags};

const MIGRATIONS: &[&str] = &[include_str!("schema.sql")];

#[derive(Debug)]
pub enum DatabaseError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    UnsupportedSchema { found: i64, supported: usize },
    WriterPoisoned,
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "database filesystem error: {error}"),
            Self::Sqlite(error) => write!(f, "database SQLite error: {error}"),
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
