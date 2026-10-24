# ADR 0022 — Dependency changes

Status: Accepted — 2026-10-24

## Context

0002 requires an ADR for dependency decisions. Implementing O0 and O1 had to fix where `sha2` is used. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0002 — `sha2`.** `sha2` is a direct dependency of the app (token hashing in `oroshi token hash` and token verification, 0008) and of `oroshi-store` (blob identity). `oroshi-nix` does not depend on it: narinfo hashes are parsed and encoded, never computed there.

## Consequences

- `oroshi-nix` stays free of hashing code.
