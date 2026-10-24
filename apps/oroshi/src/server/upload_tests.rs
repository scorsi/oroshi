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
    fixture_with_limits(crate::nix::UploadLimits::new(&Config::default()))
}

fn fixture_with_limits(
    limits: crate::nix::UploadLimits,
) -> (tempfile::TempDir, Arc<crate::nix::Store>, Router) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::nix::Store {
        blobs: oroshi_store::BlobStore::open(root.path()).unwrap(),
        database: oroshi_store::Database::open(root.path()).unwrap().0,
        limits,
        uploads: Default::default(),
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
    app: impl axum::ServiceExt<Request, Response = Response, Error = Infallible>,
    method: &str,
    path: &str,
    credential: Option<&str>,
    body: Body,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(credential) = credential {
        request = request.header(header::AUTHORIZATION, credential);
    }
    send(app, request.body(body).unwrap()).await
}

async fn send(
    mut app: impl axum::ServiceExt<Request, Response = Response, Error = Infallible>,
    request: Request,
) -> Response {
    app.call(request).await.unwrap()
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

fn limits(config: &str) -> crate::nix::UploadLimits {
    crate::nix::UploadLimits::new(&Config::parse(config).unwrap())
}

#[tokio::test]
async fn nar_size_cap_refuses_during_stream_and_cleans_partial_file() {
    let (root, store, app) = fixture_with_limits(limits("[nix]\nmax_nar_bytes = 4096"));
    let bytes = vec![42; 16384];
    let body = Body::from_stream(ReaderStream::new(StreamingBody {
        root: root.path().to_owned(),
        written: 0,
        size: bytes.len(),
        fail: false,
    }));
    let response = request(app, "PUT", &path(&bytes, ".nar"), Some("secret"), body).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_empty(root.path(), &store, &bytes);
}

#[tokio::test]
async fn storage_cap_uses_stored_bytes_and_refuses_before_polling_when_full() {
    let (root, store, app) = fixture_with_limits(limits("[storage]\nmax_bytes = 4"));
    assert_eq!(store.database.stored_bytes().unwrap(), 0);
    let response = request(
        app.clone(),
        "PUT",
        &path(b"data", ".nar"),
        Some("secret"),
        Body::from("data"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(store.database.stored_bytes().unwrap(), 4);
    // A pending body must be refused immediately without waiting for EOF or idle timeout.
    let (reader, _writer) = tokio::io::duplex(1);
    let body = Body::from_stream(ReaderStream::new(reader));
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        request(app, "PUT", &path(b"new", ".nar"), Some("secret"), body),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(store.database.stored_bytes().unwrap(), 4);
    assert_eq!(row_count(&store, "nar"), 1);
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn storage_cap_refuses_during_stream_and_cleans_partial_file() {
    let (root, store, app) = fixture_with_limits(limits("[storage]\nmax_bytes = 4096"));
    let bytes = vec![42; 16384];
    let body = Body::from_stream(ReaderStream::new(StreamingBody {
        root: root.path().to_owned(),
        written: 0,
        size: bytes.len(),
        fail: false,
    }));
    let response = request(app, "PUT", &path(&bytes, ".nar"), Some("secret"), body).await;
    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_empty(root.path(), &store, &bytes);
}

struct PacedBody {
    delay: Pin<Box<tokio::time::Sleep>>,
    interval: std::time::Duration,
    remaining: usize,
}

impl AsyncRead for PacedBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.delay.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        if self.remaining > 0 {
            buffer.put_slice(b"x");
            self.remaining -= 1;
            let next = tokio::time::Instant::now() + self.interval;
            self.delay.as_mut().reset(next);
        }
        Poll::Ready(Ok(()))
    }
}

fn paced_body(interval: std::time::Duration, size: usize) -> Body {
    Body::from_stream(ReaderStream::new(PacedBody {
        delay: Box::pin(tokio::time::sleep(std::time::Duration::ZERO)),
        interval,
        remaining: size,
    }))
}

#[tokio::test]
async fn continuous_upload_hits_total_deadline_and_cleans_partial_file() {
    let mut limits = limits("");
    limits.deadline = std::time::Duration::from_millis(499);
    assert!(limits.deadline < std::time::Duration::from_millis(500));
    limits.idle_timeout = std::time::Duration::from_secs(1);
    let (root, store, app) = fixture_with_limits(limits);
    let bytes = vec![b'x'; 100];
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        request(
            app,
            "PUT",
            &path(&bytes, ".nar"),
            Some("secret"),
            paced_body(std::time::Duration::from_millis(10), bytes.len()),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "deadline"
    );
    assert_empty(root.path(), &store, &bytes);
}

#[tokio::test]
async fn idle_upload_times_out_and_cleans_partial_file() {
    let mut limits = limits("");
    assert_eq!(limits.idle_timeout, std::time::Duration::from_secs(60));
    limits.deadline = std::time::Duration::from_secs(1);
    limits.idle_timeout = std::time::Duration::from_millis(50);
    let (root, store, app) = fixture_with_limits(limits);
    let start = std::time::Instant::now();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        request(
            app,
            "PUT",
            &path(b"xx", ".nar"),
            Some("secret"),
            paced_body(std::time::Duration::from_secs(1), 2),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "idle"
    );
    assert!(start.elapsed() < std::time::Duration::from_millis(500));
    assert_empty(root.path(), &store, b"xx");
}

#[tokio::test]
async fn continuous_upload_within_all_limits_succeeds_at_size_boundary() {
    let mut limits = limits("[nix]\nmax_nar_bytes = 4\n[storage]\nmax_bytes = 4");
    limits.idle_timeout = std::time::Duration::from_millis(200);
    let (root, store, app) = fixture_with_limits(limits);
    let response = request(
        app,
        "PUT",
        &path(b"xxxx", ".nar"),
        Some("secret"),
        paced_body(std::time::Duration::from_millis(60), 4),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(store.database.stored_bytes().unwrap(), 4);
    assert_eq!(row_count(&store, "nar"), 1);
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn concurrent_uploads_cannot_spend_the_same_remaining_capacity() {
    let (root, store, app) = fixture_with_limits(limits("[storage]\nmax_bytes = 6"));
    let first_path = path(b"xxxx", ".nar");
    let second_path = path(b"data", ".nar");
    let (first, second) = tokio::join!(
        request(
            app.clone(),
            "PUT",
            &first_path,
            Some("secret"),
            paced_body(std::time::Duration::from_millis(10), 4)
        ),
        request(app, "PUT", &second_path, Some("secret"), Body::from("data")),
    );
    let mut statuses = [first.status(), second.status()];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::NO_CONTENT, StatusCode::INSUFFICIENT_STORAGE]
    );
    assert_eq!(store.database.stored_bytes().unwrap(), 4);
    assert_eq!(row_count(&store, "nar"), 1);
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn duplicate_hash_with_another_suffix_does_not_consume_capacity() {
    let (_root, store, app) = fixture_with_limits(limits("[storage]\nmax_bytes = 4"));
    for suffix in [".nar", ".nar.xz"] {
        let response = request(
            app.clone(),
            "PUT",
            &path(b"data", suffix),
            Some("secret"),
            Body::from("data"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    assert_eq!(store.database.stored_bytes().unwrap(), 4);
    assert_eq!(row_count(&store, "nar"), 2);
    assert_eq!(row_count(&store, "blob"), 1);
}

#[tokio::test]
async fn narinfo_uploads_also_enforce_idle_timeout() {
    let mut limits = limits("");
    limits.deadline = std::time::Duration::from_secs(1);
    limits.idle_timeout = std::time::Duration::from_millis(50);
    let (root, store, app) = fixture_with_limits(limits);
    let start = std::time::Instant::now();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        request(
            app,
            "PUT",
            &format!("/nix/{}.narinfo", crate::nix::tests::HASH),
            Some("secret"),
            paced_body(std::time::Duration::from_secs(1), 2),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "idle"
    );
    assert!(start.elapsed() < std::time::Duration::from_millis(500));
    assert_empty(root.path(), &store, b"xx");
}

#[tokio::test]
async fn storage_total_counts_all_distinct_blobs_without_narinfo() {
    let (root, store, app) = fixture_with_limits(limits("[storage]\nmax_bytes = 6"));
    for bytes in ["abc", "def"] {
        let response = request(
            app.clone(),
            "PUT",
            &path(bytes.as_bytes(), ".nar"),
            Some("secret"),
            Body::from(bytes),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    assert_eq!(store.database.stored_bytes().unwrap(), 6);
    assert_eq!(row_count(&store, "narinfo"), 0);
    let response = request(
        app,
        "PUT",
        &path(b"g", ".nar"),
        Some("secret"),
        Body::from("g"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(store.database.stored_bytes().unwrap(), 6);
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn upload_without_any_received_bytes_hits_idle_timeout() {
    let mut limits = limits("");
    limits.deadline = std::time::Duration::from_secs(1);
    limits.idle_timeout = std::time::Duration::from_millis(50);
    let (root, store, app) = fixture_with_limits(limits);
    let (reader, _writer) = tokio::io::duplex(1);
    let start = std::time::Instant::now();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        request(
            app,
            "PUT",
            &path(b"data", ".nar"),
            Some("secret"),
            Body::from_stream(ReaderStream::new(reader)),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert!(start.elapsed() < std::time::Duration::from_millis(500));
    assert_empty(root.path(), &store, b"data");
}

#[tokio::test]
async fn nar_refusal_reasons_are_plain_text_and_logged_verbatim() {
    for (config, bytes, expected, status) in [
        ("", "bad", "hash-mismatch", StatusCode::BAD_REQUEST),
        (
            "[nix]\nmax_nar_bytes = 2",
            "data",
            "too-large",
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "[storage]\nmax_bytes = 2",
            "data",
            "storage-full",
            StatusCode::INSUFFICIENT_STORAGE,
        ),
    ] {
        let (_root, store, _) = fixture_with_limits(limits(config));
        let registry = Arc::new(Registry::new());
        let app = app(store, registry, &super::observability_tests::config());
        let (response, lines, _) = super::observability_tests::logged_request(
            app,
            &path(b"data", ".nar"),
            Some("secret"),
            Body::from(bytes),
        )
        .await;
        assert_eq!(response.status(), status);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            expected
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["reason"], expected);
    }
}

#[tokio::test]
async fn non_read_methods_require_a_token_on_every_nix_route() {
    let (_root, _store, app) = fixture();
    let nar = path(b"data", ".nar");
    for method in ["POST", "DELETE", "PATCH"] {
        for target in ["/nix/nix-cache-info", "/nix/_oroshi/pin", nar.as_str()] {
            let response = request(app.clone(), method, target, None, Body::empty()).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {target}"
            );
        }
    }
    let authorized = request(
        app.clone(),
        "POST",
        "/nix/nix-cache-info",
        Some("secret"),
        Body::empty(),
    )
    .await;
    assert_eq!(authorized.status(), StatusCode::METHOD_NOT_ALLOWED);
    for method in ["GET", "HEAD"] {
        let response = request(
            app.clone(),
            method,
            "/nix/nix-cache-info",
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn capped_uploads_reserve_declared_bytes_and_run_in_parallel() {
    let (_root, store, app) = fixture_with_limits(limits("[storage]\nmax_bytes = 8"));
    let put = |target: String, body: Body| {
        send(
            app.clone(),
            Request::builder()
                .method("PUT")
                .uri(target)
                .header(header::AUTHORIZATION, "secret")
                .header(header::CONTENT_LENGTH, 4)
                .body(body)
                .unwrap(),
        )
    };
    let reserved = || *store.uploads.reserved.lock().unwrap();
    let permits = || store.uploads.permits.available_permits();
    // A stalled upload holds one permit and its 4 declared bytes, not the whole store.
    let (reader, writer) = tokio::io::duplex(1);
    let stalled = tokio::spawn(put(
        path(b"xxxx", ".nar"),
        Body::from_stream(ReaderStream::new(reader)),
    ));
    for _ in 0..500 {
        if reserved() == 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(reserved(), 4);
    assert_eq!(permits(), crate::nix::UPLOAD_CONCURRENCY - 1);
    let parallel = put(path(b"data", ".nar"), Body::from("data")).await;
    assert_eq!(parallel.status(), StatusCode::NO_CONTENT);
    let over = put(path(b"more", ".nar"), Body::from("more")).await;
    assert_eq!(over.status(), StatusCode::INSUFFICIENT_STORAGE);
    drop(writer);
    assert_eq!(stalled.await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!((reserved(), permits()), (0, crate::nix::UPLOAD_CONCURRENCY));
    let released = put(path(b"more", ".nar"), Body::from("more")).await;
    assert_eq!(released.status(), StatusCode::NO_CONTENT);
    assert_eq!(store.database.stored_bytes().unwrap(), 8);
}
