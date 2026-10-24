//! Nix cache HTTP routes (ADR 0006).
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use oroshi_nix::{decode_base32, parse_sha256};
use oroshi_store::{BlobStore, Database};
use tokio_util::io::ReaderStream;

use crate::auth::{self, Tokens};

pub struct Store {
    pub blobs: BlobStore,
    pub database: Database,
    pub priority: u32,
}

pub fn routes(store: Arc<Store>, tokens: Arc<Tokens>) -> Router {
    Router::new()
        .route(
            "/nar/:file",
            axum::routing::put(put_nar).route_layer(axum::middleware::from_fn_with_state(
                tokens,
                auth::require_scope,
            )),
        )
        .route("/nix-cache-info", get(cache_info))
        .route("/:name", get(narinfo))
        .route("/nar/:file", get(nar))
        .with_state(store)
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
