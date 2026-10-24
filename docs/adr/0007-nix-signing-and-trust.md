# ADR 0007 — Nix signing: the builder signs, oroshi verifies

Status: Accepted — 2026-10-24

## Context

Hosts accept a substituted path only if its narinfo carries a signature by a key in their `trusted-public-keys`. The signature covers the fingerprint `1;<store path>;<NarHash>;<NarSize>;<comma-separated reference paths>` with ed25519; keys are written `<name>:<base64>`. Either the producer signs before upload, or the cache signs on ingest.

## Decision

- **The machine that builds signs.** The building host signs with Nix's own mechanism (`secret-key-files` in its `nix.conf`, which signs every locally built path, or `nix store sign` before `nix copy`). The deployment provisions the secret key only on the building host. oroshi **never holds a signing key**.
- **oroshi verifies on ingest**: a narinfo without at least one valid signature by a key in `nix.trusted_keys` (a configurable list of public keys, module option `services.oroshi.nix.trustedKeys`, 0009, 0012) is refused with 400 naming the reason. This catches a misconfigured uploader before hosts reject the path one by one.
- Several uploaders are several keys (one per building host); rotation = add the new key to oroshi and to the hosts, switch the builder, remove the old key.
- **Mixed closures**: substituted dependencies carry upstream signatures, not the builder's signature. `nix.trusted_keys` may include upstream public keys (e.g. `cache.nixos.org-1:<base64>`) alongside builder keys, so a closure containing built and substituted paths is accepted when every path has a trusted signature. Upstream signatures are kept unchanged; oroshi never signs, including if a relay is added (0006).

- After successful builds, a Nix `post-build-hook` on the building host uploads the signed outputs and their closures with `nix copy`. The hook uses a client-side write token through `netrc`; it must sign outputs before copying if they were not signed automatically. Hook configuration belongs in the deployment repository.

## Consequences

- A compromised oroshi can delete or withhold paths but cannot make a host accept a forged one; a leaked write token alone cannot either.
- Builder keys and any configured upstream keys are trust anchors; readers must trust the corresponding keys to substitute the mixed closure.

## Rejected alternatives

- oroshi signs on ingest: the signing key lives on the registry service, and anyone with a write token gets arbitrary content signed — the token becomes as powerful as the key.
- Both sign: two keys to trust and rotate for no added guarantee.
- No verification on ingest: unsigned paths are stored, served and refused by every host, wasting disk and debugging time.
