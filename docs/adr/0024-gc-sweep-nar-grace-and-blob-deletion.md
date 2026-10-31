# ADR 0024 — GC sweep: NAR row grace and blob deletion order

Status: Accepted — 2026-10-31

## Context

0005 sweeps blobs after the row deletes: a blob goes when no row references it and its file is older than `gc.orphan_grace`, and correctness rests on "row before blob is deleted, blob before row is created". Two gaps appeared while splitting O2. A Nix NAR is registered (`nar` row) before the narinfo that uses it (0006), so a `nar` row no narinfo uses yet is normal during `nix copy`, and the blob's `created_at` (0004) is not its age: a deduplicated upload reuses an old blob. And an upload that reuses an existing blob file does no `rename`, so a sweep can unlink that file between the upload's check and its commit. An accepted ADR is not modified (README): each item names the ADR point it amends or supersedes; everything else in that ADR stands.

## Decision

- **Amends 0004 and 0005 — `nar.registered_at`.** The `nar` table gets a `registered_at` time, set on insert and refreshed on every NAR upload of that row (also a deduplicated one). The sweep deletes a `nar` row that no narinfo uses only when its `registered_at` is older than `gc.orphan_grace`; the blob's `created_at` is not used for it.
- **Supersedes 0005 — blob deletion order.** The sweep deletes a blob's file inside the same SQLite write transaction that deletes its `blob` row: re-check that no `nar` row uses the blob, delete the row, `unlink` the file, commit; a file already gone is not an error. If the transaction fails after the unlink, the unused row stays without its file: the next sweep deletes it, or the next upload of the same content publishes the file again (0004). 0005's "row before blob is deleted" ordering is replaced by this one.
- **Amends 0005 — registering a NAR.** `register_nar` checks, inside its write transaction, that the blob file exists; if it does not, the upload fails cleanly (500 `storage-error`, the client retries) instead of registering a missing blob. Since the sweep unlinks under the single SQLite writer, a registration either commits before the sweep's transaction (the sweep then sees the `nar` row and keeps the blob) or checks the file after it is gone.
- Orphan files under `blobs/` without a row and `tmp/` files keep 0005's rule: removed when older than the grace.

## Consequences

- An in-flight `nix copy` keeps its NARs for `gc.orphan_grace` after the last NAR upload, whatever the age of the deduplicated blob.
- No `blob` row can point at a file the sweep deleted, except an unused one after a failure between unlink and commit, which the next sweep or identical upload repairs.
- Tests: a deduplicated upload racing the sweep never leaves a row without its file; a NAR uploaded within the grace survives a sweep even when its blob is old.

## Rejected alternatives

- Grace from `blob.created_at`: deletes the `nar` row of a deduplicated upload in progress.
- Unlinking after the commit (0005's order): a deduplicated registration committed between the row delete and the unlink would reference a deleted file.
