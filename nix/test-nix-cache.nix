# O1 tasks 15 and 16: stock-client round trips, refusals and restart
# (ADRs 0006, 0007, 0008, 0011 and 0012).
{
  self,
  pkgs,
  system,
  nisshi,
  masu,
}:
let
  validateLogs = nisshi.lib.validateLogs.${system};
  validateMetrics = masu.lib.validateMetrics.${system};
  metrics = "http://127.0.0.1:9000/metrics";
  cacheUrl = "http://cache:8080/nix";
  uploadUrl = "${cacheUrl}?compression=zstd";
  # Valid configuration placeholders, replaced with VM-generated credentials.
  templateKey = builtins.readFile ../crates/oroshi-nix/tests/fixtures/narinfo/test.example.org-1.pub;
  templateHash = builtins.hashString "sha256" "oroshi-test-placeholder";
  # Untrusted until the mixed-closure scenario substitutes the VM upstream key.
  upstreamTemplateKey = pkgs.lib.fileContents ../crates/oroshi-nix/tests/fixtures/narinfo/cache.nixos.org-1.pub;
  # Lowered to the stored bytes after the restart to exercise 507.
  templateMaxBytes = 1073741824;
  maxNarBytes = 65536;
  # A known token whose only scope is not nix:write.
  readerToken = "oroshi-test-reader-token";
  hook = pkgs.writeShellScript "oroshi-upload" ''
    set -eu
    export NIX_CONFIG="experimental-features = nix-command
    netrc-file = /root/cache.netrc"
    ${pkgs.nix}/bin/nix copy --to '${uploadUrl}' $OUT_PATHS
    printf '%s\n' $OUT_PATHS >> /root/uploaded-paths
  '';
in
pkgs.testers.runNixOSTest {
  name = "oroshi-nix-cache";

  nodes = {
    cache = { lib, ... }: {
      imports = [ self.nixosModules.default ];
      services.oroshi = {
        enable = true;
        listen = "0.0.0.0:8080";
        metrics.listen = "127.0.0.1:9000";
        tokens.builder = {
          sha256 = templateHash;
          scopes = [ "nix:write" ];
        };
        tokens.reader = {
          sha256 = builtins.hashString "sha256" readerToken;
          scopes = [ "nix:read" ];
        };
        nix = {
          trustedKeys = [
            templateKey
            upstreamTemplateKey
          ];
          inherit maxNarBytes;
        };
        storage.maxBytes = templateMaxBytes;
      };
      networking.firewall.allowedTCPPorts = [ 8080 ];
      environment.systemPackages = [ pkgs.curl ];
      # Provision the generated public key/hash before starting the service.
      systemd.services.oroshi = {
        wantedBy = lib.mkForce [ ];
        serviceConfig.ExecStart = lib.mkForce "${
          self.packages.${pkgs.stdenv.hostPlatform.system}.oroshi
        }/bin/oroshi serve --config /run/oroshi-test.toml";
      };
    };

    builder = {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = [ ];
        builders = pkgs.lib.mkForce null;
        secret-key-files = [ "/root/builder.sec" ];
        post-build-hook = hook;
        netrc-file = "/root/cache.netrc";
      };
      # Only the shell is an input; the outputs are created inside the VM.
      environment.systemPackages = [
        pkgs.bash
        pkgs.curl
      ];
      environment.etc."oroshi-closure.nix".text = ''
        { nonce }:
        let
          b = builtins.derivation {
            name = "oroshi-b-''${nonce}";
            system = "${pkgs.stdenv.hostPlatform.system}";
            builder = (builtins.storePath "${pkgs.bash}") + "/bin/bash";
            args = [ "-c" "set -eu; echo built-b-in-vm >&2; echo dependency > $out" ];
          };
          a = builtins.derivation {
            name = "oroshi-a-''${nonce}";
            system = "${pkgs.stdenv.hostPlatform.system}";
            builder = (builtins.storePath "${pkgs.bash}") + "/bin/bash";
            args = [ "-c" "set -eu; echo built-a-in-vm >&2; read -r value < ''${b}; printf '%s\\n' \"$value\" ''${b} > $out" ];
          };
        in { inherit a b; }
      '';
      # Built locally on top of a path substituted from the VM "upstream".
      environment.etc."oroshi-mixed.nix".text = ''
        { upstream }:
        builtins.derivation {
          name = "oroshi-mixed";
          system = "${pkgs.stdenv.hostPlatform.system}";
          builder = (builtins.storePath "${pkgs.bash}") + "/bin/bash";
          args = [ "-c" "set -eu; read -r value < ''${builtins.storePath upstream}; printf '%s\\n' \"$value\" ''${builtins.storePath upstream} > $out" ];
        }
      '';
      # Added, not built: no post-build-hook upload.
      environment.etc."oroshi-refusal.nix".text = ''
        { nonce }:
        let
          d = builtins.toFile "oroshi-refusal-d-''${nonce}" "dependency ''${nonce}\n";
        in
        {
          inherit d;
          r = builtins.toFile "oroshi-refusal-r-''${nonce}" "''${d}\n";
        }
      '';
    };

    host = {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = [ cacheUrl ];
        trusted-public-keys = [ templateKey ];
        builders = pkgs.lib.mkForce null;
        max-jobs = 0;
        fallback = false;
        netrc-file = "/etc/nix/netrc";
      };
      environment.systemPackages = [ pkgs.curl ];
    };
  };

  testScript = ''
    import hashlib
    import json
    import shlex
    import time

    empty = shlex.quote(str())
    start_all()

    with subtest("provision VM-only signing keys and write token"):
        builder.succeed("umask 077; nix key generate-secret --key-name builder.example.org-1 > /root/builder.sec")
        builder.succeed("umask 077; nix key generate-secret --key-name second.example.org-1 > /root/second.sec")
        builder.succeed("umask 077; nix key generate-secret --key-name upstream.example.org-1 > /root/upstream.sec")
        builder.succeed("umask 077; nix key generate-secret --key-name untrusted.example.org-1 > /root/untrusted.sec")
        upstream_key = builder.succeed("nix key convert-secret-to-public < /root/upstream.sec").strip()
        public_key = builder.succeed("nix key convert-secret-to-public < /root/builder.sec").strip()
        token = builder.succeed("head -c 32 /dev/urandom | base64 -w0").strip()
        token_hash = hashlib.sha256(token.encode()).hexdigest()
        builder.succeed("umask 077; printf %s " + shlex.quote(
            f"machine cache login builder password {token}\n"
        ) + " > /root/cache.netrc")
        template = cache.succeed("cat /etc/oroshi/oroshi.toml")
        max_bytes_line = "max_bytes = ${toString templateMaxBytes}"
        for placeholder in ("${templateKey}", "${upstreamTemplateKey}", "${templateHash}", max_bytes_line):
            assert template.count(placeholder) == 1, (placeholder, template)

        def start_cache(trust_upstream=False, max_bytes=${toString templateMaxBytes}):
            config = template.replace("${templateKey}", public_key).replace("${templateHash}", token_hash)
            if trust_upstream:
                config = config.replace("${upstreamTemplateKey}", upstream_key)
            config = config.replace(max_bytes_line, f"max_bytes = {max_bytes}")
            cache.succeed("printf %s " + shlex.quote(config) + " > /run/oroshi-test.toml")
            cache.succeed("systemctl restart oroshi.service")
            cache.wait_for_unit("oroshi.service")
            cache.wait_for_open_port(8080)

        start_cache()
        host_config = host.succeed("cat /etc/nix/nix.conf").replace("${templateKey}", public_key)
        host.succeed("printf %s " + shlex.quote(host_config) + " > /run/nix-test.conf")
        host.succeed("cp --remove-destination /run/nix-test.conf /etc/nix/nix.conf")
        host.succeed("systemctl restart nix-daemon.service")
        builder.succeed("systemctl restart nix-daemon.service")

    def narinfo(path):
        store_hash = path.removeprefix("/nix/store/").split("-", 1)[0]
        return host.succeed(f"curl -fsS ${cacheUrl}/{store_hash}.narinfo")

    def signatures(info):
        return {line for line in info.splitlines() if line.startswith("Sig: ")}

    with subtest("local builds sign and the post-build-hook uploads in dependency order"):
        nonce = builder.succeed("cat /proc/sys/kernel/random/uuid").strip()
        instantiate = f"nix-instantiate /etc/oroshi-closure.nix --argstr nonce {nonce}"
        drv_a = builder.succeed(instantiate + " -A a").strip()
        drv_b = builder.succeed(instantiate + " -A b").strip()
        a = builder.succeed(f"nix-store -q --outputs {drv_a}").strip()
        b = builder.succeed(f"nix-store -q --outputs {drv_b}").strip()
        for path in (a, b):
            builder.fail(f"nix-store --check-validity {path}")
            host.fail(f"nix-store --check-validity {path}")
        builder.succeed(f"nix-store --realise {drv_a} --option substituters {empty} --option builders {empty}")
        assert "built-a-in-vm" in builder.succeed(f"nix-store --read-log {drv_a}")
        assert "built-b-in-vm" in builder.succeed(f"nix-store --read-log {drv_b}")
        uploaded = builder.succeed("cat /root/uploaded-paths").splitlines()
        assert uploaded == [b, a], uploaded
        assert set(builder.succeed(f"nix-store -qR {a}").splitlines()) == {a, b}
        initial = {path: narinfo(path) for path in (b, a)}
        for path, info in initial.items():
            assert f"StorePath: {path}\n" in info, info
            assert "Compression: zstd\n" in info, info
            sigs = signatures(info)
            assert len(sigs) == 1 and next(iter(sigs)).startswith("Sig: builder.example.org-1:"), info
            out = host.succeed(f"nix path-info --store ${cacheUrl} --sigs {path}")
            assert "builder.example.org-1:" in out, out
        assert f"References: {b.removeprefix('/nix/store/')}\n" in initial[a], initial[a]

    with subtest("host substitutes both paths anonymously without any build"):
        host.succeed("test ! -e /root/.netrc && test ! -e /etc/nix/netrc")
        for drv in (drv_a, drv_b):
            host.succeed(f"test ! -e {drv}")
        out = host.succeed(
            f"nix-store --realise {a} --option substituters ${cacheUrl} "
            f"--option builders {empty} --option max-jobs 0 --option fallback false 2>&1"
        )
        for path in (a, b):
            assert f"copying path '{path}' from '${cacheUrl}'" in out, out
        assert set(host.succeed(f"nix-store -qR {a}").splitlines()) == {a, b}
        assert host.succeed(f"cat {b}") == "dependency\n"
        assert host.succeed(f"cat {a}") == f"dependency\n{b}\n"

    with subtest("repeat copy is idempotent"):
        blob_snapshot = "find /var/lib/oroshi/blobs -type f -exec sha256sum {} + | sort"
        blobs_before = cache.succeed(blob_snapshot)
        assert len(blobs_before.splitlines()) == 2, blobs_before
        out = builder.succeed(f"nix copy --to '${uploadUrl}' {a} 2>&1")
        assert "copying 0 paths" in out, out
        assert {path: narinfo(path) for path in (b, a)} == initial
        assert cache.succeed(blob_snapshot) == blobs_before

    with subtest("remote signing merges a second signature and preserves metadata"):
        builder.succeed(f"nix store sign --store ${cacheUrl} --key-file /root/second.sec {a}")
        merged = narinfo(a)
        sigs = signatures(merged)
        assert len(sigs) == 2 and signatures(initial[a]) < sigs, merged
        assert any(sig.startswith("Sig: second.example.org-1:") for sig in sigs), merged
        assert [line for line in merged.splitlines() if not line.startswith("Sig: ")] == [
            line for line in initial[a].splitlines() if not line.startswith("Sig: ")
        ], merged
        assert narinfo(b) == initial[b]
        assert cache.succeed(blob_snapshot) == blobs_before
        out = host.succeed(f"nix path-info --store ${cacheUrl} --option narinfo-cache-positive-ttl 0 --sigs {a}")
        assert "builder.example.org-1:" in out and "second.example.org-1:" in out, out

    journal = "journalctl _SYSTEMD_UNIT=oroshi.service -o cat --no-pager"
    refusal_events = ("upload-refused", "auth-refused")
    netrc = "--netrc-file /root/cache.netrc"
    # Every file a refused upload could leave behind: blobs and partial temp files.
    state_files = "find /var/lib/oroshi/blobs /var/lib/oroshi/tmp -type f -exec sha256sum {} + | sort"

    def events():
        cache.succeed("journalctl --sync")
        return [json.loads(line) for line in cache.succeed(journal).splitlines()]

    def wait_event(seen, event, **fields):
        for _ in range(20):
            new = events()[seen:]
            if any(e["event"] == event and all(e.get(k) == v for k, v in fields.items()) for e in new):
                return new
            time.sleep(0.5)
        raise AssertionError(f"no {event} {fields} in {new}")

    def status(target):
        return builder.succeed(f"curl -sS -o /dev/null -w '%{{http_code}}' ${cacheUrl}/{target}")

    def put(target, body, auth=netrc):
        # Expect lets early refusals answer before the body is sent.
        return builder.succeed(
            f"curl -sS -o /dev/null -w '%{{http_code}}' -H 'Expect: 100-continue' {auth} -T {body} ${cacheUrl}/{target}"
        )

    def refused(code, target, body, event, auth=netrc, **fields):
        seen = len(events())
        files = cache.succeed(state_files)
        assert put(target, body, auth) == str(code), (target, code)
        new = wait_event(seen, event, module="nix", **fields)
        assert sum(e["event"] in refusal_events for e in new) == 1, new
        assert cache.succeed(state_files) == files, target

    def store_hash(path):
        return path.removeprefix("/nix/store/").split("-", 1)[0]

    def metric(series):
        out = cache.succeed("curl -fsS ${metrics}")
        samples = [line.removeprefix(series + " ") for line in out.splitlines() if line.startswith(series + " ")]
        assert len(samples) == 1, (series, out)
        return int(samples[0])

    with subtest("a closure mixing an upstream-signed and a built path needs both keys"):
        builder.succeed(f"echo upstream-{nonce} > /root/upstream-content")
        u = builder.succeed(
            f"nix store add-file --store file:///root/upstream --name oroshi-upstream-{nonce} /root/upstream-content"
        ).strip()
        builder.succeed(f"nix store sign --store file:///root/upstream --key-file /root/upstream.sec {u}")
        builder.succeed(f"nix copy --from file:///root/upstream {u}")
        local_sigs = builder.succeed(f"nix path-info --sigs {u}")
        assert "upstream.example.org-1:" in local_sigs and "builder.example.org-1:" not in local_sigs, local_sigs
        seen = len(events())
        builder.fail(f"nix copy --to '${uploadUrl}' {u}")
        wait_event(seen, "upload-refused", module="nix", kind="narinfo", reason="400 Bad Request")
        assert status(f"{store_hash(u)}.narinfo") == "404"
        start_cache(trust_upstream=True)
        drv_m = builder.succeed(f"nix-instantiate /etc/oroshi-mixed.nix --argstr upstream {u}").strip()
        m = builder.succeed(f"nix-store -q --outputs {drv_m}").strip()
        builder.succeed(f"nix-store --realise {drv_m} --option substituters {empty} --option builders {empty}")
        assert builder.succeed("tail -n 1 /root/uploaded-paths").strip() == m
        assert set(builder.succeed(f"nix-store -qR {m}").splitlines()) == {m, u}
        info_u, info_m = narinfo(u), narinfo(m)
        # oroshi adds no signature: each path keeps only its producer's.
        assert [s.split(":")[1].strip() for s in signatures(info_u)] == ["upstream.example.org-1"], info_u
        assert [s.split(":")[1].strip() for s in signatures(info_m)] == ["builder.example.org-1"], info_m
        assert f"References: {u.removeprefix('/nix/store/')}\n" in info_m, info_m

    with subtest("prepare an added closure and its narinfo variants"):
        paths = json.loads(builder.succeed(
            f"nix-instantiate --eval --strict --json --read-write-mode /etc/oroshi-refusal.nix --argstr nonce {nonce}"
        ))
        d, r = paths["d"], paths["r"]
        assert d.removeprefix("/nix/store/") in builder.succeed(f"nix-store -q --references {r}")
        builder.succeed(f"nix copy --to file:///root/fresh {d} {r}")
        for key in ("untrusted", "builder"):
            builder.succeed(f"nix store sign --store file:///root/fresh --key-file /root/{key}.sec {d} {r}")
        variants = {}
        for name, path in (("d", d), ("r", r)):
            info = builder.succeed(f"cat /root/fresh/{store_hash(path)}.narinfo")
            base = [line for line in info.splitlines() if not line.startswith("Sig: ")]
            sig = {line.split(":")[1].strip(): line for line in signatures(info)}
            nar = next(line.removeprefix("URL: ") for line in base if line.startswith("URL: "))
            variants[name] = {
                "nar": nar,
                "unsigned": base,
                "untrusted": base + [sig["untrusted.example.org-1"]],
                "trusted": base + [sig["builder.example.org-1"]],
            }
            for kind in ("unsigned", "untrusted", "trusted"):
                text = "\n".join(variants[name][kind]) + "\n"
                builder.succeed("printf %s " + shlex.quote(text) + f" > /root/{name}-{kind}.narinfo")
        nar_d, nar_r = variants["d"]["nar"], variants["r"]["nar"]
        # The conflicting variant changes only NarHash.
        nar_hash_r = next(line for line in variants["r"]["unsigned"] if line.startswith("NarHash: "))
        conflict = [nar_hash_r if line.startswith("NarHash: ") else line for line in variants["d"]["trusted"]]
        assert conflict != variants["d"]["trusted"]
        builder.succeed("printf %s " + shlex.quote("\n".join(conflict) + "\n") + " > /root/d-conflict.narinfo")
        for path in (d, r):
            assert status(f"{store_hash(path)}.narinfo") == "404"

    with subtest("writes without a nix:write token are refused before any storage"):
        out = builder.succeed(
            f"curl -sS -D - -o /dev/null -H 'Expect: 100-continue' -T /root/fresh/{nar_d} ${cacheUrl}/{nar_d}"
        )
        assert "HTTP/1.1 401 " in out, out
        headers = {k.strip().lower(): v.strip() for k, v in (h.split(":", 1) for h in out.splitlines() if ":" in h)}
        assert headers["www-authenticate"] == 'Basic realm="oroshi"', out
        refused(401, nar_d, f"/root/fresh/{nar_d}", "auth-refused", auth=str(), token="unknown")
        refused(401, nar_d, f"/root/fresh/{nar_d}", "auth-refused", auth="-H 'Authorization: Bearer not-a-token'", token="unknown")
        refused(403, nar_d, f"/root/fresh/{nar_d}", "auth-refused", auth="-H 'Authorization: Bearer ${readerToken}'", token="reader")
        assert status(nar_d) == "404"

    with subtest("NAR hash mismatch and oversize NARs leave no blob"):
        refused(400, nar_r, f"/root/fresh/{nar_d}", "upload-refused", kind="nar", reason="400 Bad Request")
        builder.succeed("head -c ${toString (maxNarBytes + 1)} /dev/urandom > /root/oversize")
        oversize = builder.succeed("nix-hash --type sha256 --flat --base32 /root/oversize").strip()
        refused(413, f"nar/{oversize}.nar", "/root/oversize", "upload-refused", kind="nar", reason="413 Payload Too Large")
        assert status(nar_r) == "404" and status(f"nar/{oversize}.nar") == "404"

    with subtest("narinfos are refused without their NAR, trusted signature or references"):
        def refused_narinfo(code, path, variant):
            refused(code, f"{store_hash(path)}.narinfo", f"/root/{variant}.narinfo", "upload-refused",
                    kind="narinfo", reason="400 Bad Request" if code == 400 else "409 Conflict")
        refused_narinfo(400, d, "d-trusted")
        assert status(f"{store_hash(d)}.narinfo") == "404"
        for name in ("d", "r"):
            nar = variants[name]["nar"]
            assert put(nar, f"/root/fresh/{nar}") == "204"
        for variant in ("d-unsigned", "d-untrusted"):
            refused_narinfo(400, d, variant)
            assert status(f"{store_hash(d)}.narinfo") == "404"
        refused_narinfo(400, r, "r-trusted")
        assert status(f"{store_hash(r)}.narinfo") == "404"

    with subtest("the same narinfos commit once valid, and a conflicting one is 409"):
        for name, path in (("d", d), ("r", r)):
            assert put(f"{store_hash(path)}.narinfo", f"/root/{name}-trusted.narinfo") == "200"
            assert narinfo(path) == "\n".join(variants[name]["trusted"]) + "\n"
        committed_d = narinfo(d)
        refused_narinfo(409, d, "d-conflict")
        assert narinfo(d) == committed_d

    with subtest("nix-cache-info PUT is ignored and other PUTs are forbidden"):
        cache_info = builder.succeed("curl -fsS ${cacheUrl}/nix-cache-info")
        builder.succeed("printf 'StoreDir: /nix/store\\n' > /root/nix-cache-info")
        assert put("nix-cache-info", "/root/nix-cache-info") == "204"
        assert builder.succeed("curl -fsS ${cacheUrl}/nix-cache-info") == cache_info
        refused(403, "log/oroshi", "/root/nix-cache-info", "upload-refused", kind="other", reason="403 Forbidden")

    with subtest("upload and refusal metrics moved and validate"):
        cache.succeed("${validateMetrics} validate --metrics ${metrics}")
        assert metric('oroshi_uploads_total{module="nix",result="committed"}') > 0
        assert metric('oroshi_uploads_total{module="nix",result="refused"}') >= 8
        assert metric('oroshi_auth_refusals_total{module="nix"}') >= 3

    with subtest("a restarted cache still serves and substitutes every path, and enforces its cap"):
        served = (a, b, u, m, d, r)
        before = {path: narinfo(path) for path in served}
        files = cache.succeed(state_files)
        stored = metric("oroshi_stored_bytes")
        pid = cache.succeed("systemctl show -P MainPID oroshi.service").strip()
        start_cache(trust_upstream=True, max_bytes=stored)
        assert cache.succeed("systemctl show -P MainPID oroshi.service").strip() != pid
        assert {path: narinfo(path) for path in served} == before
        for info in before.values():
            nar = next(line.removeprefix("URL: ") for line in info.splitlines() if line.startswith("URL: "))
            assert status(nar) == "200", nar
        assert cache.succeed(state_files) == files
        host.succeed(f"nix-store --delete --ignore-liveness {a} {b}")
        out = host.succeed(
            f"nix-store --realise {a} {r} --option substituters ${cacheUrl} --option narinfo-cache-positive-ttl 0 "
            f"--option builders {empty} --option max-jobs 0 --option fallback false 2>&1"
        )
        for path in (a, b, r, d):
            assert f"copying path '{path}' from '${cacheUrl}'" in out, out
        assert host.succeed(f"cat {a}") == f"dependency\n{b}\n"
        assert host.succeed(f"cat {r}") == f"{d}\n"
        builder.succeed("head -c 512 /dev/urandom > /root/over-cap")
        over_cap = builder.succeed("nix-hash --type sha256 --flat --base32 /root/over-cap").strip()
        refused(507, f"nar/{over_cap}.nar", "/root/over-cap", "upload-refused", kind="nar", reason="507 Insufficient Storage")

    with subtest("every cache log line and the metrics endpoint still validate"):
        cache.succeed("${validateMetrics} validate --metrics ${metrics}")
        names = {e["event"] for e in events()}
        assert {"upload-committed", "upload-refused", "auth-refused"} <= names, names
        pipeline = journal + " | ${validateLogs} validate --logs"
        cache.succeed(f"bash -o pipefail -c {shlex.quote(pipeline)}")
  '';
}
