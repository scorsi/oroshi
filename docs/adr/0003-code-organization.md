# ADR 0003 — Code organization: one repository, one binary

Status: Accepted — 2026-10-24

## Context

Five protocol modules over a shared core, one process. Protocol logic (parsing, validation, formats) must be unit-testable without a server; HTTP plumbing (auth, metrics, logs) must be written once.

## Decision

- One repository `oroshi`, Cargo workspace, layout:
  - `crates/oroshi-store` — blob store, SQLite metadata and migrations, retention and GC (0004, 0005). No HTTP.
  - `crates/oroshi-nix` — Nix base32, store path syntax, narinfo parse/serialize, fingerprint and signature verification (0006, 0007). Pure functions, no IO, no HTTP.
  - Later `crates/oroshi-go`, `oroshi-cargo`, `oroshi-hex`, `oroshi-npm`, same rule: protocol logic only.
  - `apps/oroshi` — the single binary: configuration, logging, metrics, token checks, the axum router and one handler file per module (`src/nix.rs`, …), subcommands.
  - `nix/` — package, NixOS module, nixosTests (0012). `docs/adr/`, `docs/milestones.md`.
- Subcommands of `oroshi`: `serve`, `config check`, `gc` (0005), `token hash` (reads a token on stdin, prints its hex SHA-256, 0008), `--version`.
- One HTTP listener, one host name, modules under path prefixes: `/nix/`, `/go/`, `/cargo/`, `/hex/`, `/npm/` (0010).
- Single workspace version (0013).

## Consequences

- Only `apps/oroshi` depends on axum and tokio; protocol crates stay small and fast to test.
- A cross-cutting change is one commit and one `nix flake check`.

## Rejected alternatives

- One binary per module: five services, five units, five configs for one storage; nothing gained at this scale.
- One crate per module that owns its axum router: puts auth, metrics and logging behind a shared trait for no benefit.
