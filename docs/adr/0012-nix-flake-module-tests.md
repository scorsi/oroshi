# ADR 0012 — Nix: flake outputs, NixOS module, tests with real clients

Status: Accepted — 2026-10-24

## Context

The supported service environment is NixOS; packaging and tests use Nix. The only meaningful proof that a registry works is its stock client against it. `nix flake check` evaluates every nixosTest in one process and can exhaust memory when evaluating many VM tests together.

## Decision

- **Outputs**: `packages.oroshi` (crane, dependencies in their own derivation), `nixosModules.default` (`services.oroshi`), `devShells.default` (stable Rust, `sqlite`, `nix` for vectors), `checks`, `formatter` (treefmt). No darwin module: oroshi runs on NixOS only; clients use their standard protocol configuration on any supported client platform.
- **Inputs**: nixpkgs, flake-parts, crane, treefmt. **Test-only inputs**: `nisshi` and `masu` (`github:scorsi/<name>`), used by nixosTests only, never by the package or the module. No other input.
- **Module** `services.oroshi`: `enable`, `package`, `listen`, `metrics.listen` (default `null`), `stateDir`, `persistPaths` (read-only, paths requiring persistence), `log.level`, `log.access`, `storage.{maxBytes, gcTargetRatio}`, `retention.days`, `gc.{schedule, orphanGrace}`, `tokens.<name>.{sha256, scopes}`, `nix.{priority, trustedKeys, maxNarBytes}`, `settings` (raw TOML merged last). Typed options mirror the configuration fields one to one (0009): `listen` maps to `server.listen`, `stateDir` to `storage.dir`, and camelCase names map to snake_case fields. Exceptions: `enable`, `package`, `persistPaths` and `gc.schedule` are module-only; `settings` is the raw TOML escape hatch, merged last and still validated. It generates the TOML (`pkgs.formats.toml`), runs `oroshi config check` on it at build time, defines `oroshi.service` and `oroshi-gc.{service,timer}` as in 0011, and asserts `metrics.listen` is loopback. It does not configure the reverse proxy, observability collectors, clients or network access controls: those live in the deployment repository.
- **Checks by tier**:
  - **Unit** (all systems, inside the crane build): `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, a check that every binary carries the workspace version.
  - **Module evaluation** (all systems): the module with a full option set produces a configuration accepted by `config check`; a non-loopback `metrics.listen` fails evaluation.
  - **nixosTests** (Linux only, `lib.optionalAttrs`), each against real clients, each with the nisshi/masu validators of 0011:
    - `service` (O0): the unit starts, `/healthz` is 200, logs and metrics validate.
    - `nix-cache` (O1): nodes `cache`, `builder`, `host`. `builder` signs a test path with a test key and uploads through a Nix `post-build-hook` running `nix copy --to http://cache/nix` with a `netrc` on the isolated test network; `host` substitutes it with `nix-store --realise` through `substituters` and `trusted-public-keys`; a mixed built/substituted closure with builder and upstream keys trusted is accepted; unsigned upload, untrusted signature, missing reference, wrong token, no token are refused with the documented codes; restart of `cache` keeps the data; anonymous reads, token-authenticated writes and the internal TLS proxy boundary are tested as specified in O1.
    - `gc` (O2): rooted path survives, expired path (including one with a self-reference) and its blobs are gone, a host's closure stays complete, an upload running during `systemctl start oroshi-gc` commits.
    - One per language module (`go-proxy`, `cargo`, `hex`, `npm`) with `go`, `cargo`, `mix`, `pnpm`, and a fake upstream node serving static files (no network in the sandbox).
- CI runs one `nix build -L .#checks.<system>.<name>` per check; `nix flake check` stays the development command.
- **A breakage seen on a real host becomes a nixosTest case before it is fixed.**

## Consequences

- Unit and evaluation checks run on each supported system; nixosTests require a Linux environment capable of running NixOS VMs.
- Test fixtures (keys, signed narinfos) are generated in the devShell with `nix key generate-secret` and `nix store sign` and committed with the command that produced them.

## Rejected alternatives

- Testing against a hand-written client: proves oroshi agrees with itself, not with Nix.
- Upstream fakes inside oroshi's own test code for language modules: the nixosTest must use the real client end to end.
