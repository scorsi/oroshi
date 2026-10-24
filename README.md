# oroshi (卸)

> Package registry: Nix binary cache first, then Go, Cargo, Hex and npm registries with pull-through caching. Rust.

*卸 (oroshi): wholesale — the warehouse that distributes goods to the shops.*

**Status:** O0 task 1 — flake and Cargo workspace skeleton with `oroshi --version`. Accepted ADRs in [`docs/adr/`](docs/adr/README.md), plan in [`docs/milestones.md`](docs/milestones.md).

## Role

1. **Nix binary cache**: building hosts sign and upload build results through a Nix `post-build-hook`; clients substitute them instead of rebuilding.
2. **Language registries**, one module per ecosystem, each with publishing and **pull-through caching** of the public registry. Cached packages remain available when the upstream is unavailable.
3. **Generated protobuf packages** (later): each service owns its `proto/` folder; CI publishes the generated Rust crate, Go module, Hex package and npm package here, so consumers depend on regular versioned packages.

oroshi is a separate product, not part of kajiba: it manages large blobs, bandwidth, retention and GC independently and is useful without the forge.

## Protocols (standards only)

| Module | Protocol | Upstream relay |
|---|---|---|
| Nix | binary cache over HTTP (`.narinfo`, NAR, ed25519 signatures) | none in v1 (0006) |
| Go | module proxy (`GOPROXY`: `@v/list`, `.info`, `.mod`, `.zip`) | proxy.golang.org |
| Rust | Cargo alternative registry, sparse index + publish API | crates.io |
| Elixir | self-hosted Hex repository (signed static files) | hex.pm |
| npm | npm registry protocol | registry.npmjs.org |

OCI images are outside the product scope. No protobuf registry protocol either (the Buf Schema Registry API is proprietary); protos are published as generated packages.

## Order

1. Nix binary cache.
2. Go module proxy (smallest protocol).
3. Cargo sparse registry, then Hex, then npm.
4. CI pipeline publishing generated protobuf packages, once consumers are numerous enough.

## Shared core

Content-addressed blob storage (0004), retention and garbage collection (0005), write tokens (0008: hashes in the configuration, token values held only by clients). Reads are anonymous and reachable only from an internal network through the TLS reverse proxy's internal listener (0010).

## Testing

See [Nix cache usage](docs/usage.md) for client, uploader and internal TLS proxy configuration.

`nixosTest` per module, using the real clients: `nix copy` / substituters, `go`, `cargo`, `mix`, `pnpm`.

## Roadmap

Nix cache first; language modules added over time. See the milestone plan for acceptance criteria.

## Conventions

Nix flake exposing the package, a NixOS module and the tests (0012). Logs in nisshi's schema, loopback `/metrics` in masu's convention (0011). Plain HTTP behind a TLS reverse proxy with an internal listener (0010); deployment requirements in ADR 0014. Docs in English.
