# Using the Nix cache

These examples follow [the stock-client test](../nix/test-nix-cache.nix),
[the TLS boundary test](../nix/test-tls-boundary.nix) and
[the garbage collection test](../nix/test-gc.nix) (ADRs
[0005](adr/0005-retention-and-gc.md),
[0006](adr/0006-nix-binary-cache.md), [0007](adr/0007-nix-signing-and-trust.md),
[0008](adr/0008-authentication.md), [0010](adr/0010-http-exposure.md),
[0014](adr/0014-deployment-requirements.md)). Replace angle-bracket placeholders;
keep addresses, certificates, keys and credentials in the deployment repository.

## Read anonymously

On NixOS or nix-darwin, configure the internal TLS URL, including `/nix`:

```nix
nix.settings = {
  substituters = [ "https://oroshi.example.org/nix" ];
  trusted-public-keys = [ "builder.example.org-1:<base64-public-key>" ];
};
```

No read token or netrc is needed. Trust the proxy's certificate issuer in the
client CA bundle; the TLS test uses `ssl-cert-file = "/etc/ssl/certs/ca-certificates.crt"`.
Every internal peer can read uploaded paths: never embed secrets in store paths.
A miss returns 404 without an upstream request; clients can use their next
substituter, with its corresponding trusted public key.

## Sign and upload

Provision a signing key and a netrc readable only by the builder's Nix daemon.
Generate the signing key with `nix key generate-secret --key-name builder.example.org-1`
and obtain its public key with `nix key convert-secret-to-public` on stdin.
Provision a random 32-byte base64url write token outside oroshi. The netrc contains
the token value, never its hash (the Basic login name is ignored):

```text
machine oroshi.example.org login builder password <token>
```

Hash the exact token bytes, without an added newline:

```sh
oroshi-server token hash < /run/secrets/oroshi-token
```

On the cache's NixOS configuration, after importing oroshi's module:

```nix
services.oroshi = {
  enable = true;
  listen = "127.0.0.1:8080";
  tokens.builder = {
    sha256 = "<64-lowercase-hex-digits-from-oroshi-token-hash>";
    scopes = [ "nix:write" ];
  };
  nix.trustedKeys = [ "builder.example.org-1:<base64-public-key>" ];
};
```

Only the hash and public keys go to oroshi; it never holds a signing key or signs.
`nix:write` authorizes uploads; `nix:read` is reserved and unused for reads.

On the builder (NixOS or nix-darwin):

```nix
{ pkgs, ... }: {
  nix.settings = {
    experimental-features = [ "nix-command" ];
    secret-key-files = [ "/run/secrets/builder.sec" ];
    netrc-file = "/run/secrets/oroshi.netrc";
    post-build-hook = pkgs.writeShellScript "oroshi-upload" ''
      set -eu
      export NIX_CONFIG="experimental-features = nix-command
      netrc-file = /run/secrets/oroshi.netrc"
      ${pkgs.nix}/bin/nix copy --to 'https://oroshi.example.org/nix?compression=zstd' $OUT_PATHS
    '';
  };
}
```

Nix signs locally built outputs automatically; the hook copies their closures in
dependency order. Substituted dependencies retain upstream signatures: add those
upstream public keys alongside the builder key in `services.oroshi.nix.trustedKeys`
(TOML `nix.trusted_keys`) and readers' `trusted-public-keys`.
Every narinfo needs at least one valid trusted signature.

To merge another signature into an existing remote path in `$path`, use the
builder's netrc setting and `nix store sign --store`, as the test does:

```sh
nix store sign --store https://oroshi.example.org/nix --key-file /run/secrets/second.sec "$path"
```

Repeating `nix copy` skips existing paths; it does not propagate new signatures.
Only signatures may change: other narinfo differences return 409.

## Internal TLS boundary

Use a co-located TLS reverse proxy, for example genkan. The following nginx site
matches the test's streaming contract; replace the internal address and certificate
paths. Keep the backend on loopback, allow internal peers only, and give public
listeners no route to oroshi (the test's public listener returns 404).

```nginx
server {
    listen <internal-address>:443 ssl;
    server_name oroshi.example.org;
    ssl_certificate /run/credentials/server.pem;
    ssl_certificate_key /run/credentials/server.key;
    client_max_body_size 0;
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_read_timeout 60s;
        proxy_send_timeout 60s;
        proxy_request_buffering off;
        proxy_buffering off;
    }
}
```

Preserve `/nix`; permit continuous transfers without a conflicting total size or
duration limit. oroshi still enforces upload caps, a total deadline shorter than
half the orphan grace, and a 60-second idle timeout on upload bodies and on
responses a client stops reading. Clients must never access the
plain HTTP backend directly. The service also requires one serving instance per
state directory on a durable local filesystem with atomic rename, file/directory
fsync and SQLite WAL locking; leave disk headroom beyond `storage.maxBytes`, when
set. It is a logical cap on blob bytes: bound the filesystem with a quota as well.

## Retention and garbage collection

`oroshi-server gc` deletes what is no longer kept; uploads and reads continue while it runs.

- **Expiry**: a path not read or uploaded for `retention.days` (default 30) expires,
  unless a named root holds it or a path that is still kept references it. Reads
  refresh a path's access time at most once per hour.
- **Size cap**: with `storage.maxBytes` set, when registered blob bytes stay above
  `storage.gcTargetRatio` (default 0.9) of it after expiry, gc also removes the least
  recently accessed paths outside every root's closure, never one that a remaining
  path references, until it reaches the target. Without `storage.maxBytes` only
  expiry bounds the cache.
- **Sweep**: NARs and blobs no path uses, files under `blobs/` without a row and
  temporary files are removed once older than `gc.orphanGrace` (default 3600 s),
  so in-flight uploads are never touched.

```nix
services.oroshi = {
  retention.days = 30;
  storage = {
    maxBytes = 500 * 1024 * 1024 * 1024;
    gcTargetRatio = 0.9;
  };
  gc = {
    schedule = "daily"; # systemd OnCalendar, used by oroshi-gc.timer
    orphanGrace = 3600;
  };
};
```

The module installs `oroshi-gc.timer` (`Persistent=true`: a run missed while the
machine was off happens at the next boot) and `oroshi-gc.service`, a oneshot running
`oroshi-server gc --config <path>` as the `oroshi` user with the server's sandbox. Run it now
with:

```sh
systemctl start oroshi-gc.service
```

Each run takes `gc.lock` in the state directory (a second concurrent run is refused),
writes one `gc-run{deleted_entries,deleted_blobs,freed_bytes,dur_us}` line (at
`error` with `err` on failure) and exits non-zero on failure. The server exposes
`oroshi_gc_runs_total{result}`, `oroshi_gc_freed_bytes_total` and
`oroshi_gc_last_success_timestamp_seconds` (0 before any successful run); alert
when the last success grows older than the schedule.

## Pin paths with named roots

A root keeps the closures of its store paths forever, whatever their access time
or the size cap. Give the release automation a token with `nix:pin` (add
`"nix:write"` too if it also uploads):

```nix
services.oroshi.tokens.release = {
  sha256 = "<64-lowercase-hex-digits-from-oroshi-token-hash>";
  scopes = [ "nix:pin" ];
};
```

`PUT` replaces the root with the store paths of the body, one per line; every path
must already be in the cache. `DELETE` removes the root; its paths then expire
normally. Names match `[A-Za-z0-9._-]{1,64}`.

```sh
printf '%s\n' /nix/store/<hash>-<name> /nix/store/<hash>-<other> |
  curl -fsS -X PUT -H "Authorization: Bearer $(cat /run/secrets/oroshi-pin-token)" \
    --data-binary @- https://oroshi.example.org/nix/_oroshi/roots/release
curl -fsS -X DELETE -H "Authorization: Bearer $(cat /run/secrets/oroshi-pin-token)" \
  https://oroshi.example.org/nix/_oroshi/roots/release
```

Both answer 204; a `DELETE` of an unknown root answers 404. A malformed name or
line is refused with 400 `bad-name`, an absent path with 400 `missing-reference`,
an empty list with 400 `invalid-body` and a body above 1 MiB with 413 `too-large`;
a token without `nix:pin` gets 403. Changes are logged as `root-set{name,paths}`
and `root-deleted{name}`. Roots cannot be read back: the automation that sets them
already knows their content.

## Drop a poisoned narinfo

To remove a path that must not be served (for example a broken build signed by a
trusted key), run on the cache, as the `oroshi` user, against the module's
configuration:

```sh
sudo -u oroshi <oroshi-package>/bin/oroshi-server nix drop --config /etc/oroshi/oroshi.toml \
  /nix/store/<hash>-<name>
```

The narinfo and its references are deleted at once; its NAR and blob are left to
the next gc sweep. The drop is refused when another path references it or a root
holds it; `--with-referrers` also drops every path that references it, directly or
not, and is still refused, dropping nothing, if any of them is rooted. An absent
path is refused. Refusals exit non-zero with a `fatal` line; success logs
`narinfo-dropped{name,referrers}`. A dropped path can then be uploaded again,
with a different NAR. There is no `--dry-run`.

## Refusals

Upload responses use the closed plain-text reasons in
[`apps/oroshi-server/src/nix.rs`](../apps/oroshi-server/src/nix.rs), logged as `upload-refused`;
authentication failures are logged as `auth-refused`. These lines and `request`
carry `client`: the address in `X-Forwarded-For` (its rightmost entry) when the
connection comes from a loopback address, as from the local reverse proxy, and the
connecting address otherwise (ADR 0029).

| HTTP | Reason | Meaning |
| --- | --- | --- |
| 401 | Authentication | Missing, malformed or unknown token; challenges with `WWW-Authenticate: Basic realm="oroshi"`. |
| 403 | Authentication | Known token without `nix:write`. |
| 400 | `invalid-body`, `invalid-narinfo` | Unreadable body or malformed narinfo, including a `Compression` outside none/xz/zstd/bzip2 or not matching the URL suffix, and a malformed `CA`. |
| 400 | `bad-name` | Invalid store path or NAR URL in narinfo. |
| 400 | `hash-mismatch`, `file-size-mismatch` | NAR hash, StorePath URL hash, FileHash or FileSize mismatch. |
| 400 | `unsigned`, `untrusted-key` | No signature, or no valid signature from a trusted key. |
| 400 | `missing-nar`, `missing-reference` | NAR not uploaded yet, or referenced narinfo absent (self-references allowed). |
| 403 | `bad-name` | Unsupported PUT path, NAR filename or compression suffix. |
| 408 | `idle`, `deadline` | Idle timeout or total upload deadline. |
| 409 | `conflict` | Existing narinfo differs beyond signatures. |
| 413 | `too-large` | NAR exceeds `nix.maxNarBytes`, or narinfo exceeds 64 KiB, before or after merging signatures. |
| 415 | `unsupported-encoding` | Content-Encoding other than identity; NAR compression belongs in the filename. |
| 500 | `storage-error` | Internal storage/worker failure. |
| 507 | `storage-full` | Upload would exceed `storage.maxBytes`. |

Missing or malformed read names return 404. Supported NAR suffixes are `.nar`,
`.nar.xz`, `.nar.zst` and `.nar.bz2`; oroshi does not decompress or recompress.
