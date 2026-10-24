//! Anonymous Nix cache reads (ADR 0006).
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use oroshi_nix::{decode_base32, parse_sha256};
use oroshi_store::{BlobStore, Database};
use tokio_util::io::ReaderStream;

pub struct Store {
    pub blobs: BlobStore,
    pub database: Database,
    pub priority: u32,
}

pub fn routes(store: Arc<Store>) -> Router {
    Router::new()
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
    let stem = [".nar", ".nar.xz", ".nar.zst", ".nar.bz2"]
        .iter()
        .find_map(|suffix| file.strip_suffix(suffix))
        .ok_or(StatusCode::NOT_FOUND)?;
    parse_sha256(&format!("sha256:{stem}")).map_err(|_| StatusCode::NOT_FOUND)?;
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

#[cfg(test)]
pub(crate) mod tests;
