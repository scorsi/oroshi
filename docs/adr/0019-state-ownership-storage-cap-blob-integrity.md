# ADR 0019 — State ownership, storage cap and blob integrity

Status: Accepted — 2026-10-24

## Context

0004 defines the storage and 0014 the deployment requirements. Implementing O1 and its security review had to fix who owns the state directory, what `storage.max_bytes` counts, and what happens to a corrupted blob. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0014 — single instance.** `oroshi serve` takes a non-blocking exclusive lock on `<storage.dir>/oroshi.lock` before opening the database and holds it for its lifetime; a second serving process on the same state directory fails at startup (`fatal`).
- **Amends 0004 — `storage.max_bytes`.** `max_bytes` is a logical cap on registered blob bytes: it does not count filesystem overhead, inodes, the SQLite database, temporary files or orphan blobs. Bounding the filesystem itself (a quota, headroom, free-space alerts) is the deployment's job (0014).
- **Clarifies 0004 — corrupted blobs.** 0004 says blobs are never rewritten. A blob whose bytes no longer match its hash name (bit rot, a manual edit) is replaced when the same content is uploaded again, after the upload's own bytes were verified against the name. A sound blob is still never rewritten.

## Consequences

- Running two `oroshi serve` on one state directory by mistake fails fast instead of corrupting state.
- The filesystem holding the state directory needs room beyond `storage.max_bytes`.
