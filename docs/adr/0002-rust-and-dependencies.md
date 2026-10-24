# ADR 0002 — Language: Rust, and the dependency policy

Status: Accepted — 2026-10-24

## Context

oroshi streams large bodies, hashes them and serves files: a long-running server where memory and predictable latency matter. Every crate is supply-chain surface and code the maintainer does not read.

## Decision

- **Rust**, stable toolchain, built by crane (0012). `#![forbid(unsafe_code)]` in every crate except where a listed crate requires otherwise (none in v1).
- **Any direct dependency not listed here is a new ADR**, never a `cargo add`. `cargo tree --depth 1` is checked against this list in review.
- Direct dependencies for the skeleton and the Nix module (O0, O1):

  | Crate | Used by | Why |
  | --- | --- | --- |
  | `tokio` (`rt-multi-thread`, `net`, `fs`, `io-util`, `time`, `macros`, `signal`) | app | the runtime |
  | `axum` (default features minus `json`/`form` if unused), `tokio-util` (`io`) | app | HTTP server, streaming bodies (`ReaderStream`); axum brings hyper and tower |
  | `serde` (`derive`), `toml` | app | configuration (0009) |
  | `serde_json` | app | log lines (below); later the JSON of Go, Cargo and npm protocols |
  | `tracing`, `tracing-subscriber` (`registry`, `std` only: no `fmt`, no `env-filter`) | app | logging facade and `Targets` filter |
  | `rusqlite` (system SQLite, not `bundled`) | `oroshi-store` | metadata (0004) |
  | `sha2` | `oroshi-store`, `oroshi-nix` | blob identity, Nix hashes |
  | `ed25519-dalek` (`std`, verification only) | `oroshi-nix` | narinfo signatures (0007) |
  | `base64` | `oroshi-nix`, app | signatures and keys, HTTP Basic credentials |

  Dev-only: `tempfile`, `proptest` (parsers and codecs).
- **Logs** are written by a hand-written `tracing` layer that emits nisshi's log schema v1 through `serde_json`, with a hand-written UTC timestamp formatter (9 fractional digits, from `SystemTime`): no `chrono`, no `time`, no `tracing-subscriber` `json` feature (0011).
- **Hand-written, no crate**: Nix base32, hex encoding, argument parsing (a few subcommands), `sd_notify` (`READY=1` on `NOTIFY_SOCKET` through `std::os::unix::net::UnixDatagram`), Prometheus text exposition (0011), SQLite migrations (`PRAGMA user_version`), error types (no `anyhow`, no `thiserror`).
- **Later modules start with their dependency ADR**: an HTTP client with TLS for pull-through (O3: candidates `hyper-util` client + `hyper-rustls`, `reqwest` with `rustls`), compression readers if a protocol needs them, RSA and protobuf for Hex's signed repository (O5). The choice is made when the need is real, not now.

## Consequences

- The dependency list of each crate is reviewable in one screen; adding one is a visible decision.
- `rusqlite` is synchronous: database calls run on `spawn_blocking`.

## Rejected alternatives

- Go: Rust is chosen for explicit ownership of buffers and predictable memory use in the streaming server.
- `sqlx`: async, but compile-time query checking, macros and a larger tree for a handful of queries.
- `reqwest` now: no pull-through in O1; deciding later costs nothing.
- `tracing-subscriber` `json` or `fmt`: the schema needs exact field order and timestamp format; a hand-written layer is a few dozen tested lines.
- `prometheus` / `metrics` crates: hand-written exposition is allowed by masu's convention and checked by `masu validate`.
