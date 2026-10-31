# ADR 0027 — NixOS module: `storage.maxBytes` optional

Status: Accepted — 2026-10-31

## Context

0020 makes the NixOS module require `storage.maxBytes` "until garbage collection ships (O2)", because without GC nothing bounds the cache. O2 ships `oroshi gc` and its timer. An accepted ADR is not modified (README): each item names the ADR point it supersedes; everything else in that ADR stands.

## Decision

- **Supersedes 0020 — `storage.maxBytes` in the NixOS module.** The module no longer asserts that `storage.maxBytes` is set; it is optional, as in the binary's configuration (0019). Unset, the cache is bounded by expiry (`retention.days`, 0005) only and no size-cap pass runs.

## Consequences

- A NixOS configuration without `storage.maxBytes` evaluates again.
- Bounding the filesystem stays the deployment's job (0014, 0019).

## Rejected alternatives

- Keeping the assertion: forces a cap on deployments that rely on expiry alone.
