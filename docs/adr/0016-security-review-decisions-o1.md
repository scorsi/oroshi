# ADR 0016 — Decisions from the O1 security review

Status: Accepted — 2026-10-24

## Context

A defensive security review of O0 and O1 found points where the accepted ADRs leave a write-token holder, a slow client or the operator more room than intended, and two places where the code already differs from an ADR's wording. An accepted ADR is not modified (README): each item below names the ADR point it supersedes or clarifies; everything else in that ADR stands.

## Decision

- **Supersedes 0006 and 0015 — signature merge.** When a narinfo `PUT` merges signatures into an existing narinfo, only the incoming signatures that verify against `nix.trusted_keys` are added; the signatures already stored are kept. A first upload still stores every well-formed signature it carries (bounded by the 64 KiB body cap). A merged narinfo larger than 64 KiB is refused with 413 `too-large` and the stored narinfo is unchanged. Without this, repeated uploads with extra signatures could grow a stored narinfo without bound.
- **Amends 0015 — unsigned fields.** `Compression` and `CA` are not covered by the signature, and a narinfo is immutable after its first upload, so they are validated before the first commit; a failure is 400 `invalid-narinfo`:
  - `Compression` is one of `none`, `xz`, `zstd`, `bzip2` and matches the `URL` suffix (`.nar`, `.nar.xz`, `.nar.zst`, `.nar.bz2`). An absent `Compression` means `bzip2`, as Nix reads it.
  - `CA` is absent or a content address in the form Nix writes it: `text:sha256:<hash>` or `fixed:[r:|git:]<algo>:<hash>`, with `<algo>` one of `md5`, `sha1`, `sha256`, `sha512` and `<hash>` in Nix base32 of exactly that algorithm's length.
  A re-upload that differs in these fields from the stored narinfo is still 409 `conflict`.
- **0006 — first upload wins (no change).** A narinfo is not replaced automatically, even when its stored NAR is wrong: the cache cannot tell a correction from an overwrite. An operator command to drop a narinfo ships with O2's garbage-collection tooling.
- **Supersedes 0004 — `storage.max_bytes` in the NixOS module.** Until garbage collection ships (O2), the module requires `storage.maxBytes` (an evaluation assertion with an explanatory message); the binary's configuration keeps it optional. `max_bytes` is a logical cap on registered blob bytes: it does not count filesystem overhead, inodes, the SQLite database, temporary files or orphan blobs. Bounding the filesystem itself (a quota, headroom, free-space alerts) is the deployment's job (0014).
- **Amends 0011 — `auth-refused` level.** A refusal of a request carrying no `Authorization` header is logged at `info`: clients routinely send a first request without credentials and wait for the challenge. A refusal with a credential stays at `warn`. Recording the client address in these events is deferred: it needs a decision on which proxy-supplied header to trust.
- **Supersedes 0010 and 0015 — idle timeout on downloads.** The 60 s idle timeout also applies to responses: a response write that makes no progress for 60 s (the client stopped reading) closes the connection, releasing its open blob. 0015's "upload bodies only, no idle timeout on downloads" no longer holds; the upload body timeout is unchanged.
- **Clarifies 0015 and 0005 — upload deadline.** The upload deadline covers the whole upload until the database commit: body, verification, blob publication and commit. It is checked again before the blob is published and before the metadata commit; past it, the upload is refused with 408 `deadline`. A blob published but not committed is an orphan, as with any metadata failure (0004).
- **Clarifies 0012 — network controls in the module.** 0012 leaves network controls to the deployment. The NixOS module nevertheless restricts the unit to loopback peers (`IPAddressAllow=localhost`, `IPAddressDeny=any`) and asserts that `listen` is a loopback address: clients reach oroshi only through the TLS proxy (0010), so this costs no supported setup. The deployment still owns the proxy and the firewall.
- **Clarifies 0004 — corrupted blobs.** 0004 says blobs are never rewritten. A blob whose bytes no longer match its hash name (bit rot, a manual edit) is replaced when the same content is uploaded again, after the upload's own bytes were verified against the name. A sound blob is still never rewritten.
- **0015 — unknown-route `PUT` (no change).** A `PUT` to any other path stays 403 `bad-name`.

## Consequences

- A builder signing with a key the cache does not trust gets a successful response, but its signature is not served; trust a key in `nix.trusted_keys` to have its signatures merged.
- A narinfo with a wrong `Compression`, a missing `Compression` on a non-bzip2 NAR, or a malformed `CA` is refused instead of being stored permanently and breaking clients.
- A path poisoned by a first upload stays poisoned until the O2 operator command exists; the database can be fixed by hand meanwhile.
- A NixOS configuration without `storage.maxBytes` fails evaluation until O2.
- An anonymous client probing writes no longer fills the journal at warn.
- A client that stops reading a download for 60 s is disconnected; the proxy's own timeouts (0010) stay at or above it.
- A very slow verification or commit fails the upload with `deadline` rather than finishing after it.

## Rejected alternatives

- Replacing a narinfo when a newer upload carries a trusted signature: the unsigned fields could then be changed by any write-token holder at any time.
- Capping the number of signatures instead of the merged size: a size cap also bounds long key names.
- Defaulting `storage.maxBytes` to a fraction of the filesystem: the module cannot know the filesystem size at evaluation time.
- A download idle timeout implemented in the response body stream: the body is not polled while the socket is blocked, so it could not fire.
