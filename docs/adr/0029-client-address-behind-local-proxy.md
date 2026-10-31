# ADR 0029 — Client address behind the local proxy

Status: Accepted — 2026-10-31

## Context

oroshi serves plain HTTP behind a TLS reverse proxy on the same host (0010), so every client connection reaches it from a loopback address. 0021 deferred recording the client address in refusal logs until a decision on which proxy-supplied header to trust. The reverse proxy of the deployment overwrites `X-Forwarded-For` with the one client address it trusts, discarding any client-supplied value. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0021 — client address.** Each request carries a client address, computed once when the request is read:
  - The TCP peer is the client unless it is a loopback address: `127.0.0.0/8`, `::1`, or an IPv4-mapped loopback (`::ffff:127.0.0.1`). A non-loopback peer is never trusted, whatever header it sends.
  - From a loopback peer, the client is the **rightmost** entry of the last `X-Forwarded-For` header line, trimmed, parsed as a bare IPv4 or IPv6 address. The local proxy wrote that entry; entries left of it, if any, came from somewhere else.
  - An absent, empty or malformed entry (not a bare address, including one with a port) **falls back to the peer address**; the request is not refused. The address only serves logs, so refusing would fail requests over a diagnostic field.
  - `Forwarded` (RFC 7239) and other headers are **ignored**: the proxy writes `X-Forwarded-For`, and one trusted source cannot disagree with another.
  - IPv4-mapped addresses are logged in their IPv4 form.
- **Amends 0011 and 0021 — events.** `request`, `auth-refused` and `upload-refused` gain a `client` field. Successful writes need no change: their `request` line already carries it. No other event concerns a request.
- **Metrics** carry no client address (0011: closed label sets).
- **No security decision uses it.** Authentication is by token only (0008); the connection limit is global, not per address; there is no per-address rate limit. A later per-address decision must state whether it uses this address or the TCP peer.

## Consequences

- Logs and refusals name the client behind the local proxy instead of `127.0.0.1`.
- A local process connecting over loopback can put any address in the logs. It can already reach the backend directly (0010, 0014), and the address grants nothing.
- A deployment that puts a non-local proxy in front of oroshi logs that proxy's address.

## Rejected alternatives

- A configurable list of trusted proxies: one local proxy is the only supported topology (0010); a list is configuration for no deployment.
- The leftmost entry: under a proxy that appends, it is the one a client controls.
- Refusing a request with a malformed header from a loopback peer: fails a request over a log field.
