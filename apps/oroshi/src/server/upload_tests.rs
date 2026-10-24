use super::*;
use axum::body::{Body, to_bytes};
use oroshi_nix::encode_base32;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::ReaderStream;

pub(super) fn fixture() -> (tempfile::TempDir, Arc<crate::nix::Store>, Router) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::nix::Store {
        blobs: oroshi_store::BlobStore::open(root.path()).unwrap(),
        database: oroshi_store::Database::open(root.path()).unwrap(),
        priority: 30,
        trusted_keys: vec![
            include_str!(
                "../../../../crates/oroshi-nix/tests/fixtures/narinfo/test.example.org-1.pub"
            )
            .trim()
            .parse()
            .unwrap(),
        ],
    });
    let config = Config::parse(&format!(
        "[tokens.writer]\nsha256 = {:?}\nscopes = ['nix:write']\n\
         [tokens.reader]\nsha256 = {:?}\nscopes = ['nix:read']\n",
        format!("{:x}", Sha256::digest(b"secret")),
        format!("{:x}", Sha256::digest(b"reader")),
    ))
    .unwrap();
    let app = app(store.clone(), Arc::new(Registry::new()), &config);
    (root, store, app)
}

fn path(bytes: &[u8], suffix: &str) -> String {
    format!("/nix/nar/{}{suffix}", encode_base32(&Sha256::digest(bytes)))
}

pub(super) async fn request(
    mut app: impl axum::ServiceExt<Request, Response = Response, Error = Infallible>,
    method: &str,
    path: &str,
    credential: Option<&str>,
    body: Body,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(credential) = credential {
        request = request.header(header::AUTHORIZATION, credential);
    }
    app.call(request.body(body).unwrap()).await.unwrap()
}

pub(super) fn row_count(store: &crate::nix::Store, table: &str) -> i64 {
    store
        .database
        .reader()
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn assert_empty(root: &std::path::Path, store: &crate::nix::Store, bytes: &[u8]) {
    assert_eq!(row_count(store, "nar"), 0);
    assert_eq!(row_count(store, "blob"), 0);
    assert!(
        store
            .blobs
            .open_blob(&Sha256::digest(bytes).into())
            .is_err()
    );
    assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(root.join("blobs/sha256"))
            .unwrap()
            .count(),
        0
    );
}

// Each new chunk requires the previous one to have reached the temp file.
struct StreamingBody {
    root: PathBuf,
    written: usize,
    size: usize,
    fail: bool,
}

impl AsyncRead for StreamingBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.written > 0 {
            let files: Vec<_> = std::fs::read_dir(self.root.join("tmp"))
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].metadata().unwrap().len(), self.written as u64);
        }
        if self.written == self.size {
            return Poll::Ready(if self.fail {
                Err(io::Error::other("interrupted request body"))
            } else {
                Ok(())
            });
        }
        let length = buffer.remaining().min(self.size - self.written).min(4096);
        buffer.put_slice(&vec![42; length]);
        self.written += length;
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn streamed_uploads_accept_each_suffix_and_are_anonymously_readable() {
    for suffix in [".nar", ".nar.xz", ".nar.zst", ".nar.bz2"] {
        let (root, store, app) = fixture();
        let bytes = vec![42; 3 * 1024 * 1024];
        let path = path(&bytes, suffix);
        let body = Body::from_stream(ReaderStream::new(StreamingBody {
            root: root.path().to_owned(),
            written: 0,
            size: bytes.len(),
            fail: false,
        }));
        let response = request(app.clone(), "PUT", &path, Some("Bearer secret"), body).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let blob = store.database.nar_blob(&path[5..]).unwrap().unwrap();
        assert_eq!(blob.hash, <[u8; 32]>::from(Sha256::digest(&bytes)));
        assert_eq!(blob.size, bytes.len() as u64);
        assert_eq!(row_count(&store, "nar"), 1);
        assert_eq!(row_count(&store, "blob"), 1);
        assert_eq!(row_count(&store, "narinfo"), 0);
        let response = request(app, "GET", &path, None, Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            bytes.len().to_string()
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
            0
        );
    }
}

#[tokio::test]
async fn hash_mismatch_returns_400_without_blob_or_metadata() {
    let (root, store, app) = fixture();
    let bytes = b"wrong body";
    let response = request(
        app,
        "PUT",
        &path(b"expected", ".nar.zst"),
        Some("secret"),
        Body::from(&bytes[..]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_empty(root.path(), &store, bytes);
}

#[tokio::test]
async fn uploads_require_write_scope_and_accept_all_credential_forms() {
    for (credential, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("unknown"), StatusCode::UNAUTHORIZED),
        (Some("reader"), StatusCode::FORBIDDEN),
        (Some("secret"), StatusCode::NO_CONTENT),
        (Some("Bearer secret"), StatusCode::NO_CONTENT),
        (Some("Basic dXNlcjpzZWNyZXQ="), StatusCode::NO_CONTENT),
    ] {
        let (root, store, app) = fixture();
        let response = request(
            app,
            "PUT",
            &path(b"data", ".nar"),
            credential,
            Body::from("data"),
        )
        .await;
        assert_eq!(response.status(), status);
        if status == StatusCode::UNAUTHORIZED {
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE],
                "Basic realm=\"oroshi\""
            );
        }
        if status.is_client_error() {
            assert_empty(root.path(), &store, b"data");
        } else {
            assert_eq!(row_count(&store, "nar"), 1);
        }
    }
}

#[tokio::test]
async fn unsupported_suffixes_and_malformed_hashes_are_forbidden() {
    let (root, store, app) = fixture();
    for file in [
        format!("{}.nar.gz", encode_base32(&Sha256::digest(b"data"))),
        format!("{}.nar.zstd", encode_base32(&Sha256::digest(b"data"))),
        "bad.nar".into(),
        format!("{}.nar", "e".repeat(52)),
        format!("z{}.nar", "0".repeat(51)),
        format!("{}.nar", "0".repeat(51)),
    ] {
        let response = request(
            app.clone(),
            "PUT",
            &format!("/nix/nar/{file}"),
            Some("secret"),
            Body::from("data"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{file}");
        assert_empty(root.path(), &store, b"data");
    }
}

#[tokio::test]
async fn duplicate_upload_preserves_blob_and_metadata_and_still_verifies_body() {
    use std::os::unix::fs::MetadataExt;
    let (root, store, app) = fixture();
    let path = path(b"data", ".nar.xz");
    let response = request(
        app.clone(),
        "PUT",
        &path,
        Some("secret"),
        Body::from("data"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let hash = Sha256::digest(b"data").into();
    let before = store.blobs.open_blob(&hash).unwrap().metadata().unwrap();
    store
        .database
        .writer()
        .unwrap()
        .execute("UPDATE blob SET created_at = 1", [])
        .unwrap();
    for (body, status) in [
        ("data", StatusCode::NO_CONTENT),
        ("wrong", StatusCode::BAD_REQUEST),
    ] {
        let response = request(app.clone(), "PUT", &path, Some("secret"), Body::from(body)).await;
        assert_eq!(response.status(), status);
    }
    let after = store.blobs.open_blob(&hash).unwrap().metadata().unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(row_count(&store, "nar"), 1);
    assert_eq!(row_count(&store, "blob"), 1);
    assert_eq!(
        store
            .database
            .reader()
            .unwrap()
            .query_row::<i64, _, _>("SELECT created_at FROM blob", [], |row| row.get(0))
            .unwrap(),
        1
    );
    let response = request(
        app,
        "PUT",
        &self::path(b"data", ".nar.zst"),
        Some("secret"),
        Body::from("data"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(row_count(&store, "nar"), 2);
    assert_eq!(row_count(&store, "blob"), 1);
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn interrupted_body_leaves_no_blob_or_metadata() {
    let (root, store, app) = fixture();
    let bytes = vec![42; 8192];
    let body = Body::from_stream(ReaderStream::new(StreamingBody {
        root: root.path().to_owned(),
        written: 0,
        size: bytes.len(),
        fail: true,
    }));
    let response = request(app, "PUT", &path(&bytes, ".nar"), Some("secret"), body).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_empty(root.path(), &store, &bytes);
}

#[tokio::test]
async fn metadata_failure_rolls_back_rows_and_leaves_a_published_orphan() {
    let (_root, store, app) = fixture();
    store.database.writer().unwrap().execute_batch(
        "CREATE TRIGGER refuse_nar BEFORE INSERT ON nar BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
    ).unwrap();
    let response = request(
        app,
        "PUT",
        &path(b"data", ".nar"),
        Some("secret"),
        Body::from("data"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(row_count(&store, "nar"), 0);
    assert_eq!(row_count(&store, "blob"), 0);
    assert!(
        store
            .blobs
            .open_blob(&Sha256::digest(b"data").into())
            .is_ok()
    );
}
