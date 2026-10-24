//! Nix cache HTTP routes (ADR 0006).
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use oroshi_nix::{NarInfo, PublicKey, decode_base32, parse_sha256, parse_store_path};
use oroshi_store::{BlobStore, Database, DatabaseError};
use tokio::time::{Duration, Instant, timeout_at};
use tokio_util::io::ReaderStream;

use crate::auth::{self, Tokens};
use crate::metrics::{Module, Registry, UploadResult};

pub struct Store {
    pub blobs: BlobStore,
    pub database: Database,
    pub priority: u32,
    pub trusted_keys: Vec<PublicKey>,
    pub limits: UploadLimits,
    pub uploads: Uploads,
}

const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Concurrent NAR uploads; each holds a blocking-pool thread while streaming.
pub(crate) const UPLOAD_CONCURRENCY: usize = 32;

/// In-flight NAR uploads: a concurrency limit and the bytes reserved against `max_bytes`.
pub struct Uploads {
    pub(crate) permits: Arc<tokio::sync::Semaphore>,
    pub(crate) reserved: std::sync::Mutex<u64>,
}

impl Default for Uploads {
    fn default() -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(UPLOAD_CONCURRENCY)),
            reserved: std::sync::Mutex::new(0),
        }
    }
}

/// Bytes held against the storage cap until the upload registers or fails.
struct Reservation<'a>(&'a std::sync::Mutex<u64>, u64);

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        // Registration commits before this runs, so stored + reserved never undercounts.
        if let Ok(mut reserved) = self.0.lock() {
            *reserved -= self.1;
        }
    }
}

pub struct UploadLimits {
    pub max_nar_bytes: u64,
    pub max_bytes: Option<u64>,
    pub deadline: Duration,
    #[cfg(test)]
    pub idle_timeout: Duration,
}

impl UploadLimits {
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            max_nar_bytes: config.nix.max_nar_bytes,
            max_bytes: config.storage.max_bytes,
            // Stay strictly below half the grace, including odd seconds.
            deadline: (Duration::from_secs(config.gc.orphan_grace) / 2)
                .saturating_sub(Duration::from_nanos(1)),
            #[cfg(test)]
            idle_timeout: UPLOAD_IDLE_TIMEOUT,
        }
    }

    fn idle_timeout(&self) -> Duration {
        #[cfg(test)]
        {
            self.idle_timeout
        }
        #[cfg(not(test))]
        {
            UPLOAD_IDLE_TIMEOUT
        }
    }
}

pub fn routes(store: Arc<Store>, tokens: Arc<Tokens>, registry: Arc<Registry>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { StatusCode::NOT_FOUND }).put(|| async { StatusCode::FORBIDDEN }),
        )
        .route("/nar/:file", get(nar).put(put_nar))
        .route(
            "/nix-cache-info",
            get(cache_info).put(|| async { StatusCode::NO_CONTENT }),
        )
        .route("/:name", get(narinfo).put(put_narinfo))
        .fallback(|method: Method| async move {
            if method == Method::PUT {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::NOT_FOUND
            }
        })
        .layer(axum::middleware::from_fn(check_encoding))
        .layer(axum::middleware::from_fn_with_state(
            registry,
            observe_upload,
        ))
        .layer(axum::middleware::from_fn_with_state(
            tokens,
            authorize_writes,
        ))
        .with_state(store)
}

#[derive(Clone)]
struct UploadCommitted {
    name: String,
    bytes: u64,
}

#[derive(Clone)]
pub struct UploadRefused(&'static str);

struct UploadError(StatusCode, &'static str);

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        (self.0, Extension(UploadRefused(self.1)), self.1).into_response()
    }
}

impl From<StatusCode> for UploadError {
    fn from(status: StatusCode) -> Self {
        Self(status, "storage-error")
    }
}

fn read_failure(error: &io::Error) -> UploadError {
    if error.kind() == io::ErrorKind::TimedOut {
        UploadError(
            StatusCode::REQUEST_TIMEOUT,
            if error.to_string() == "idle" {
                "idle"
            } else {
                "deadline"
            },
        )
    } else {
        UploadError(StatusCode::BAD_REQUEST, "invalid-body")
    }
}

async fn observe_upload(
    State(registry): State<Arc<Registry>>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::PUT {
        return next.run(request).await;
    }
    let kind = if request.uri().path().starts_with("/nar/") {
        "nar"
    } else if request.uri().path().ends_with(".narinfo") {
        "narinfo"
    } else {
        "other"
    };
    let mut response = next.run(request).await;
    if let Some(committed) = response.extensions().get::<UploadCommitted>() {
        registry
            .uploads(Module::Nix, UploadResult::Committed)
            .add(1);
        registry.upload_bytes(Module::Nix).add(committed.bytes);
        tracing::info!(
            event = "upload-committed",
            module = "nix",
            kind,
            name = committed.name,
            bytes = committed.bytes
        );
    } else if !response.status().is_success() {
        registry.uploads(Module::Nix, UploadResult::Refused).add(1);
        let reason = response
            .extensions()
            .get::<UploadRefused>()
            .map_or("bad-name", |refusal| refusal.0);
        if response.extensions().get::<UploadRefused>().is_none() {
            response = UploadError(response.status(), reason).into_response();
        }
        tracing::warn!(event = "upload-refused", module = "nix", kind, reason);
    }
    response
}

// Deny by default: every method except anonymous reads needs the write scope.
async fn authorize_writes(
    State(tokens): State<Arc<Tokens>>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }
    auth::require_scope(State(tokens), request, next).await
}

async fn check_encoding(request: Request, next: Next) -> Response {
    if request.method() == Method::PUT
        && request
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return UploadError(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported-encoding")
            .into_response();
    }
    next.run(request).await
}

async fn put_narinfo(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    body: Body,
) -> Result<Response, UploadError> {
    let hash = name
        .strip_suffix(".narinfo")
        .ok_or(UploadError(StatusCode::FORBIDDEN, "bad-name"))?
        .to_owned();
    let reader = UploadReader::new(body, &store.limits);
    let bytes = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        reader
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| read_failure(&error))?;
        if bytes.len() > 64 * 1024 {
            return Err(UploadError(StatusCode::PAYLOAD_TOO_LARGE, "too-large"));
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    let size = bytes.len() as u64;
    let invalid = |_| UploadError(StatusCode::BAD_REQUEST, "invalid-narinfo");
    let body = std::str::from_utf8(&bytes).map_err(invalid)?;
    let info: NarInfo = body
        .parse()
        .map_err(|_| UploadError(StatusCode::BAD_REQUEST, "invalid-narinfo"))?;
    if parse_store_path(&info.store_path)
        .map_err(|_| UploadError(StatusCode::BAD_REQUEST, "bad-name"))?
        .hash
        != hash
    {
        return Err(UploadError(StatusCode::BAD_REQUEST, "hash-mismatch"));
    }
    tokio::task::spawn_blocking(move || {
        let failure = |reason| UploadError(StatusCode::BAD_REQUEST, reason);
        let internal = |_| UploadError(StatusCode::INTERNAL_SERVER_ERROR, "storage-error");
        // Conflicting re-uploads are 409 even when their changed fields fail ingest checks.
        if let Some(content) = store.database.narinfo(&hash).map_err(internal)? {
            let mut existing: NarInfo = content
                .parse()
                .map_err(|_| UploadError(StatusCode::INTERNAL_SERVER_ERROR, "storage-error"))?;
            existing
                .merge_signatures(&info)
                .map_err(|_| UploadError(StatusCode::CONFLICT, "conflict"))?;
        }
        info.verify_signatures(&store.trusted_keys).map_err(|_| {
            failure(if info.signatures.is_empty() {
                "unsigned"
            } else {
                "untrusted-key"
            })
        })?;
        let file = info
            .url
            .strip_prefix("nar/")
            .ok_or_else(|| failure("bad-name"))?;
        file_hash(file).ok_or_else(|| failure("bad-name"))?;
        store
            .database
            .commit_narinfo(&info, "upload", now()?)
            .map_err(|error| match error {
                DatabaseError::NarinfoConflict => UploadError(StatusCode::CONFLICT, "conflict"),
                DatabaseError::MissingReference => failure("missing-reference"),
                DatabaseError::MissingNar => failure("missing-nar"),
                DatabaseError::FileHashMismatch => failure("hash-mismatch"),
                DatabaseError::FileSizeMismatch => failure("file-size-mismatch"),
                _ => internal(error),
            })?;
        Ok((
            Extension(UploadCommitted {
                name: info.store_path,
                bytes: size,
            }),
            StatusCode::OK,
        )
            .into_response())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}

async fn cache_info(State(store): State<Arc<Store>>) -> String {
    format!(
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {}\n",
        store.priority
    )
}

fn now() -> Result<i64, StatusCode> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| time.as_secs().try_into().ok())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn narinfo(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Response, StatusCode> {
    let hash = name
        .strip_suffix(".narinfo")
        .filter(|hash| hash.len() == 32 && decode_base32(hash).is_ok())
        .ok_or(StatusCode::NOT_FOUND)?
        .to_owned();
    tokio::task::spawn_blocking(move || {
        let content = store
            .database
            .narinfo(&hash)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;
        store
            .database
            .record_narinfo_access(&hash, now()?)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(([(header::CONTENT_TYPE, "text/x-nix-narinfo")], content).into_response())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}

async fn nar(
    State(store): State<Arc<Store>>,
    Path(file): Path<String>,
    method: Method,
) -> Result<Response, StatusCode> {
    file_hash(&file).ok_or(StatusCode::NOT_FOUND)?;
    let (file, size) = tokio::task::spawn_blocking(move || {
        let url = format!("nar/{file}");
        let blob = store
            .database
            .nar_blob(&url)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;
        // A registered blob missing on disk is a storage failure, not a cache miss.
        let file = store
            .blobs
            .open_blob(&blob.hash)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        store
            .database
            .record_nar_access(&url, now()?)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok::<_, StatusCode>((file, blob.size))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)))
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-nix-nar"),
            (header::CONTENT_LENGTH, &size.to_string()),
        ],
        body,
    )
        .into_response())
}

fn file_hash(file: &str) -> Option<[u8; 32]> {
    let stem = [".nar", ".nar.xz", ".nar.zst", ".nar.bz2"]
        .iter()
        .find_map(|suffix| file.strip_suffix(suffix))?;
    parse_sha256(&format!("sha256:{stem}")).ok()
}

// Poll only the next body frame when the synchronous blob writer needs bytes.
struct UploadReader {
    body: Body,
    chunk: Bytes,
    runtime: tokio::runtime::Handle,
    deadline: Instant,
    idle_deadline: Instant,
    idle_timeout: Duration,
}

impl UploadReader {
    fn new(body: Body, limits: &UploadLimits) -> Self {
        let start = Instant::now();
        Self {
            body,
            chunk: Bytes::new(),
            runtime: tokio::runtime::Handle::current(),
            deadline: start + limits.deadline,
            idle_deadline: start + limits.idle_timeout(),
            idle_timeout: limits.idle_timeout(),
        }
    }
}

impl Read for UploadReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
        }
        while self.chunk.is_empty() {
            if Instant::now() >= self.deadline.min(self.idle_deadline) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    if self.deadline <= self.idle_deadline {
                        "deadline"
                    } else {
                        "idle"
                    },
                ));
            }
            let frame = self
                .runtime
                .block_on(timeout_at(
                    self.deadline.min(self.idle_deadline),
                    std::future::poll_fn(|cx| Pin::new(&mut self.body).poll_frame(cx)),
                ))
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        if self.deadline <= self.idle_deadline {
                            "deadline"
                        } else {
                            "idle"
                        },
                    )
                })?;
            let Some(frame) = frame else { return Ok(0) };
            let frame =
                frame.map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if let Ok(data) = frame.into_data() {
                if !data.is_empty() {
                    self.idle_deadline = Instant::now() + self.idle_timeout;
                }
                self.chunk = data;
            }
        }
        let length = buffer.len().min(self.chunk.len());
        buffer[..length].copy_from_slice(&self.chunk.split_to(length));
        Ok(length)
    }
}

async fn put_nar(
    State(store): State<Arc<Store>>,
    Path(file): Path<String>,
    headers: header::HeaderMap,
    body: Body,
) -> Result<Response, UploadError> {
    let hash = file_hash(&file).ok_or(UploadError(StatusCode::FORBIDDEN, "bad-name"))?;
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    let reader = UploadReader::new(body, &store.limits);
    let permit = timeout_at(
        reader.deadline.min(reader.idle_deadline),
        store.uploads.permits.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        UploadError(
            StatusCode::REQUEST_TIMEOUT,
            if reader.deadline <= reader.idle_deadline {
                "deadline"
            } else {
                "idle"
            },
        )
    })?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let max_nar_bytes = store.limits.max_nar_bytes;
        let (remaining, _reservation) = match store.limits.max_bytes {
            Some(cap) => {
                let mut reserved = store
                    .uploads
                    .reserved
                    .lock()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                let used = store
                    .database
                    .stored_bytes()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .saturating_add(*reserved);
                let existing = store
                    .database
                    .blob_size(&hash)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                if used >= cap && existing.is_none() {
                    return Err(UploadError(
                        StatusCode::INSUFFICIENT_STORAGE,
                        "storage-full",
                    ));
                }
                // Re-uploading an immutable hash consumes no additional stored bytes.
                let remaining = cap
                    .saturating_sub(used)
                    .saturating_add(existing.unwrap_or(0));
                if existing.is_none()
                    && remaining < max_nar_bytes
                    && length.is_some_and(|length| length > remaining)
                {
                    return Err(UploadError(
                        StatusCode::INSUFFICIENT_STORAGE,
                        "storage-full",
                    ));
                }
                let amount = if existing.is_some() {
                    0
                } else {
                    remaining.min(max_nar_bytes).min(length.unwrap_or(u64::MAX))
                };
                *reserved += amount;
                (
                    remaining,
                    Some(Reservation(&store.uploads.reserved, amount)),
                )
            }
            None => (u64::MAX, None),
        };
        let cap = max_nar_bytes.min(remaining);
        let blob = store
            .blobs
            .write(reader, cap, Some(&hash))
            .map_err(|error| match error.kind() {
                io::ErrorKind::FileTooLarge if remaining < max_nar_bytes => {
                    UploadError(StatusCode::INSUFFICIENT_STORAGE, "storage-full")
                }
                io::ErrorKind::FileTooLarge => {
                    UploadError(StatusCode::PAYLOAD_TOO_LARGE, "too-large")
                }
                io::ErrorKind::TimedOut => read_failure(&error),
                io::ErrorKind::InvalidData => UploadError(StatusCode::BAD_REQUEST, "hash-mismatch"),
                io::ErrorKind::InvalidInput => UploadError(StatusCode::BAD_REQUEST, "invalid-body"),
                _ => UploadError(StatusCode::INTERNAL_SERVER_ERROR, "storage-error"),
            })?;
        store
            .database
            .register_nar(&format!("nar/{file}"), &blob, now()?)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok((
            Extension(UploadCommitted {
                name: file,
                bytes: blob.size,
            }),
            StatusCode::NO_CONTENT,
        )
            .into_response())
    })
    .await
    .map_err(|_| UploadError(StatusCode::INTERNAL_SERVER_ERROR, "storage-error"))?
}

#[cfg(test)]
pub(crate) mod tests;
