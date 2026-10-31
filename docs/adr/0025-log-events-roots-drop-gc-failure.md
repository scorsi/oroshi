# ADR 0025 — Log events for roots, narinfo drop and GC failure

Status: Accepted — 2026-10-31

## Context

0011 lists the log events and 0021 amends some of them. O2 adds named roots (0023) and the narinfo drop command (0026), whose changes must be traceable, and `oroshi gc` can fail. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0011 and 0021 — new events**, all at `info`:
  - `root-set{name,paths}`: a root was created or replaced; `paths` is the number of distinct store paths in it.
  - `root-deleted{name}`: a root was removed.
  - `narinfo-dropped{name,referrers}`: `oroshi nix drop` removed a narinfo; `name` is its store path base name (`<hash>-<name>`), `referrers` the number of referrers dropped with it (0 without `--with-referrers`).
- **Amends 0011 — failed GC.** A failed `oroshi gc` writes its `gc-run` line at `error` with `err`, carrying the counts reached before the failure; no separate failure event.
- Refusals of the roots API are `upload-refused` or `auth-refused` (0023, 0008); a refused drop is a command-line error logged as `fatal` with a non-zero exit.

## Consequences

- `journalctl` shows every change of a root and every manual drop; the root content itself is not logged.
- A dashboard detects a failed GC from one event name and its level.

## Rejected alternatives

- Logging the full path list of a root: up to 1 MiB in one line.
- A `gc-failed` event: two event names for one run.
