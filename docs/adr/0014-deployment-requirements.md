# ADR 0014 — Deployment requirements

Status: Accepted — 2026-10-24

## Context

oroshi streams large immutable blobs and commits metadata in SQLite WAL mode (0004). Its correctness depends on durable local filesystem operations, exclusive ownership of its state by one serving instance, and an internal network access boundary (0008, 0010). These are product requirements; host selection and capacity planning belong in the deployment repository.

## Decision

- **Storage and filesystem**: the entire state directory must be on a durable local filesystem with atomic rename, working file and directory `fsync`, and SQLite WAL locking semantics. `db/`, `tmp/` and `blobs/` must remain on that filesystem. Network and union filesystems are unsupported. Filesystem permissions must restrict state access to the service account and authorized administrators.
- **Capacity**: the deployment chooses the host and configures capacity through `storage.max_bytes` (`services.oroshi.storage.maxBytes` in the NixOS module). It must leave filesystem headroom for SQLite WAL, temporary uploads and other filesystem users; the logical cap is not a filesystem quota or a reservation (0004, 0005). Cache loss requires rebuilding or refetching; primary data requires the backup and restore contract introduced with O4.
- **Resources**: the supported service environment is NixOS with systemd (0012). The deployment must provide CPU, memory, disk I/O and network bandwidth for its expected concurrent uploads and downloads. Bodies are streamed rather than buffered in full; required capacity depends on workload and retention, not a fixed hardware model. Upload size and duration limits still apply (0005, 0006).
- **Single instance**: exactly one `oroshi serve` process owns a state directory. Multiple serving instances sharing state, active-active operation and distributed storage are unsupported. The `oroshi gc` subcommand may run concurrently with that server under the coordination rules of 0005.
- **Network boundary**: clients reach oroshi only from an internal network through a TLS reverse proxy's internal listener (0010). Public routing and direct client access to the plain HTTP backend are prohibited. The deployment restricts backend access to the proxy and protects the backend hop. Reads require no token; writes require scoped tokens (0008).
- **Deployment configuration**: host selection, capacity, addresses, proxy configuration, credentials and client setup belong in the deployment repository. oroshi supplies its package, service module, validation and protocol contracts; it does not select infrastructure.

## Consequences

- A deployment must satisfy the filesystem and network requirements before serving traffic; the NixOS module alone cannot establish that boundary.
- Moving the service requires stopping the serving instance before transferring its state, or starting with an empty cache. Primary data must be preserved through its backup and restore contract.
- Resource sizing is validated against the deployment's workload; no particular machine or storage capacity is required by the product.

## Rejected alternatives

- Selecting a particular host or storage device in a product ADR: deployment choices do not define the product contract.
- Network or union filesystems: their rename, durability or locking behavior is outside the supported storage contract.
- Shared-state serving replicas: require a different coordination and availability design.
- Direct client access to the backend or public exposure: bypasses the intended read access boundary and TLS protection.
