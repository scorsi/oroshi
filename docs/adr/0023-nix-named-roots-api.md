# ADR 0023 — Nix named roots API

Status: Accepted — 2026-10-31

## Context

0005 defines named roots (`PUT /nix/_oroshi/roots/<name>` with a list of store paths, `DELETE` to drop it, scope `nix:pin`, 0008) but not the request body, the name syntax or whether a root can be read back. O2 implements them. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0005 — root name.** `<name>` matches `[A-Za-z0-9._-]{1,64}`; any other name is refused with 400 `bad-name`.
- **Amends 0005 — `PUT` body.** The body is `text/plain`: store paths in the form `/nix/store/<hash>-<name>`, one per line, at most 1 MiB. A body above 1 MiB is refused with 413 `too-large`; a line that is not a store path with 400 `bad-name`; a store path without a narinfo with 400 `missing-reference`. Empty lines are ignored and a repeated path counts once; a body with no path is refused with 400 `invalid-body` (`DELETE` removes a root). The new content replaces the previous one in one transaction; success is 204. Body transfer limits are those of uploads (0017, 0018): `unsupported-encoding`, `idle`, `deadline`. These refusals are logged as `upload-refused` with `kind` `root` (0017's reasons, no new reason).
- **Amends 0005 — `DELETE`.** Removes the root and answers 204; an unknown root answers 404 and is not logged as a refusal.
- **No read endpoint.** There is no `GET` on a root and no root listing: roots are written by automation that already knows their content, and the logs record each change (0025).

## Consequences

- A root of tens of thousands of paths fits the 1 MiB cap (a store path line is under 250 bytes).
- Inspecting roots needs the database (`sqlite3`) or the logs.

## Rejected alternatives

- A JSON body: needs a parser for a list of strings; one path per line is what `nix path-info -r` prints.
- A `GET` returning the root content: no consumer in O2; can be added by a later ADR without breaking clients.
