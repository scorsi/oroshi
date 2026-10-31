# ADR — oroshi (卸)

Architecture decisions of the package registry. Format: status, context, decision, consequences, rejected alternatives; optional open points record remaining risks or questions. These design drafts are edited in place until accepted.

All ADRs are **Accepted** (0001-0022 on 2026-10-24, 0023-0028 on 2026-10-31). An accepted ADR is not modified: it is superseded by a new one. Decisions taken during a milestone or a review are recorded as one ADR per theme, never one ADR per event.

| No. | Title | Status |
| --- | --- | --- |
| [0001](0001-name-and-scope.md) | Name and scope | Accepted |
| [0002](0002-rust-and-dependencies.md) | Language: Rust, and the dependency policy | Accepted |
| [0003](0003-code-organization.md) | Code organization: one repository, one binary | Accepted |
| [0004](0004-storage.md) | Storage: content-addressed blobs, SQLite metadata | Accepted |
| [0005](0005-retention-and-gc.md) | Retention and garbage collection | Accepted |
| [0006](0006-nix-binary-cache.md) | Nix binary cache protocol | Accepted |
| [0007](0007-nix-signing-and-trust.md) | Nix signing: the builder signs, oroshi verifies | Accepted |
| [0008](0008-authentication.md) | Authentication: anonymous reads, hashed write tokens | Accepted |
| [0009](0009-configuration.md) | Configuration and secrets | Accepted |
| [0010](0010-http-exposure.md) | HTTP exposure: plain HTTP behind an internal TLS reverse proxy | Accepted |
| [0011](0011-operations-observability.md) | Operations and observability | Accepted |
| [0012](0012-nix-flake-module-tests.md) | Nix: flake outputs, NixOS module, tests with real clients | Accepted |
| [0013](0013-versions-compatibility.md) | Versions, migrations and compatibility | Accepted |
| [0014](0014-deployment-requirements.md) | Deployment requirements | Accepted |
| 0015 | Decisions taken during O0 and O1 | Split into [0017](0017-nix-narinfo-ingest-rules.md), [0018](0018-transfer-timeouts-and-upload-deadline.md), [0019](0019-state-ownership-storage-cap-blob-integrity.md), [0020](0020-nixos-module-config-check-and-tests.md), [0021](0021-log-events-and-levels.md), [0022](0022-dependency-changes.md) |
| 0016 | Decisions from the O1 security review | Split into [0017](0017-nix-narinfo-ingest-rules.md), [0018](0018-transfer-timeouts-and-upload-deadline.md), [0019](0019-state-ownership-storage-cap-blob-integrity.md), [0020](0020-nixos-module-config-check-and-tests.md), [0021](0021-log-events-and-levels.md) |
| [0017](0017-nix-narinfo-ingest-rules.md) | Nix narinfo ingest rules | Accepted |
| [0018](0018-transfer-timeouts-and-upload-deadline.md) | Transfer timeouts and the upload deadline | Accepted |
| [0019](0019-state-ownership-storage-cap-blob-integrity.md) | State ownership, storage cap and blob integrity | Accepted |
| [0020](0020-nixos-module-config-check-and-tests.md) | NixOS module, `config check` and test coverage | Accepted |
| [0021](0021-log-events-and-levels.md) | Log events and levels | Accepted |
| [0022](0022-dependency-changes.md) | Dependency changes | Accepted |
| [0023](0023-nix-named-roots-api.md) | Nix named roots API | Accepted |
| [0024](0024-gc-sweep-nar-grace-and-blob-deletion.md) | GC sweep: NAR row grace and blob deletion order | Accepted |
| [0025](0025-log-events-roots-drop-gc-failure.md) | Log events for roots, narinfo drop and GC failure | Accepted |
| [0026](0026-narinfo-drop-command.md) | Narinfo drop command | Accepted |
| [0027](0027-nixos-module-storage-max-bytes-optional.md) | NixOS module: `storage.maxBytes` optional | Accepted |
| [0028](0028-gc-concurrency-test-coverage.md) | GC concurrency test coverage | Accepted |

Deployment values (addresses, ports, keys, token hashes, host selection and capacity) are not recorded here: they live in the deployment repository.

Each language module (Go, Cargo, Hex, npm) starts with its own protocol ADR and, if it needs a crate, a dependency ADR (0002).
