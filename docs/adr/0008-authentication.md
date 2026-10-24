# ADR 0008 — Authentication: anonymous reads, hashed write tokens

Status: Accepted — 2026-10-24

## Context

Writers are explicitly authorized build and publishing clients. Readers are clients on the internal network. Clients authenticate their own way: Nix through `netrc` (HTTP Basic), Cargo with a raw `Authorization` token, Hex with a raw key, npm and Go with `Bearer`. Tokens use random values, hex SHA-256 hashes in plain configuration and constant-time comparison.

## Decision

- **Tokens**: 32 random bytes, base64url, generated outside oroshi. The configuration maps a name to the hex SHA-256 of the token and a list of scopes; **hashes are not secrets** and live in plain Nix. The token value is provisioned only on the client side; Nix uploaders use a `netrc` file. Secret provisioning is configured in the deployment repository. `oroshi token hash` prints the hash of a token read on stdin.
- **Scopes**: `<module>:write` (`nix:write`, later `cargo:write`, …), `nix:pin` (0005), `<module>:read` (reserved for possible future read authentication; unused in v1).
- **Accepted credentials**, on every route: `Authorization: Bearer <t>`, `Authorization: Basic <base64(user:t)>` (user ignored), and a raw `Authorization: <t>`. Hashing the presented token and comparing hashes in constant time.
- Responses: no credential on a write → 401 with `WWW-Authenticate: Basic realm="oroshi"` (so a client that waits for a challenge before sending credentials works; the nixosTest proves Nix's behaviour); unknown token → 401; valid token without the scope → 403. Each refusal is logged (`auth-refused`, the token name when known, never the token) and counted (0011).
- Rotation: list the old and new hashes under two names, switch the client, remove the old.
- **Anonymous reads**: no token is required. Clients can reach oroshi only from an internal network through the TLS reverse proxy's internal listener, never through a public listener or directly through the backend (0010, 0014). Every write requires a token with the appropriate scope.

## Consequences

- In v1, oroshi holds token hashes and public verification keys only, with no signing key or token value. These hashes and keys need no secret provisioning.
- A stolen write token can upload NAR blobs (unsigned narinfos are refused by oroshi, 0007) or fill the disk up to the cap (0004); rotation is a two-line change.

## Open points

- **Internal read visibility**: every store path pushed, including closures of private configurations, becomes readable by any peer of the internal network. Secrets managed outside the Nix store are not exposed by these cache reads; secrets embedded in store paths are readable. Revisit this policy once in production; requiring read tokens is an option.

## Rejected alternatives

- mTLS: requires client-certificate provisioning and support across every stock client; tokens work with the protocol authentication mechanisms.
- Tokens stored in a database with an admin API: an API to secure and a state to back up, for a handful of static writers declared in Nix.
- Token values in files read by oroshi: puts secrets on the storage host for no benefit over hashes.
