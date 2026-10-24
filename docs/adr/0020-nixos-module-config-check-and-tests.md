# ADR 0020 — NixOS module, `config check` and test coverage

Status: Accepted — 2026-10-24

## Context

0009 defines the configuration and 0012 the flake outputs, the NixOS module and the tests. Implementing O0 and O1 and the O1 security review had to fix the `config check` command line, what the module enforces, and which checks prove O1. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0009 — `config check`.** `oroshi config check` accepts the path as `--config <path>` or as a positional argument. A command-line usage error logs `fatal` and exits with status 2; an invalid configuration logs `config-invalid` and exits with status 1. The NixOS module also exposes the generated file at `/etc/oroshi/oroshi.toml`.
- **Supersedes 0004 — `storage.maxBytes` in the NixOS module.** Until garbage collection ships (O2), the module requires `storage.maxBytes` (an evaluation assertion with an explanatory message); the binary's configuration keeps it optional (its meaning: 0019).
- **Clarifies 0012 — network controls in the module.** 0012 leaves network controls to the deployment. The NixOS module nevertheless restricts the unit to loopback peers (`IPAddressAllow=localhost`, `IPAddressDeny=any`) and asserts that `listen` is a loopback address: clients reach oroshi only through the TLS proxy (0010), so this costs no supported setup. The deployment still owns the proxy and the firewall.
- **Amends 0012 — tests of O1.** The `nix copy` upload contract — NARs and narinfos uploaded with `PUT`, references before the paths that refer to them — is proven by `checks.<system>.nix-cache`, which also covers refusals, mixed-signer closures and restart. The TLS boundary (0010) is proven by `checks.<system>.tls-boundary`, with nginx as the example reverse proxy.

## Consequences

- A NixOS configuration without `storage.maxBytes` fails evaluation until O2.
- A module configuration with a non-loopback `listen` fails evaluation.

## Rejected alternatives

- Defaulting `storage.maxBytes` to a fraction of the filesystem: the module cannot know the filesystem size at evaluation time.
