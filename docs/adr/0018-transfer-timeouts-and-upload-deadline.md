# ADR 0018 — Transfer timeouts and the upload deadline

Status: Accepted — 2026-10-24

## Context

0010 fixes a 60 s idle timeout and 0005 an upload deadline tied to the orphan grace. Implementing O1 and its security review had to fix what these timers cover. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0010 — idle timeout.** The idle timeout stays fixed at 60 s (not configurable). It applies to upload request bodies: an upload that receives no body bytes for 60 s is refused (`idle`). It also applies to responses: a response write that makes no progress for 60 s (the client stopped reading) closes the connection, releasing its open blob. oroshi sets no other idle timeout of its own on connections.
- **Amends 0005 — upload deadline.** The upload deadline is derived from the orphan grace: strictly below half of `gc.orphan_grace`, so an upload in progress always finishes or fails before its temporary data could be treated as an orphan. It covers the whole upload until the database commit: body, verification, blob publication and commit. It is checked again before the blob is published and before the metadata commit; past it, the upload is refused with 408 `deadline`. A blob published but not committed is an orphan, as with any metadata failure (0004).

## Consequences

- Changing `gc.orphan_grace` changes the upload deadline.
- A client that stops reading a download for 60 s is disconnected; the proxy's own timeouts (0010) stay at or above it.
- A very slow verification or commit fails the upload with `deadline` rather than finishing after it.

## Rejected alternatives

- A configurable idle timeout: no need found; the proxy limits are set against the fixed value (0010).
- A download idle timeout implemented in the response body stream: the body is not polled while the socket is blocked, so it could not fire.
