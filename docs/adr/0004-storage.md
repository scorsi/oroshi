# ADR 0004 — Storage: content-addressed blobs, SQLite metadata

Status: Accepted — 2026-10-24

## Context

Every module stores immutable files (NARs, module zips, `.crate`, tarballs) plus small metadata (narinfo, indexes, versions, access times). Files are large and deduplicate well (the same NAR uploaded twice, the same crate fetched by two hosts). GC needs reference tracking and atomic state changes (0005). One serving instance owns the state directory (0014).

## Decision

- **Blobs**: files named by the hex SHA-256 of their bytes, as stored (compressed if the client sent them compressed), under `stateDir/blobs/sha256/<first 2 hex>/<64 hex>`. Immutable once renamed into place; never rewritten.
- **Write path**: stream the request body to `stateDir/tmp/<random>`, hashing while writing, with a size cap; on end of body check the expected hash when the protocol gives one, `fsync` the file, `rename` into `blobs/` (a no-op if the blob already exists), `fsync` the directory, then commit the metadata row in SQLite. A blob on disk without a row is an **orphan**; only GC removes it (0005). A failed or aborted upload leaves at most a temp file, removed at startup and by GC.
- **Metadata**: SQLite in WAL mode, `stateDir/db/oroshi.sqlite`, a single writer connection and read connections, forward-only migrations embedded and applied at startup (0013). Per module its own tables (Nix: `narinfo`, `nar`, `reference`); shared tables: `blob(sha256, size, created_at)`. Access time is a column updated in batches (at most once per entry per hour), never `atime`.
- **Primary vs cached data**: content oroshi is the source of truth for (packages published to `cargo`, `hex`, `npm`, `go`) is **primary**; Nix paths and pull-through copies are **cache** (rebuildable or refetchable). The module that first stores primary data (O4) adds a consistent dump command (SQLite backup API + the referenced blobs) and its backup ADR; cache data is never backed up.
- **Layout** under `stateDir` (default `/var/lib/oroshi`): `db/`, `blobs/`, `tmp/` (same filesystem as `blobs/`, so `rename` is atomic). The entire state directory must be durable on a local filesystem supporting atomic rename and `fsync`; network and union filesystems are unsupported (0014). The NixOS module exposes `stateDir` and the sub-paths requiring persistence.
- **Disk cap**: `storage.max_bytes` (optional). An upload that would exceed it is refused with `507 Insufficient Storage` and logged; GC (0005) brings usage back under the cap.

## Consequences

- Deduplication across modules is free; deleting a blob requires knowing nobody references it (0005).
- A blob's name proves its content: a scrub (`oroshi gc --verify`, later) can detect bit rot.
- Raw file copies of the SQLite database are never a backup.

## Rejected alternatives

- Flat metadata files (narinfo files on disk as the Nix cache layout): simple for Nix alone, but no atomic multi-row change, no access tracking without `atime`, and every module would reinvent an index.
- Object storage (S3/MinIO) as the backend: another service to run for a single-host store.
- Storing blobs inside SQLite: multi-GB NARs.
