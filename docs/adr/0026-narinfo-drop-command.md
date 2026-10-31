# ADR 0026 — Narinfo drop command

Status: Accepted — 2026-10-31

## Context

0017 keeps "first upload wins" and promises an operator command to drop a narinfo with O2's garbage-collection tooling, so a path poisoned by a bad first upload can be uploaded again. Dropping a path that another narinfo references, or that a root holds, would break the closure invariant of 0005. An accepted ADR is not modified (README): each item names the ADR point it completes; everything else in that ADR stands.

## Decision

- **Completes 0017 — `oroshi nix drop --config <path> <store-path>`.** Runs against the live database, like `oroshi gc`, and takes the same `gc.lock`. It deletes the narinfo and its outgoing reference edges in one write transaction; its NAR and blob are left to the GC sweep (0024).
- **Referenced or rooted path.** The drop is refused when another narinfo references the path or a root holds it. With `--with-referrers`, it drops the path and all its transitive referrers in one transaction; it is still refused, dropping nothing, if any of them is rooted.
- **No dry run.** Neither `oroshi nix drop` nor `oroshi gc` has a `--dry-run` option.
- An absent path is refused. Success logs `narinfo-dropped` (0025).

## Consequences

- A poisoned path is fixed by `oroshi nix drop` then a new `nix copy`; with referrers, the client re-uploads them too.
- Dropping a rooted path needs the root changed first (0023): a root is an explicit promise.
- Without a dry run, the refusal message (referenced or rooted) is how an operator sees what a drop would affect.

## Rejected alternatives

- Dropping referrers implicitly: one command could silently remove a large closure.
- Leaving dangling references: `host` substitutions would fail on the missing path (0005).
- A `--dry-run` option: a second code path to keep in sync for a rare manual command; the refusals already describe the blocking paths.
