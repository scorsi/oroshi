# oroshi — notes for AI assistants

Read [`docs/adr/`](docs/adr/README.md) before making an architectural choice.
[`docs/milestones.md`](docs/milestones.md) is the plan: its acceptance criteria
are the definition of done for a milestone.

## Conventions

- Direct dependencies are limited to ADR 0002; any other crate needs a new ADR.
  Every crate forbids unsafe code.
- The workspace has one version in the root `Cargo.toml`; every member inherits
  it. `oroshi-server --version` and the flake's `versions` check enforce it (0013).
- Protocol logic lives in `crates/`, HTTP plumbing in `apps/oroshi-server` (0003).
- Format with `nix fmt` (treefmt: rustfmt, nixfmt).
- Verify with `cargo test --workspace` and `nix flake check`; report checks that
  could not run. Linux-only nixosTests are added by their milestone tasks.
- Code, comments, docs and messages are in English. Use `example.org` and
  "the deployment repository" instead of real deployment values.
