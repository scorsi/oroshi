//! Nix cache HTTP routes (ADR 0006).
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use oroshi_nix::{NarInfo, PublicKey, decode_base32, parse_sha256, parse_store_path};
use oroshi_store::{BlobStore, Database, DatabaseError};
use tokio_util::io::ReaderStream;

use crate::auth::{self, Tokens};

pub struct Store {
    pub blobs: BlobStore,
    pub database: Database,
    pub priority: u32,
    pub trusted_keys: Vec<PublicKey>,
}

pub fn routes(store: Arc<Store>, tokens: Arc<Tokens>) -> Router {
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
        .layer(axum::middleware::from_fn_with_state(tokens, authorize_put))
        .with_state(store)
}

async fn authorize_put(
    State(tokens): State<Arc<Tokens>>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::PUT {
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
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    next.run(request).await
}

async fn put_narinfo(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    body: Body,
) -> Result<StatusCode, (StatusCode, String)> {
    let hash = name
        .strip_suffix(".narinfo")
        .ok_or((StatusCode::FORBIDDEN, "unsupported PUT path".into()))?;
    let bytes = axum::body::to_bytes(body, usize::MAX).await.map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "failed to read narinfo body".into(),
        )
    })?;
    let body = std::str::from_utf8(&bytes)
        .map_err(|_| (StatusCode::BAD_REQUEST, "narinfo is not UTF-8".into()))?;
    let info: NarInfo = body
        .parse()
        .map_err(|error: oroshi_nix::ParseError| (StatusCode::BAD_REQUEST, error.to_string()))?;
    if parse_store_path(&info.store_path)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?
        .hash
        != hash
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "StorePath hash does not match URL hash".into(),
        ));
    }
    let hash = hash.to_owned();
    tokio::task::spawn_blocking(move || {
        let failure = |reason: &str| (StatusCode::BAD_REQUEST, reason.to_owned());
        let internal = |_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage failure".to_owned(),
            )
        };
        // Conflicting re-uploads are 409 even when their changed fields fail ingest checks.
        if let Some(content) = store.database.narinfo(&hash).map_err(internal)? {
            let mut existing: NarInfo = content.parse().map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid stored narinfo".to_owned(),
                )
            })?;
            existing
                .merge_signatures(&info)
                .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
        }
        info.verify_signatures(&store.trusted_keys)
            .map_err(|error| failure(&error.to_string()))?;
        let file = info
            .url
            .strip_prefix("nar/")
            .ok_or_else(|| failure("invalid NAR URL"))?;
        file_hash(file).ok_or_else(|| failure("invalid NAR URL"))?;
        let blob = store
            .database
            .nar_blob(&info.url)
            .map_err(internal)?
            .ok_or_else(|| failure("NAR is not uploaded"))?;
        if info.file_hash != Some(blob.hash) || info.file_size != Some(blob.size) {
            return Err(failure("FileHash or FileSize does not match uploaded NAR"));
        }
        store
            .database
            .commit_narinfo(
                &info,
                "upload",
                now().map_err(|code| (code, "clock failure".into()))?,
            )
            .map_err(|error| match error {
                DatabaseError::NarinfoConflict => (StatusCode::CONFLICT, error.to_string()),
                DatabaseError::MissingReference => failure("missing narinfo reference"),
                _ => internal(error),
            })?;
        Ok(StatusCode::OK)
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload worker failed".into(),
        )
    })?
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
}

impl Read for UploadReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.chunk.is_empty() {
            let frame = self.runtime.block_on(std::future::poll_fn(|cx| {
                Pin::new(&mut self.body).poll_frame(cx)
            }));
            let Some(frame) = frame else { return Ok(0) };
            let frame =
                frame.map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if let Ok(data) = frame.into_data() {
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
    body: Body,
) -> Result<StatusCode, StatusCode> {
    let hash = file_hash(&file).ok_or(StatusCode::FORBIDDEN)?;
    let reader = UploadReader {
        body,
        chunk: Bytes::new(),
        runtime: tokio::runtime::Handle::current(),
    };
    tokio::task::spawn_blocking(move || {
        let blob = store
            .blobs
            .write(reader, u64::MAX, Some(&hash))
            .map_err(|error| match error.kind() {
                io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            })?;
        store
            .database
            .register_nar(&format!("nar/{file}"), &blob, now()?)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}

#[cfg(test)]
pub(crate) mod tests;
