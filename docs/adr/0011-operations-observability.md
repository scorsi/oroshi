# ADR 0011 — Operations and observability

Status: Accepted — 2026-10-24

## Context

oroshi's observability contracts interoperate with hikyaku (journal collection and loopback metrics scraping), nisshi (logs) and masu (metrics). nisshi fixes the log schema v1 (`docs/log-schema.md`) and how services write it (nisshi 0009: one JSON object per line on stderr, no journald client, journald rate limit for per-request loggers); masu fixes the metric convention (masu 0005, `docs/metric-convention.md`: loopback listener, `<service>_<what>_<unit>`, closed label sets, at most 500 series). hikyaku itself is configured in the deployment repository, not here.

## Decision

- **Logs**: every line `oroshi` writes on stderr, in every subcommand, is one schema v1 object: `v` (1), `ts` (UTC, RFC 3339, exactly 9 fractional digits, `Z`), `level` (`debug`, `info`, `warn`, `error`; `trace` written as `debug`), `event` (kebab-case), optional `msg`, `err` only at `warn` and `error`, then the event's fields. Never a reserved field (`node`, `service`, `stream`, `seq`, `source`, `recv_ts`, `_*`), never a token or an `Authorization` header. Written by the hand-written `tracing` layer of 0002; a panic hook writes `fatal{err}`.
- **Events**: `start{version}`, `listening{addr}`, `metrics-listening{addr}`, `ready`, `stopping`, `fatal`, `config-invalid`; `request{module,method,path,status,bytes,dur_us}`; `upload-committed{module,kind,name,bytes}`; `upload-refused{module,kind,reason}` (warn); `auth-refused{module,token}` (warn); `gc-run{deleted_entries,deleted_blobs,freed_bytes,dur_us}`; `migration-applied{version}`.
- **Quiet reads**: a `nix build` asks thousands of narinfos. Successful and 404 reads are counted, not logged, unless `log.access = true` (default false); writes, refusals and 5xx are always logged. The module still sets `LogRateLimitIntervalSec=30s` and `LogRateLimitBurst=30000` as `mkDefault` (nisshi 0009), since `log.access` makes oroshi a per-request logger.
- **Metrics**: `GET /metrics` on `[metrics] listen`, module option `services.oroshi.metrics.listen`, default `null` (no listener; the deployment picks the port). Loopback only, refused otherwise by `config check` and asserted by the module. Hand-written Prometheus text 0.0.4:

  | Metric | Labels |
  | --- | --- |
  | `oroshi_build_info` (gauge 1) | `version` |
  | `oroshi_requests_total` | `module` (`nix`, `go`, `cargo`, `hex`, `npm`, `other`), `method` (`GET`, `HEAD`, `PUT`, `other`), `code` (`1xx`, `2xx`, `3xx`, `4xx`, `5xx`) |
  | `oroshi_request_duration_seconds` (histogram, 0.01, 0.05, 0.25, 1, 5, 30 s) | `module` |
  | `oroshi_response_bytes_total`, `oroshi_upload_bytes_total` | `module` |
  | `oroshi_uploads_total` | `module`, `result` (`committed`, `refused`) |
  | `oroshi_auth_refusals_total` | `module` |
  | `oroshi_stored_bytes`, `oroshi_blobs` (gauges, from the database) | none |
  | `oroshi_gc_runs_total` | `result` (`ok`, `error`) |
  | `oroshi_gc_freed_bytes_total` | none |
  | `oroshi_gc_last_success_timestamp_seconds` | none |

  Closed label sets only; paths, token names and store hashes go to logs. Counters never reset except at process start. GC metrics are written by the `gc` subcommand into the database and exposed by the server, since the timer process has no listener. The reachable label sets are the five protocol modules plus `other` (health and fallback routes); methods other than `GET`, `HEAD`, `PUT` collapse to `other`; status codes collapse to their five classes. Budget the full Cartesian product, including combinations unused by current routes:

  | Metric family | Maximum series |
  | --- | --- |
  | Build info (one running version) | 1 |
  | Requests: 6 modules × 4 methods × 5 status classes | 120 |
  | Duration: 6 modules × (6 finite buckets + `+Inf` + sum + count) | 54 |
  | Response and upload bytes: 2 × 6 modules | 12 |
  | Uploads: 6 modules × 2 results | 12 |
  | Auth refusals: 6 modules | 6 |
  | Stored bytes and blobs | 2 |
  | GC runs: 2 results | 2 |
  | GC freed bytes and last success | 2 |
  | **Total** | **211 of the 500-series budget** |

  Future HTTP methods remain `other`; adding a label value or metric requires updating this bound and the series-count test.
- **Health**: `Type=notify`, `READY=1` once migrations are applied, `stateDir` is writable and all configured listeners are bound; `GET /healthz` on the main listener answers 200 with no body (for reverse proxy health checks). No separate readiness endpoint.
- **systemd** (module): static user and group `oroshi` (owns the state directory, 0014), `StateDirectory`, `ProtectSystem=strict`, `ReadWritePaths=stateDir`, `PrivateTmp`, `NoNewPrivileges`, empty `CapabilityBoundingSet=`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `UMask=0027`; timer `oroshi-gc.timer` (daily, `Persistent=true`) running `oroshi gc` as the same user.
- **Validators in tests**: the nixosTests pipe `journalctl _SYSTEMD_UNIT=oroshi.service -o cat` (and `oroshi-gc.service`) into `nisshi validate --logs` after asserting the expected events exist, and run `masu validate --metrics` on the live endpoint, checking that `oroshi_requests_total` moved. nisshi and masu are test-only flake inputs (0012).

## Consequences

- Debugging is `journalctl -u oroshi -o cat | jq`; field filters of `journalctl` do not see service fields.
- A hit-rate or disk alarm is masu's job from these metrics; oroshi sends nothing anywhere.

## Rejected alternatives

- Logging every read: a cold `nix build` would push thousands of lines per minute to nisshi for no diagnostic value.
- A metrics client library: hand-written exposition is allowed and validated by `masu validate`.
- GC inside the server on an internal schedule: a scheduler and a second code path for what a timer and a subcommand do, testable by `systemctl start`.
