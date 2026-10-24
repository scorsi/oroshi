use super::*;
use axum::body::{Bytes, HttpBody, to_bytes};
use oroshi_nix::NarInfo;
use std::pin::Pin;

pub(crate) const FIXTURE: &str = include_str!(
    "../../../../crates/oroshi-nix/tests/fixtures/narinfo/cache-np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x.narinfo"
);
pub(crate) const HASH: &str = "np8ldxalhj7qkmmf3p5gxx2d9nnm5q3x";

pub(crate) fn fixture() -> (tempfile::TempDir, Arc<Store>, Vec<u8>, NarInfo) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store {
        blobs: BlobStore::open(root.path()).unwrap(),
        database: Database::open(root.path()).unwrap(),
        priority: 17,
    });
    // Payload identity is tested here; ingest hash/signature validation belongs to task 11.
    let bytes: Vec<u8> = (0..200_000).map(|n| (n % 251) as u8).collect();
    let blob = store.blobs.write(bytes.as_slice(), u64::MAX, None).unwrap();
    let info: NarInfo = FIXTURE.parse().unwrap();
    {
        let writer = store.database.writer().unwrap();
        writer
            .execute(
                "INSERT INTO blob VALUES (?1, ?2, 0)",
                (&blob.hash[..], blob.size),
            )
            .unwrap();
        writer
            .execute(
                "INSERT INTO nar VALUES (?1, ?2)",
                (&info.url, &blob.hash[..]),
            )
            .unwrap();
    }
    store.database.commit_narinfo(&info, "upload", 0).unwrap();
    store
        .database
        .writer()
        .unwrap()
        .execute_batch(
            "CREATE TABLE access_updates (count INTEGER NOT NULL);
         INSERT INTO access_updates VALUES (0);
         CREATE TRIGGER count_access AFTER UPDATE OF last_accessed_at ON narinfo
         BEGIN UPDATE access_updates SET count = count + 1; END;",
        )
        .unwrap();
    (root, store, bytes, info)
}

async fn body(response: Response) -> Bytes {
    to_bytes(response.into_body(), usize::MAX).await.unwrap()
}

#[tokio::test]
async fn cache_info_uses_configured_priority() {
    let (_root, store, _, _) = fixture();
    assert_eq!(
        cache_info(State(store)).await,
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 17\n"
    );
}

#[tokio::test]
async fn committed_narinfo_and_streamed_nar_preserve_bytes_and_batch_access() {
    let (_root, store, bytes, info) = fixture();
    let response = narinfo(State(store.clone()), Path(format!("{HASH}.narinfo")))
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/x-nix-narinfo"
    );
    assert_eq!(body(response).await.as_ref(), FIXTURE.as_bytes());
    let accessed = || {
        store
            .database
            .reader()
            .unwrap()
            .query_row::<i64, _, _>("SELECT last_accessed_at FROM narinfo", [], |row| row.get(0))
            .unwrap()
    };
    let first = accessed();
    assert!(first > 0);
    let file = info.url.strip_prefix("nar/").unwrap();
    for method in [Method::GET, Method::HEAD] {
        let response = nar(State(store.clone()), Path(file.to_owned()), method.clone())
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-nix-nar"
        );
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            bytes.len().to_string()
        );
        let actual = if method == Method::GET {
            let mut stream = response.into_body();
            let frame = std::future::poll_fn(|cx| Pin::new(&mut stream).poll_frame(cx))
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            assert!(
                frame.len() < bytes.len(),
                "the NAR must arrive in bounded chunks"
            );
            let mut actual = frame.to_vec();
            actual.extend_from_slice(&to_bytes(stream, usize::MAX).await.unwrap());
            Bytes::from(actual)
        } else {
            body(response).await
        };
        assert_eq!(
            actual.as_ref(),
            if method == Method::HEAD {
                &[]
            } else {
                &bytes[..]
            }
        );
    }
    assert_eq!(accessed(), first);
    assert_eq!(
        store
            .database
            .reader()
            .unwrap()
            .query_row::<i64, _, _>("SELECT count FROM access_updates", [], |row| row.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .database
            .record_narinfo_access(HASH, first + 3599)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .database
            .record_narinfo_access(HASH, first + 3600)
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn malformed_names_and_unknown_entries_are_not_found() {
    let (_root, store, _, _) = fixture();
    for name in [
        "bad.narinfo",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.narinfo",
        "00000000000000000000000000000000.narinfo",
        "hello.ls",
    ] {
        assert_eq!(
            narinfo(State(store.clone()), Path(name.into()))
                .await
                .unwrap_err(),
            StatusCode::NOT_FOUND
        );
    }
    for name in [
        "../secret",
        "example.nar",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.nar",
        "z000000000000000000000000000000000000000000000000000.nar",
        "0000000000000000000000000000000000000000000000000000.nar.gz",
        "0000000000000000000000000000000000000000000000000000.nar",
    ] {
        assert_eq!(
            nar(State(store.clone()), Path(name.into()), Method::GET)
                .await
                .unwrap_err(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn registered_but_missing_blob_is_a_server_error_without_access_update() {
    let (root, store, _, info) = fixture();
    std::fs::remove_dir_all(root.path().join("blobs")).unwrap();
    assert_eq!(
        nar(
            State(store.clone()),
            Path(info.url[4..].into()),
            Method::GET
        )
        .await
        .unwrap_err(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let time: i64 = store
        .database
        .reader()
        .unwrap()
        .query_row("SELECT last_accessed_at FROM narinfo", [], |row| row.get(0))
        .unwrap();
    assert_eq!(time, 0);
}

#[tokio::test]
async fn nar_get_and_head_alone_record_access_once() {
    for method in [Method::GET, Method::HEAD] {
        let (_root, store, _, info) = fixture();
        for _ in 0..2 {
            nar(
                State(store.clone()),
                Path(info.url[4..].into()),
                method.clone(),
            )
            .await
            .unwrap();
        }
        let reader = store.database.reader().unwrap();
        assert!(
            reader
                .query_row::<i64, _, _>("SELECT last_accessed_at FROM narinfo", [], |row| row
                    .get(0))
                .unwrap()
                > 0
        );
        assert_eq!(
            reader
                .query_row::<i64, _, _>("SELECT count FROM access_updates", [], |row| row.get(0))
                .unwrap(),
            1
        );
    }
}

#[tokio::test]
async fn all_protocol_nar_suffixes_are_readable() {
    let (_root, store, bytes, info) = fixture();
    let stem = info.url.strip_suffix(".nar.xz").unwrap();
    for suffix in [".nar", ".nar.zst", ".nar.bz2"] {
        let url = format!("{stem}{suffix}");
        store
            .database
            .writer()
            .unwrap()
            .execute(
                "INSERT INTO nar SELECT ?1, blob_sha256 FROM nar WHERE url = ?2",
                (&url, &info.url),
            )
            .unwrap();
        let response = nar(State(store.clone()), Path(url[4..].into()), Method::GET)
            .await
            .unwrap();
        assert_eq!(body(response).await.as_ref(), bytes.as_slice());
    }
}
