# ADR — oroshi (卸)

Architecture decisions of the package registry. Format: status, context, decision, consequences, rejected alternatives; optional open points record remaining risks or questions. These design drafts are edited in place until accepted.

All ADRs are **Accepted** (2026-10-24). An accepted ADR is not modified: it is superseded by a new one.

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
| [0015](0015-decisions-taken-during-o0-and-o1.md) | Decisions taken during O0 and O1 | Accepted |

Deployment values (addresses, ports, keys, token hashes, host selection and capacity) are not recorded here: they live in the deployment repository.

Each language module (Go, Cargo, Hex, npm) starts with its own protocol ADR and, if it needs a crate, a dependency ADR (0002).
