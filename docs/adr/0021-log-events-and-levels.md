# ADR 0021 — Log events and levels

Status: Accepted — 2026-10-24

## Context

0011 defines the log events. Implementing O0 and O1 and the O1 security review had to fix some event fields and levels. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands. The upload refusal reasons are in 0017.

## Decision

- **Amends 0011 — events.** `migration-applied{version}` is logged once per applied migration version. The `request` event's `status` is the integer HTTP status, the same convention as the other products of the ecosystem.
- **Amends 0011 — `auth-refused` level.** A refusal of a request carrying no `Authorization` header is logged at `info`: clients routinely send a first request without credentials and wait for the challenge. A refusal with a credential stays at `warn`. Recording the client address in these events is deferred: it needs a decision on which proxy-supplied header to trust.

## Consequences

- An anonymous client probing writes no longer fills the journal at warn.
