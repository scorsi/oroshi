# ADR 0005 — Retention and garbage collection

Status: Accepted — 2026-10-24

## Context

The cache grows with every CI run. Some content must never disappear (published packages: lockfiles depend on them; a closure someone pinned), most can expire (old CI outputs, pull-through copies). GC runs while uploads and downloads continue. A Nix closure must stay complete: a narinfo served whose references are gone breaks substitution.

## Decision

- **Retention classes**:
  - **Pinned, never expires**: published versions of primary packages (0004; yank hides, never deletes); Nix paths reachable from a **named root**.
  - **Expires after `retention.days` without access** (default 30): every other Nix path and every pull-through copy.
- **Nix closure invariant**: a narinfo is accepted only if every path in its `References` is already present or is the path itself (0006), and a narinfo is deleted only if no other remaining narinfo references it. The candidate's self-edge is excluded from the deletion check, so self-reference alone does not prevent expiry. Liveness is transitive: a path is live if it is under a root, accessed within `retention.days`, or referenced by a live path.
- **Named roots** (Nix, O2): `PUT /nix/_oroshi/roots/<name>` with a list of store paths (each must be present), `DELETE` to drop it; scope `nix:pin` (0008). A root replaces its previous content. Any client with that scope may manage roots; root selection is configured in the deployment repository.
- **Size cap**: if `storage.max_bytes` is set and usage is above `storage.gc_target_ratio` (default 0.9) of it after expiry, GC also removes the least recently accessed non-live-by-root paths, closure-aware, until under the target.
- **GC** is `oroshi gc`, run by a systemd timer (default daily) against the same database; it does not stop the server.
  1. **Mark**: in one read transaction, compute the set of entries to delete.
  2. **Delete rows**: per entry, in a write transaction, re-check that it is still unrooted, not accessed since mark, and unreferenced by any other narinfo (exclude its self-edge); delete the row and its outgoing reference edges. SQLite's single writer serialises this with uploads: an upload that adds a referrer either commits first (the re-check sees it) or commits after (its reference check fails, the client gets 400 and retries; `nix copy` re-uploads the missing path).
  3. **Sweep blobs**: delete a blob only if no row references it **and** its file is older than `gc.orphan_grace` (default 1 h). An upload writes its blob before its row, so an in-flight upload's blob is younger than the grace; the server enforces an upload deadline shorter than the grace (`gc.orphan_grace` / 2).
  4. Remove temp files older than the grace.
- A blob deleted while being served keeps streaming (an open file survives `unlink` on Linux).
- GC writes one `gc-run` log line and updates the GC metrics (0011).

## Consequences

- No locking between GC and the server beyond SQLite's; correctness rests on "row before blob is deleted, blob before row is created" and the grace period, each with a test (concurrent upload during GC, download during sweep).
- Pull-through content can disappear upstream after it expired here; pin it (O3+) if that matters.

## Rejected alternatives

- Stop-the-world GC: uploads from CI fail during the run.
- Reference counting on blobs: drifts on crashes; mark-and-sweep from rows is self-healing.
- Expiry by upload time only: deletes paths every host still substitutes daily.
