# ADR 0017 — Nix narinfo ingest rules

Status: Accepted — 2026-10-24

## Context

0006 defines the Nix binary cache protocol and 0007 the signature trust. Implementing uploads (O1) and its security review had to fix what the cache accepts, how it refuses, and how signatures are merged. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands. Each item describes what the code does and is covered by its tests.

## Decision

- **Amends 0006 — narinfo fields.** A narinfo carrying a field other than `StorePath`, `URL`, `Compression`, `FileHash`, `FileSize`, `NarHash`, `NarSize`, `References`, `Deriver`, `CA` and `Sig` is refused (`invalid-narinfo`): an explicit failure is preferred over silently dropping metadata the uploader expects to be served. A duplicated single-valued field and an empty value (other than `References`) are refused the same way.
- **Amends 0006 — `Content-Encoding`.** The 415 refusal of a `Content-Encoding` other than `identity` applies to every `PUT` under `/nix/`, NAR uploads included, not only to narinfos.
- **Amends 0006 and 0011 — closed refusal reasons.** A refused upload carries one reason from a closed set, logged in `upload-refused{module,kind,reason}` and returned as the plain-text response body:

  | Reason | Status | Meaning |
  | --- | --- | --- |
  | `bad-name` | 400, 403 | malformed NAR or narinfo name, a `URL` outside `nar/` or with an invalid file hash, any other `PUT` (403) |
  | `unsupported-encoding` | 415 | `Content-Encoding` other than `identity` |
  | `invalid-body` | 400 | the request body could not be read |
  | `idle` | 408 | no body bytes for the idle timeout (0010, 0018) |
  | `deadline` | 408 | the upload deadline expired (0005, 0018) |
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
- **0006 — unknown-route `PUT` (no change).** A `PUT` to any other path stays 403 `bad-name`.
- **Amends 0006 — narinfo body cap.** A narinfo `PUT` body is capped at 64 KiB; a larger body is refused with 413 (`too-large`).
- **Supersedes 0006 — signature merge (0007).** When a narinfo `PUT` merges signatures into an existing narinfo, only the incoming signatures that verify against `nix.trusted_keys` are added; the signatures already stored are kept. A first upload still stores every well-formed signature it carries (bounded by the 64 KiB body cap). A merged narinfo larger than 64 KiB is refused with 413 `too-large` and the stored narinfo is unchanged. Without this, repeated uploads with extra signatures could grow a stored narinfo without bound.
- **Amends 0006 — unsigned fields.** `Compression` and `CA` are not covered by the signature, and a narinfo is immutable after its first upload, so they are validated before the first commit; a failure is 400 `invalid-narinfo`:
  - `Compression` is one of `none`, `xz`, `zstd`, `bzip2` and matches the `URL` suffix (`.nar`, `.nar.xz`, `.nar.zst`, `.nar.bz2`). An absent `Compression` means `bzip2`, as Nix reads it.
  - `CA` is absent or a content address in the form Nix writes it: `text:sha256:<hash>` or `fixed:[r:|git:]<algo>:<hash>`, with `<algo>` one of `md5`, `sha1`, `sha256`, `sha512` and `<hash>` in Nix base32 of exactly that algorithm's length.
  A re-upload that differs in these fields from the stored narinfo is still 409 `conflict`.
- **0006 — first upload wins (no change).** A narinfo is not replaced automatically, even when its stored NAR is wrong: the cache cannot tell a correction from an overwrite. An operator command to drop a narinfo ships with O2's garbage-collection tooling.

## Consequences

- A future Nix release that adds a narinfo field fails uploads with `invalid-narinfo` until oroshi learns the field; the refusal is visible in the logs and to the uploader.
- Operators and tests can match refusals on a stable reason string; adding a reason is an ADR change.
- A builder signing with a key the cache does not trust gets a successful response, but its signature is not served; trust a key in `nix.trusted_keys` to have its signatures merged.
- A narinfo with a wrong `Compression`, a missing `Compression` on a non-bzip2 NAR, or a malformed `CA` is refused instead of being stored permanently and breaking clients.
- A path poisoned by a first upload stays poisoned until the O2 operator command exists; the database can be fixed by hand meanwhile.

## Rejected alternatives

- Dropping unknown narinfo fields: silent metadata loss, and the served narinfo would differ from the uploaded one.
- Free-form refusal messages: tests and operators could not rely on them.
- Replacing a narinfo when a newer upload carries a trusted signature: the unsigned fields could then be changed by any write-token holder at any time.
- Capping the number of signatures instead of the merged size: a size cap also bounds long key names.
