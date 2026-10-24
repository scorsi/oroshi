# ADR 0006 — Nix binary cache protocol

Status: Accepted — 2026-10-24

## Context

Nix's HTTP binary cache is a set of static-looking files: `nix-cache-info`, `<hash>.narinfo`, NAR files. Nix can also **write** to an `http(s)://` store: `nix copy --to` uploads each file with HTTP `PUT`, credentials from the client's `netrc-file`. Hosts read through `substituters` and verify signatures against `trusted-public-keys` (0007).

## Decision

- **Read** (prefix `/nix/`, no token, internal network only through the reverse proxy's internal listener, 0008 and 0010):
  - `GET /nix/nix-cache-info`: `StoreDir: /nix/store`, `WantMassQuery: 1`, `Priority: <nix.priority>` (default 30, ahead of cache.nixos.org's 40).
  - `GET|HEAD /nix/<hash>.narinfo` (`text/x-nix-narinfo`), 404 when absent; serving updates the access time (0004).
  - `GET|HEAD /nix/nar/<file>` (`application/x-nix-nar`), streamed from the blob.
  - Anything else under `/nix/` (`.ls`, `log/`, `realisations/`, `debuginfo/`): 404.
- **Write** (`PUT`, token with scope `nix:write`):
  - `PUT /nix/nar/<filehash>.nar[.xz|.zst|.bz2]`: stored as a blob (0004). The base32 SHA-256 in the name must match the body, else 400. Compression is whatever the client chose; the recommended uploader URL is `https://oroshi.example.org/nix?compression=zstd`. oroshi never recompresses and never decompresses.
  - `PUT /nix/<hash>.narinfo`: accepted only if it parses; `StorePath` is a valid `/nix/store` path whose hash equals `<hash>`; `URL` names a NAR already uploaded whose size and SHA-256 equal `FileSize` and `FileHash`; every `References` path is present (closure invariant, 0005) or is the path itself; it carries at least one valid signature by a trusted key (0007). `Content-Encoding` other than identity: 415.
  - A narinfo for a path already present: identical apart from `Sig` → signatures merged, 200 (this is what `nix store sign --store` does); any other difference → 409. Narinfos are otherwise immutable.
  - `PUT /nix/nix-cache-info` (Nix writes it on a store it believes empty): 204, ignored. Any other `PUT`: 403.
  - `NarHash` is not verified on ingest (it would mean decompressing every NAR); every client verifies it on download, and the signature binds it.
- **GC and self-references**: self-references are allowed at ingest; the deletion check excludes the candidate's self-edge but still refuses deletion while any other narinfo references it (0005).
- **Upload limits**: `nix.max_nar_bytes` (default 16 GiB) and the upload deadline of 0005.
- **One cache**: a single namespace for all systems and all uploaders.
- **No upstream relay to cache.nixos.org in v1.** A miss returns 404; clients may list oroshi first and cache.nixos.org second, each with its trusted public key. Narinfo rows include a `source` column from the first migration to allow a future relay without changing the initial schema.

## Consequences

- No oroshi-specific client: `nix copy`, `nix-store --realise`, `nix build` and the `substituters` setting are the interface, proven by the nixosTests (0012).
- A path built and pushed in the wrong order is refused, not stored half-usable; `nix copy` uploads in topological order.

## Rejected alternatives

- Relay on miss in v1: adds an upstream HTTP client, cache traffic and storage use; clients can contact upstream substituters directly.
- S3-compatible API (`s3://` stores): SigV4, multipart, bucket semantics — a large surface to emulate when Nix's plain HTTP `PUT` does the job.
- `ssh-ng://` / `nix-serve`-style serving from a real Nix store: needs a Nix daemon and a store on the cache host, with Nix's own GC instead of retention by access (0005).
- Verifying `NarHash` on ingest: decompression crates and CPU for a check every reader already does.
