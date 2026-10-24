# oroshi — milestone plan

Technical milestones, no calendar. Each milestone has a verifiable **deliverable**, explicit **acceptance criteria**, and the **tests** that prove them. A milestone is closed when its criteria pass on supported systems (nixosTests on Linux, one `nix build` per check, 0012); `nix flake check` never stays red in between. ADR references in parentheses.

```
O0 skeleton → O1 Nix cache → O2 retention and GC → O3 Go proxy → O4 Cargo → O5 Hex → O6 npm
```

## External dependencies

| Dependency | Blocks |
| --- | --- |
| A Linux environment capable of running NixOS VM tests | nixosTests of every milestone |
| Stock ecosystem clients from the flake's nixpkgs | protocol compatibility tests (0012, 0013) |

Deployment acceptance separately requires the filesystem, resources and network boundary of 0014, an internal TLS reverse proxy (0010), and a building host configured to sign and upload through a Nix `post-build-hook` (0007). Those settings belong in the deployment repository; no host selection blocks product development.

---

## O0 — Skeleton: a service that starts, logs and exposes metrics

**Deliverable**: the repository (0003) with `nix flake check` green, an `oroshi` binary that loads its configuration, serves `/healthz` and `/metrics`, writes schema logs, and a NixOS module that runs it hardened. No registry feature.

Content: flake (flake-parts, crane, treefmt; test-only inputs `nisshi`, `masu`), Cargo workspace with `crates/oroshi-store`, `crates/oroshi-nix` (empty libs) and `apps/oroshi`; `LICENSE` (Apache-2.0), `README.md`, `docs/`; config struct and `config check` (0009); logging layer (0011); hand-written metrics registry and exposition (0011); server with `/healthz`, `READY=1`, graceful stop on SIGTERM; `nixosModules.default` with hardening, `LogRateLimit*` defaults, `metrics.listen` assertion; nixosTest `service`.

**Acceptance criteria**

- `nix build .#oroshi` works on supported Linux and Darwin systems; `oroshi --version` prints the workspace version.
- `oroshi config check` accepts the example configuration and refuses: an unknown key, a `metrics.listen` off loopback, a token hash that is not 64 hex characters, an unknown scope — each with a message naming the key.
- `cargo test`: every log line from the layer passes the schema (9-digit UTC `ts` for known instants including leap-year and end-of-year dates, `err` only at warn/error, string escaping, no reserved field); the exposition of a registry with a counter, a gauge and a histogram matches a golden.
- Module evaluation check: a full option set produces a configuration accepted by `config check`; a non-loopback `metrics.listen` fails evaluation.
- nixosTest `checks.<system>.service`: `oroshi.service` reaches `active` through `Type=notify`; `GET /healthz` is 200; `systemctl show` confirms `LogRateLimitBurst=30000`, `ProtectSystem=strict`, user `oroshi`; journal lines contain `start`, `listening`, `metrics-listening`, `ready` and pass `nisshi validate --logs`; `/metrics` passes `masu validate --metrics` and `oroshi_requests_total` moves after a request; `stateDir` survives a restart.

### O0 tasks

1. **Flake and workspace.** flake-parts + crane + treefmt, workspace with the three members, `--version`, checks `cargo-test`, `cargo-clippy`, `cargo-fmt`, `versions` (every binary inherits the workspace version), devShell; test-only inputs `nisshi`, `masu` declared. Done: `nix flake check` green on supported systems.
2. **Configuration.** The serde struct of 0009 (sections of O0 and O1, `deny_unknown_fields`), defaults, validation (loopback `metrics.listen`, hash format, known scopes), `oroshi config check`, an example `nix/example.toml`. Done: unit tests for each refusal and for the example.
3. **Log layer.** Hand-written UTC formatter (civil date from Unix time, 9 digits), `tracing` layer writing schema v1 through `serde_json`, `Targets` filter from `RUST_LOG` or `[log] level`, panic hook writing `fatal`. Done: unit tests above; a test feeding the layer's output to the schema rules of nisshi's fixtures where they apply.
4. **Metrics registry.** Counters, gauges, fixed-bucket histograms with closed label sets, text 0.0.4 exposition, content type. Done: golden test; series-count test.
5. **HTTP server and metrics middleware** (0011). axum router (`/healthz`, fallback 404), main listener and loopback metrics listener, request-counting middleware (`module`, `method`, `code` class, duration). Done: in-process tests with a bound ephemeral port.
6. **Service lifecycle** (0011). `sd_notify` `READY=1` once startup is complete, `STOPPING=1` on shutdown, SIGTERM drain. Done: `NOTIFY_SOCKET` test with a `UnixDatagram`; a shutdown test proves in-flight requests drain.
7. **NixOS module and evaluation checks** (0012). Options that exist in O0, TOML generation and build-time `config check`, unit hardening, `LogRateLimit*` `mkDefault`, persistence paths. Done: module evaluation checks green on supported systems, including full-option configuration and non-loopback refusal.
8. **nixosTest `service`** (0011, 0012). The O0 service scenario above with nisshi/masu validators. Done: exposed as a flake check; all O0 acceptance criteria pass.

---

## O1 — Nix binary cache

**Deliverable**: a building host signs and uploads paths through a Nix `post-build-hook` using `nix copy`; clients substitute them anonymously. The service supports an internal TLS reverse proxy (0010, 0014).

Content: blob store and SQLite metadata with migrations (0004); Nix base32, narinfo, fingerprint and signature verification (0006, 0007); token check (0008); routes of 0006 with the closure invariant; `storage.max_bytes` refusal (507); upload deadline; logs and metrics of 0011 for these routes; module options `tokens`, `nix.*`, `storage.{maxBytes,gcTargetRatio}`; nixosTest `nix-cache`; `docs/usage.md` (client settings: `substituters`, `trusted-public-keys`, uploader `netrc` and URL with `?compression=zstd`, a `post-build-hook` example).

**Acceptance criteria**

- nixosTest `checks.<system>.nix-cache` (nodes `cache`, `builder`, `host`, keys and token generated for the test):
  - `builder` signs a two-path closure (a path and one reference); its Nix `post-build-hook` runs `nix copy --to 'http://cache/nix?compression=zstd'` with a `netrc`: success; the paths' narinfos are served; `nix path-info --store http://cache/nix --sigs` shows the signature. Plain HTTP here is confined to the isolated test network; deployments require the TLS boundary of 0010.
  - A closure containing a locally built path signed by the builder and a substituted dependency carrying only an upstream signature uploads successfully through the hook when both public keys are in `nix.trusted_keys` (`services.oroshi.nix.trustedKeys`); oroshi adds no signature.
  - `host`, without a read token, with `cache` as its only substituter and the test public key trusted, runs `nix-store --realise` on the top path: both paths are substituted, no build.
  - Refused with the documented status and a `upload-refused` or `auth-refused` line: unsigned narinfo (400), narinfo signed by an untrusted key (400), narinfo before its NAR (400), narinfo whose reference is absent (400), NAR whose name does not match its hash (400), no credential (401), unknown token (401), token without `nix:write` (403), upload above `storage.maxBytes` (507).
  - Re-uploading an identical path succeeds; `nix store sign` adding a second signature merges it; a conflicting narinfo is 409.
  - `cache` restarted: everything is still served. Journal of `oroshi.service` passes `nisshi validate --logs` and holds `upload-committed`; `masu validate --metrics` passes and `oroshi_uploads_total{module="nix",result="committed"}` moved.
- Unit tests (`oroshi-nix`): Nix base32 round-trip and vectors produced by `nix hash convert`; narinfo parse/serialize round-trip on narinfos produced by `nix copy --to file://` (committed fixtures), refusal of malformed fields; fingerprint and signature vectors produced by `nix key generate-secret` and `nix store sign`.
- Unit tests (`oroshi-store`): an aborted upload leaves no blob and no row; a blob is renamed only after its hash is checked; migrations apply on an empty database and are idempotent at restart; a narinfo row cannot reference an absent path.
- **Internal TLS boundary** (isolated integration test): clients can read without credentials and upload with a write token through the proxy's internal TLS listener; its public listener has no route to oroshi, and client peers cannot connect directly to the backend. A cache miss returns 404 without an upstream request; clients may use their next substituter (0006).

### O1 tasks

1. **Nix base32 and hashes** (`oroshi-nix`): encode/decode, `sha256:<base32>` parsing, store path syntax (`/nix/store/<32 chars>-<name>`, name charset). Vectors from `nix hash convert` in the devShell, committed with the command.
2. **narinfo** (`oroshi-nix`): parse/serialize, required and optional fields (`StorePath`, `URL`, `Compression`, `FileHash`, `FileSize`, `NarHash`, `NarSize`, `References`, `Deriver`, `Sig`, `CA`), unknown fields kept or refused (decide by test against Nix output), fingerprint. Fixtures from `nix copy --to file://`.
3. **Signatures** (`oroshi-nix`): parse `<name>:<base64>` public keys and signatures, verify ed25519 over the fingerprint, "at least one trusted signature" rule, signature merge. Vectors from `nix store sign`.
4. **Blob store** (`oroshi-store`): streaming write to `tmp/` with SHA-256, size cap, expected-hash check, `fsync`, `rename`, directory `fsync`; open for reading; startup removal of temp files; `LAYOUT` file.
5. **Database connections and schema** (`oroshi-store`, 0004, 0013): connection setup (WAL, single writer), migrations by `user_version`, tables `blob`, `nar`, `narinfo` (with `source`), `reference`. Done: empty-database and restart migration tests.
6. **Transactional closure commit** (`oroshi-store`, 0005, 0006): commit narinfo and references atomically, enforcing the closure invariant with self-references allowed. Done: absent-reference refusal, self-reference acceptance and rollback tests.
7. **Access-time batching** (`oroshi-store`, 0004): at most one update per entry per hour. Done: tests for recorded access and batching.
8. **Tokens** (app, 0008): hash table from config, the three `Authorization` forms, constant-time compare, scopes, 401/403 responses with `WWW-Authenticate`, `oroshi token hash`.
9. **Read routes** (app, 0006): `nix-cache-info`, `GET|HEAD` narinfo and NAR (streamed), 404 rules, quiet-read counting. In-process tests.
10. **NAR PUT** (app, 0006): streaming upload with size and expected-hash checks. Done: in-process upload and hash-mismatch tests.
11. **narinfo validation and merge** (app, 0006, 0007): every narinfo check, trusted signatures, 409 and signature merge; `nix-cache-info` 204, other `PUT` 403, unsupported `Content-Encoding` 415. Done: in-process tests per refusal and for successful commits and merges.
12. **Upload limits and deadlines** (app, 0005, 0006, 0010): `nix.max_nar_bytes`, `storage.max_bytes` (507), upload deadline shorter than the orphan grace, idle timeout. Done: in-process limit, timeout and cleanup tests.
13. **Observability of the cache** (app, 0011): `upload-committed`, `upload-refused`, `auth-refused` lines, upload metrics, stored-bytes gauges. Golden and schema tests.
14. **Module options** (0009, 0012): `tokens`, `nix.*`, `storage.{maxBytes,gcTargetRatio}`. Done: full-option evaluation check proves the generated configuration passes `config check`.
15. **Stock-client round trip nixosTest** (0006, 0007, 0012): `nix-cache` with the post-build-hook upload, anonymous substitution, mixed built/substituted closure, re-upload and signature merge scenarios above. Done: exposed as a flake check; real clients complete the round trip.
16. **Refusal and restart cases** (0006, 0008, 0011, 0012): extend `nix-cache` with every refusal above, including an untrusted signing key, and restart persistence. Done: documented codes, data survival and nisshi/masu validation pass.
17. **TLS boundary** (0010, 0012): isolated integration scenario above for anonymous reads and authenticated writes through internal TLS, no public route or direct backend access, and miss without relay. Done: boundary checks pass as a flake check.
18. **`docs/usage.md`**: client and uploader configuration, with `example.org` names only. Done: examples match the tested client contract; all O1 acceptance criteria pass.

---

## O2 — Retention and garbage collection

**Deliverable**: the cache stays under its cap without ever breaking a closure, while uploads continue (0005).

Content: access-time tracking, named roots (`/nix/_oroshi/roots/<name>`, scope `nix:pin`), `oroshi gc` (mark, re-checked deletes, blob sweep with grace, temp cleanup, size cap pass), `oroshi-gc.{service,timer}`, GC metrics and `gc-run` line, and an operator command to drop a narinfo so a path poisoned by a bad first upload can be uploaded again (0017).

**Acceptance criteria**

- Unit and property tests on the mark phase: random reference graphs with roots and access times → never deletes a live path, never leaves a narinfo whose reference is gone.
- GC regression test: an expired, unrooted path referenced only by itself is deleted with its self-edge and unshared blobs; a reference from another remaining narinfo still prevents deletion (0005, 0006).
- Concurrency test (`oroshi-store`): a commit racing a delete of its reference either succeeds with the reference kept or fails cleanly; a blob written but not yet committed survives a sweep inside the grace.
- nixosTest `checks.<system>.gc`: with the clock moved past `retention.days`, `systemctl start oroshi-gc` removes the unrooted closure and its blobs, keeps the rooted one; `host` still substitutes the rooted closure; a `nix copy` running during GC succeeds; `masu validate --metrics` passes and `oroshi_gc_last_success_timestamp_seconds` moved; logs validate.

---

## O3 — Go module proxy (pull-through)

**Deliverable**: `GOPROXY=https://oroshi.example.org/go` serves public modules through oroshi, cached with retention.

Content: protocol ADR (`@v/list`, `.info`, `.mod`, `.zip`, `@latest`, sumdb proxying or `GONOSUMDB` policy), dependency ADR for the HTTP client with TLS (0002), upstream configuration, pull-through cache rows with expiry (0005).

**Acceptance criteria**: nixosTest `go-proxy` with a fake upstream node serving a static GOPROXY tree: `go mod download` through oroshi succeeds, a second download with the upstream stopped succeeds from cache, an unknown module is 404/410 as the protocol requires; logs and metrics validate.

Publishing generated Go modules (protobuf) is a later addition with its own ADR.

---

## O4 — Cargo sparse registry

**Deliverable**: an alternative registry with publishing, plus pull-through of crates.io.

Content: protocol ADR (sparse index, `config.json`, publish, yank/unyank, download), first **primary data**: backup ADR and `oroshi dump` (0004), tokens with `cargo:write`.

**Acceptance criteria**: nixosTest `cargo`: `cargo publish` to oroshi, a second crate depending on it builds with `cargo build` from oroshi only; yank hides the version from resolution but keeps the download; crates.io pull-through from a fake upstream; dump then restore on a fresh node serves the same index; logs and metrics validate.

---

## O5 — Hex repository

**Deliverable**: a self-hosted Hex repository with publishing and hex.pm pull-through.

Content: protocol ADR (signed registry files, publishing path: Hex API subset versus an oroshi upload of `mix hex.build` tarballs — to decide then), dependency ADR (RSA signing, protobuf, gzip), the repository signing key as a deployment-provisioned secret file (0009).

**Acceptance criteria**: nixosTest `hex`: `mix deps.get` resolves a published package and a pulled-through one from a fake upstream; tampered registry file refused by `mix`; logs and metrics validate.

---

## O6 — npm registry

**Deliverable**: an npm registry with publishing and registry.npmjs.org pull-through.

Content: protocol ADR (packument, tarballs, `npm publish`, scopes, dist-tags), integrity hashes.

**Acceptance criteria**: nixosTest `npm`: `pnpm publish` then `pnpm install` from oroshi only; pull-through from a fake upstream; logs and metrics validate.

---

After O6: the CI pipeline publishing generated protobuf packages to the language modules, when consumers are numerous enough (README). The pipeline is external to oroshi.
