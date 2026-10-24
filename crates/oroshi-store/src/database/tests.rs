use super::*;

fn version(connection: &Connection) -> i64 {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn seed(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO blob VALUES (zeroblob(32), 123, 1000);
             INSERT INTO nar VALUES ('nar/example.nar.zst', zeroblob(32));
             INSERT INTO narinfo VALUES (
                 '/nix/store/00000000000000000000000000000000-example',
                 '00000000000000000000000000000000',
                 'nar/example.nar.zst', 'narinfo content', 'upload', 1000
             );",
        )
        .unwrap();
}

#[test]
fn empty_database_creates_the_schema_in_the_specified_layout() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("state");
    let database = Database::open(&root).unwrap();
    assert!(root.join("db/oroshi.sqlite").is_file());
    let writer = database.writer().unwrap();
    assert_eq!(version(&writer), MIGRATIONS.len() as i64);
    let tables: Vec<String> = writer
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(tables, ["blob", "nar", "narinfo", "reference"]);
    seed(&writer);
    let source: String = writer
        .query_row("SELECT source FROM narinfo", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source, "upload");
}

#[test]
fn reopening_preserves_rows_without_reapplying_migrations() {
    let root = tempfile::tempdir().unwrap();
    let schema_version: i64;
    {
        let database = Database::open(root.path()).unwrap();
        let writer = database.writer().unwrap();
        seed(&writer);
        schema_version = writer
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .unwrap();
    }
    let database = Database::open(root.path()).unwrap();
    let reader = database.reader().unwrap();
    assert_eq!(version(&reader), MIGRATIONS.len() as i64);
    assert_eq!(
        reader
            .pragma_query_value::<i64, _>(None, "schema_version", |row| row.get(0))
            .unwrap(),
        schema_version
    );
    assert_eq!(
        reader
            .query_row::<String, _, _>("SELECT content FROM narinfo", [], |row| row.get(0))
            .unwrap(),
        "narinfo content"
    );
}

#[test]
fn connections_enable_wal_foreign_keys_and_durable_writes() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let writer = database.writer().unwrap();
    let readers = [database.reader().unwrap(), database.reader().unwrap()];
    for connection in [&*writer, &readers[0], &readers[1]] {
        assert_eq!(
            connection
                .pragma_query_value::<String, _>(None, "journal_mode", |row| row.get(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "foreign_keys", |row| row.get(0))
                .unwrap(),
            1
        );
    }
    assert_eq!(
        writer
            .pragma_query_value::<i64, _>(None, "synchronous", |row| row.get(0))
            .unwrap(),
        2
    );
    for reader in readers {
        let error = reader
            .execute("INSERT INTO blob VALUES (zeroblob(32), 0, 0)", [])
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly)
        );
    }
}

#[test]
fn one_writer_is_serialized_while_readers_keep_independent_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let mut writer = database.writer().unwrap();
    assert!(matches!(
        database.writer.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    let mut reader = database.reader().unwrap();
    let snapshot = reader.transaction().unwrap();
    let count = |connection: &Connection| {
        connection
            .query_row::<i64, _, _>("SELECT count(*) FROM blob", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(count(&snapshot), 0);
    let transaction = writer.transaction().unwrap();
    seed(&transaction);
    assert_eq!(count(&database.reader().unwrap()), 0);
    transaction.commit().unwrap();
    assert_eq!(count(&snapshot), 0);
    assert_eq!(count(&database.reader().unwrap()), 1);
    snapshot.commit().unwrap();
    assert_eq!(count(&reader), 1);
    drop(writer);
    assert!(database.writer().is_ok());
}

#[test]
fn newer_and_negative_schema_versions_are_refused_without_modification() {
    for found in [MIGRATIONS.len() as i64 + 1, -1] {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("db")).unwrap();
        let path = root.path().join("db/oroshi.sqlite");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "user_version", found)
                .unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            Database::open(root.path()),
            Err(DatabaseError::UnsupportedSchema { found: actual, supported })
                if actual == found && supported == MIGRATIONS.len()
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!root.path().join("db/oroshi.sqlite-wal").exists());
    }
}

#[test]
fn failing_migration_rolls_back_ddl_data_and_version_but_keeps_prior_migrations() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("migration.sqlite");
    let mut connection = Connection::open(&path).unwrap();
    let migrations = [
        "CREATE TABLE existing (value TEXT); INSERT INTO existing VALUES ('original');",
        "CREATE TABLE partial (value TEXT);
         UPDATE existing SET value = 'changed';
         INSERT INTO nonexistent VALUES (1);",
    ];
    assert!(matches!(
        migrate(&mut connection, &migrations),
        Err(DatabaseError::Sqlite(_))
    ));
    assert!(connection.is_autocommit());
    drop(connection);
    let connection = Connection::open(path).unwrap();
    assert_eq!(version(&connection), 1);
    assert_eq!(
        connection
            .query_row::<String, _, _>("SELECT value FROM existing", [], |row| row.get(0))
            .unwrap(),
        "original"
    );
    assert_eq!(
        connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'partial'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn schema_enforces_keys_sizes_and_foreign_keys() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let writer = database.writer().unwrap();
    seed(&writer);
    writer
        .execute_batch(
            "INSERT INTO reference VALUES (
                '/nix/store/00000000000000000000000000000000-example',
                '/nix/store/00000000000000000000000000000000-example'
             );",
        )
        .unwrap();
    for sql in [
        "INSERT INTO blob VALUES (zeroblob(32), 1, 0)",
        "INSERT INTO blob VALUES (zeroblob(31), 1, 0)",
        "INSERT INTO blob VALUES ('00000000000000000000000000000000', 1, 0)",
        "INSERT INTO blob VALUES (randomblob(32), -1, 0)",
        "INSERT INTO blob VALUES (randomblob(32), 1.5, 0)",
        "INSERT INTO blob VALUES (randomblob(32), 'invalid', 0)",
        "INSERT INTO blob VALUES (NULL, 1, 0)",
        "INSERT INTO nar VALUES ('nar/missing.nar', randomblob(32))",
        "INSERT INTO nar VALUES ('nar/example.nar.zst', zeroblob(32))",
        "UPDATE narinfo SET nar_url = 'nar/missing.nar'",
        "UPDATE narinfo SET source = NULL",
        "UPDATE narinfo SET store_hash = 'short'",
        "INSERT INTO narinfo SELECT store_path, store_hash, nar_url, content, source,
             last_accessed_at FROM narinfo",
        "INSERT INTO narinfo SELECT store_path || '-other', store_hash, nar_url, content,
             source, last_accessed_at FROM narinfo",
        "INSERT INTO reference SELECT * FROM reference",
        "INSERT INTO reference SELECT referrer, '/nix/store/missing' FROM reference",
        "INSERT INTO reference SELECT '/nix/store/missing', reference FROM reference",
        "DELETE FROM blob",
        "DELETE FROM nar",
        "DELETE FROM narinfo",
    ] {
        let error = writer.execute_batch(sql).expect_err(sql);
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
            "{sql}: {error}"
        );
    }
}

#[test]
fn invalid_database_directory_returns_a_filesystem_error() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("db"), b"not a directory").unwrap();
    assert!(matches!(
        Database::open(root.path()),
        Err(DatabaseError::Io(_))
    ));
}

fn libiconv() -> NarInfo {
    include_str!(
        "../../../oroshi-nix/tests/fixtures/narinfo/cache-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo"
    )
    .parse()
    .unwrap()
}

fn hello() -> NarInfo {
    include_str!(
        "../../../oroshi-nix/tests/fixtures/narinfo/cache-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo"
    )
    .parse()
    .unwrap()
}

fn register_nar(database: &Database, info: &NarInfo) {
    let writer = database.writer().unwrap();
    writer
        .execute(
            "INSERT INTO blob (sha256, size, created_at) VALUES (?1, ?2, 1000)",
            rusqlite::params![info.file_hash.unwrap(), info.file_size.unwrap()],
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO nar (url, blob_sha256) VALUES (?1, ?2)",
            rusqlite::params![info.url, info.file_hash.unwrap()],
        )
        .unwrap();
}

fn stored_paths(database: &Database) -> Vec<String> {
    database
        .reader()
        .unwrap()
        .prepare("SELECT store_path FROM narinfo ORDER BY store_path")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn edges(database: &Database) -> Vec<(String, String)> {
    database
        .reader()
        .unwrap()
        .prepare("SELECT referrer, reference FROM reference ORDER BY referrer, reference")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn assert_constraint(error: DatabaseError) {
    let DatabaseError::Sqlite(error) = error else {
        panic!("expected SQLite constraint failure, got {error}");
    };
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    );
}

#[test]
fn absent_reference_refuses_commit_without_leaving_a_narinfo_or_edges() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let info = hello();
    register_nar(&database, &info);
    assert_constraint(database.commit_narinfo(&info, "upload", 2000).unwrap_err());
    assert!(stored_paths(&database).is_empty());
    assert!(edges(&database).is_empty());
    assert!(database.writer().unwrap().is_autocommit());
}

#[test]
fn self_reference_commits_content_nar_blob_link_and_metadata() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let info = libiconv();
    register_nar(&database, &info);
    database.commit_narinfo(&info, "upload", 2000).unwrap();
    assert_eq!(
        stored_paths(&database),
        std::slice::from_ref(&info.store_path)
    );
    assert_eq!(
        edges(&database),
        [(info.store_path.clone(), info.store_path.clone())]
    );
    let stored: (String, String, String, i64, Vec<u8>) = database
        .reader()
        .unwrap()
        .query_row(
            "SELECT store_hash, content, source, last_accessed_at, blob.sha256
             FROM narinfo JOIN nar ON nar.url = narinfo.nar_url
             JOIN blob ON blob.sha256 = nar.blob_sha256",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(stored.0, parse_store_path(&info.store_path).unwrap().hash);
    assert_eq!(stored.1.parse::<NarInfo>().unwrap(), info);
    assert_eq!(stored.2, "upload");
    assert_eq!(stored.3, 2000);
    assert_eq!(stored.4, info.file_hash.unwrap());
}

#[test]
fn dependency_order_allows_a_chain_and_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let dependency = libiconv();
    let dependent = hello();
    register_nar(&database, &dependency);
    register_nar(&database, &dependent);
    database
        .commit_narinfo(&dependency, "upload", 2000)
        .unwrap();
    database.commit_narinfo(&dependent, "upload", 2001).unwrap();
    drop(database);
    let database = Database::open(root.path()).unwrap();
    assert_eq!(
        stored_paths(&database),
        [dependent.store_path.clone(), dependency.store_path.clone()]
    );
    assert_eq!(
        edges(&database),
        [
            (dependent.store_path, dependency.store_path.clone()),
            (dependency.store_path.clone(), dependency.store_path),
        ]
    );
}

#[test]
fn constraint_failure_after_an_edge_insert_rolls_back_only_the_new_commit() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let dependency = libiconv();
    let mut dependent = hello();
    dependent.references.insert(dependent.store_path.clone());
    register_nar(&database, &dependency);
    register_nar(&database, &dependent);
    database
        .commit_narinfo(&dependency, "upload", 2000)
        .unwrap();
    // Fail on the second edge only after the first edge has actually been inserted.
    database
        .writer()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_second_edge BEFORE INSERT ON reference
         WHEN NEW.referrer != NEW.reference AND EXISTS (
             SELECT 1 FROM reference WHERE referrer = NEW.referrer AND reference = NEW.referrer
         ) BEGIN SELECT RAISE(ABORT, 'injected constraint failure'); END;",
        )
        .unwrap();
    assert_constraint(
        database
            .commit_narinfo(&dependent, "upload", 2001)
            .unwrap_err(),
    );
    assert_eq!(
        stored_paths(&database),
        std::slice::from_ref(&dependency.store_path)
    );
    assert_eq!(
        edges(&database),
        [(dependency.store_path.clone(), dependency.store_path)]
    );
    assert!(database.writer().unwrap().is_autocommit());
    database
        .writer()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_second_edge")
        .unwrap();
    database.commit_narinfo(&dependent, "upload", 2001).unwrap();
    assert_eq!(edges(&database).len(), 3);
}

#[test]
fn absent_nar_refuses_commit_without_rows() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    assert_constraint(
        database
            .commit_narinfo(&libiconv(), "upload", 2000)
            .unwrap_err(),
    );
    assert!(stored_paths(&database).is_empty());
    assert!(edges(&database).is_empty());
}

#[test]
fn duplicate_path_never_overwrites_existing_metadata() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let info = libiconv();
    register_nar(&database, &info);
    database.commit_narinfo(&info, "upload", 2000).unwrap();
    let mut changed = info.clone();
    changed.nar_size += 1;
    assert_constraint(
        database
            .commit_narinfo(&changed, "other", 3000)
            .unwrap_err(),
    );
    let content: String = database
        .reader()
        .unwrap()
        .query_row("SELECT content FROM narinfo", [], |row| row.get(0))
        .unwrap();
    assert_eq!(content.parse::<NarInfo>().unwrap(), info);
    assert_eq!(
        edges(&database),
        [(info.store_path.clone(), info.store_path)]
    );
}

#[test]
fn invalid_store_path_returns_an_error_without_writing() {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(root.path()).unwrap();
    let mut info = libiconv();
    info.store_path = "/nix/store/invalid".into();
    assert!(matches!(
        database.commit_narinfo(&info, "upload", 2000),
        Err(DatabaseError::InvalidStorePath(_))
    ));
    assert!(stored_paths(&database).is_empty());
}
