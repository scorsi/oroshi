# ADR 0001 — Name and scope

Status: Accepted — 2026-10-24

## Context

Clients rebuild the same Nix closures on every host, and language builds depend on public registries being up and fast. The forge (kajiba) explicitly leaves the package registry and the Nix binary cache out of its scope (kajiba 0001). A registry has its own profile: large blobs, bandwidth, retention, garbage collection; it needs independent capacity and retention controls and is useful without the forge.

## Decision

- Name: **oroshi** (卸, wholesale). One product, one binary `oroshi`, one repository.
- **Modules**, one per ecosystem, each speaking only the ecosystem's standard protocol, so the stock client works unchanged:

  | Module | Protocol | Own publishing | Pull-through |
  | --- | --- | --- | --- |
  | `nix` | HTTP binary cache (0006) | yes (CI uploads) | no in v1 (0006) |
  | `go` | module proxy (`GOPROXY`) | later (generated modules) | proxy.golang.org |
  | `cargo` | alternative registry, sparse index + publish API | yes | crates.io |
  | `hex` | self-hosted Hex repository | yes | hex.pm |
  | `npm` | npm registry protocol | yes | registry.npmjs.org |

- Order: `nix` first, then `go` (smallest protocol), `cargo`, `hex`, `npm` (milestones).
- **Shared core**: content-addressed blob storage and metadata (0004), retention and GC (0005), tokens (0008), configuration (0009), observability (0011).
- **Out of scope**: OCI images; a protobuf registry protocol (the Buf Schema Registry API is proprietary — protos are published as generated packages through the language modules, by a CI pipeline that lives outside oroshi); a web UI; a generic artifact store; multi-tenancy; public (Internet) exposure; S3 compatibility (0006).
- Technical milestones, no calendar.

## Consequences

- A module that cannot be served by its stock client with standard configuration is not done.
- Adding an ecosystem is a new module plus an ADR; the shared core does not change.

## Rejected alternatives

- Inside kajiba: different resource profile and life cycle, and the forge must stay useful without the cache and conversely.
- Off-the-shelf (attic, harmonia, verdaccio, athens, a hosted Artifactory): five tools, five configs, five storage and GC models instead of one.
