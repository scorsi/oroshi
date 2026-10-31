# ADR 0030 — Server binary name

Status: Accepted — 2026-10-31

## Context

0001 and 0003 name the single binary `oroshi`. The owner's other products ship their server as `<name>-server`, with its local admin subcommands, and keep `<name>` for a remote client CLI. oroshi has no client yet, but the bare name is the natural one for it; renaming after deployment would break every unit and script. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0001 and 0003 — binary name.** The one binary is `oroshi-server`. It carries the service and every local admin subcommand: `serve`, `config check`, `gc`, `nix drop`, `token hash`, `--version` (which prints `oroshi-server <version>`, 0013). "One repository", "one binary" and "one service for all modules" stand.
- **Amends 0003 — layout.** The application crate is `apps/oroshi-server` (package and binary `oroshi-server`): directories keep the name of the package they hold, as `crates/oroshi-store` does. Its log targets start with `oroshi_server` (`RUST_LOG`, `[log] level`).
- **Name reserved.** `oroshi` is reserved for a future remote client CLI (`apps/oroshi`), which talks to the server over HTTP. It is not built yet; it gets its own ADR.
- **Packaging.** The flake package is `packages.<system>.oroshi-server` (also `default`), `meta.mainProgram = "oroshi-server"`; the NixOS module's `services.oroshi.package` defaults to it, and `oroshi.service` and `oroshi-gc.service` run `oroshi-server serve` and `oroshi-server gc`.
- **Unchanged.** Option names `services.oroshi.*`, unit names `oroshi.service` and `oroshi-gc.{service,timer}`, user and group `oroshi`, the state directory, `/etc/oroshi/oroshi.toml`, metric names `oroshi_*`, the `Basic realm="oroshi"` challenge and the product name.

## Consequences

- A deployment that sets `services.oroshi.package` or refers to `packages.<system>.oroshi` switches to `oroshi-server`; one that keeps the module default needs no change.
- A per-target log filter written as `oroshi=…` becomes `oroshi_server=…`.
- A future client takes the short name without renaming anything installed.

## Rejected alternatives

- Keep `oroshi` as the server and name a future client `oroshi-cli`: differs from the owner's other products for no gain.
- `apps/server` as in kajiba: oroshi's directories are named after their packages (0003); `apps/oroshi-server` keeps that rule and leaves `apps/oroshi` to the client.
