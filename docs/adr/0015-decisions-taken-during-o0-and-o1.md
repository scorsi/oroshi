# ADR 0015 — Decisions taken during O0 and O1

Status: Accepted — 2026-10-24

## Context

O0 (skeleton) and O1 (Nix binary cache and NixOS module) are the first milestones with code. An accepted ADR is not modified (README): the points the accepted ADRs left open or implicit, and that implementation had to fix, are recorded here. Each item names the ADR it amends; everything else in that ADR stands. Each item describes what the O0/O1 code does and is covered by its tests.

## Decision

- **Amends 0006 — narinfo fields.** A narinfo carrying a field other than `StorePath`, `URL`, `Compression`, `FileHash`, `FileSize`, `NarHash`, `NarSize`, `References`, `Deriver`, `CA` and `Sig` is refused (`invalid-narinfo`): an explicit failure is preferred over silently dropping metadata the uploader expects to be served. A duplicated single-valued field and an empty value (other than `References`) are refused the same way.
- **Amends 0006 — `Content-Encoding`.** The 415 refusal of a `Content-Encoding` other than `identity` applies to every `PUT` under `/nix/`, NAR uploads included, not only to narinfos.
- **Amends 0006 and 0011 — closed refusal reasons.** A refused upload carries one reason from a closed set, logged in `upload-refused{module,kind,reason}` and returned as the plain-text response body:

  | Reason | Status | Meaning |
  | --- | --- | --- |
  | `bad-name` | 400, 403 | malformed NAR or narinfo name, a `URL` outside `nar/` or with an invalid file hash, any other `PUT` (403) |
  | `unsupported-encoding` | 415 | `Content-Encoding` other than `identity` |
  | `invalid-body` | 400 | the request body could not be read |
  | `idle` | 408 | no body bytes for the idle timeout (0010) |
  | `deadline` | 408 | the upload deadline expired (0005) |
  | `too-large` | 413 | NAR above `nix.max_nar_bytes`, or narinfo above 64 KiB |
  | `storage-full` | 507 | the upload would exceed `storage.max_bytes` |
  | `hash-mismatch` | 400 | the NAR body does not match its name, or `StorePath` does not match the narinfo name |
  | `invalid-narinfo` | 400 | the narinfo does not parse (including unknown fields) |
  | `unsigned` | 400 | no signature |
  | `untrusted-key` | 400 | no valid signature by a trusted key (0007) |
  | `missing-nar` | 400 | `URL` names no uploaded NAR |
  | `file-size-mismatch` | 400 | `FileSize` differs from the stored NAR |
  | `missing-reference` | 400 | a `References` path is absent (0005) |
  | `conflict` | 409 | a narinfo already present differs apart from `Sig` |
  | `storage-error` | 500 | internal storage or database failure |

  Authentication refusals stay `auth-refused` (0008) and are not upload refusals.
- **Amends 0006 — narinfo body cap.** A narinfo `PUT` body is capped at 64 KiB; a larger body is refused with 413 (`too-large`).
- **Amends 0010 — upload timeouts.** The idle timeout stays fixed at 60 s (not configurable). It applies to upload request bodies: an upload that receives no body bytes for 60 s is refused (`idle`); oroshi sets no idle timeout of its own on connections or downloads. The upload deadline is derived from the orphan grace: strictly below half of `gc.orphan_grace`, so an upload in progress always finishes or fails before its temporary data could be treated as an orphan (0005).
- **Amends 0009 — `config check`.** `oroshi config check` accepts the path as `--config <path>` or as a positional argument. A command-line usage error logs `fatal` and exits with status 2; an invalid configuration logs `config-invalid` and exits with status 1. The NixOS module also exposes the generated file at `/etc/oroshi/oroshi.toml`.
- **Amends 0014 — single instance.** `oroshi serve` takes a non-blocking exclusive lock on `<storage.dir>/oroshi.lock` before opening the database and holds it for its lifetime; a second serving process on the same state directory fails at startup (`fatal`).
- **Amends 0011 — events.** `migration-applied{version}` is logged once per applied migration version. The `request` event's `status` is the integer HTTP status, the same convention as the other products of the ecosystem.
- **Amends 0002 — `sha2`.** `sha2` is a direct dependency of the app (token hashing in `oroshi token hash` and token verification, 0008) and of `oroshi-store` (blob identity). `oroshi-nix` does not depend on it: narinfo hashes are parsed and encoded, never computed there.
- **0008 — read access (no decision).** The open point on internal read visibility stays open. The owner's intended direction is per-module read scopes; it will be decided in a future ADR.
- **Tests of O1 (0012).** The `nix copy` upload contract — NARs and narinfos uploaded with `PUT`, references before the paths that refer to them — is proven by `checks.<system>.nix-cache`, which also covers refusals, mixed-signer closures and restart. The TLS boundary (0010) is proven by `checks.<system>.tls-boundary`, with nginx as the example reverse proxy.

## Consequences

- A future Nix release that adds a narinfo field fails uploads with `invalid-narinfo` until oroshi learns the field; the refusal is visible in the logs and to the uploader.
- Operators and tests can match refusals on a stable reason string; adding a reason is an ADR change.
- Changing `gc.orphan_grace` changes the upload deadline.
- Running two `oroshi serve` on one state directory by mistake fails fast instead of corrupting state.

## Rejected alternatives

- Dropping unknown narinfo fields: silent metadata loss, and the served narinfo would differ from the uploaded one.
- Free-form refusal messages: tests and operators could not rely on them.
- A configurable idle timeout: no need found; the proxy limits are set against the fixed value (0010).
