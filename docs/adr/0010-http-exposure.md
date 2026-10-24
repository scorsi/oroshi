# ADR 0010 — HTTP exposure: plain HTTP behind an internal TLS reverse proxy

Status: Accepted — 2026-10-24

## Context

Tokens must only travel over TLS (0008). oroshi needs a reverse proxy that streams large requests and responses without a total-body size or duration limit that conflicts with the module limits. genkan provides a compatible contract: HTTP sites keyed by host name with an `http://` backend, an internal listener separate from its public listener, no body size limit, and a 60 s idle timeout that allows slow continuous transfers (genkan 0007, 0013).

## Decision

- oroshi serves **plain HTTP/1.1** on `server.listen` and **never terminates TLS**. A TLS reverse proxy exposes one site, for example `oroshi.example.org`, on its internal listener only. No public listener may route to oroshi. The deployment repository configures the site and network access controls.
- Clients must not reach the backend directly. The deployment must restrict backend access to the reverse proxy and protect that hop against interception and modification. The backend listener is not a client-facing endpoint (0014).
- **URL layout**: one host, one prefix per module — `/nix/`, `/go/`, `/cargo/`, `/hex/`, `/npm/` — plus `/healthz` (0011) and `/nix/_oroshi/` for oroshi-specific Nix endpoints (0005). Every stock client accepts a base URL with a path (`https://oroshi.example.org/nix`, `GOPROXY=https://oroshi.example.org/go`, `sparse+https://oroshi.example.org/cargo/index/`, …), so the proxy needs no routing per module.
- Request limits in oroshi: per-module body caps (0006), the upload deadline (0005), and a 60 s idle timeout. A compatible proxy must allow continuous transfers within those limits.
- Reads are anonymous; writes require scoped tokens (0008). The internal network restriction is the read access boundary.

## Consequences

- No TLS library or certificate handling in oroshi; certificate provisioning belongs to the deployment.
- If the reverse proxy is down, oroshi is unreachable for clients; Nix clients can fall back to their next substituter.

## Rejected alternatives

- Own TLS: duplicates the reverse proxy's certificate handling and reload path.
- One host name per module: five proxy sites for one process.
- Public exposure: anonymous reads are intended only for internal-network peers (0008).
