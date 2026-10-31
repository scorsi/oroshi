# ADR 0028 — GC concurrency test coverage

Status: Accepted — 2026-10-31

## Context

0012 requires the `gc` nixosTest to prove that "an upload running during `systemctl start oroshi-gc` commits", and 0005 requires tests of uploads concurrent with GC. A VM test cannot force a given interleaving of two processes without a hook in the product to pause one of them. An accepted ADR is not modified (README): each item names the ADR point it amends; everything else in that ADR stands.

## Decision

- **Amends 0012 — `gc` nixosTest.** The overlap of a `nix copy` with `oroshi gc` is best effort: the copy is started after the clock move and runs while `systemctl start oroshi-gc` runs; the test asserts that the copy succeeds and the result is consistent, not that a given interleaving occurred.
- **Amends 0005 — where interleavings are proven.** Exact interleavings (a commit racing the delete of its reference, a deduplicated registration racing the sweep, a read between mark and delete) are proven by `oroshi-store` tests with two database handles on one state directory, driving each step explicitly.
- No test-only hook, option or environment variable exists in the product to pause or order GC steps.

## Consequences

- The nixosTest proves the real units coexist; the store tests prove the races, deterministically.
- A regression in an interleaving the store tests do not drive may pass the nixosTest by timing.

## Rejected alternatives

- A test-only pause hook in `oroshi gc`: product code and a configuration surface that exist only for tests.
