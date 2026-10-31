# O2 task 12: retention, roots and blob sweep through the real units
# (ADRs 0005, 0011, 0012 and 0028). Only the `cache` clock moves.
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
  stateDir = "/var/lib/oroshi";
  metrics = "http://127.0.0.1:9000/metrics";
  cacheUrl = "http://cache:8080/nix";
  uploadUrl = "${cacheUrl}?compression=zstd";
  localUrl = "http://127.0.0.1:8081/nix";
  # Test-only key and token, as in the tls-boundary test.
  signingKey = ./fixtures/tls-boundary/signing.sec;
  publicKey = pkgs.lib.fileContents ./fixtures/tls-boundary/signing.pub;
  token = "oroshi-gc-test-builder-token";
  retentionDays = 1;
  # Past retention and the default one-hour orphan grace.
  clockMove = 3 * 86400;
  hook = pkgs.writeShellScript "oroshi-upload" ''
    set -eu
    export NIX_CONFIG="experimental-features = nix-command
    netrc-file = /root/cache.netrc"
    ${pkgs.nix}/bin/nix copy --to '${uploadUrl}' $OUT_PATHS
  '';
  # Runs outside the driver shell so the cache can start gc while the hook uploads.
  concurrentBuild = pkgs.writeShellScript "oroshi-concurrent-build" ''
    export PATH=/run/current-system/sw/bin:$PATH
    if nix-store --realise "$1" --option substituters "" --option builders ""; then
      echo ok > /root/concurrent-result
    else
      echo failed > /root/concurrent-result
    fi
  '';
in
pkgs.testers.runNixOSTest {
  name = "oroshi-gc";

  nodes = {
    cache = {
      imports = [ self.nixosModules.default ];
      services.oroshi = {
        enable = true;
        listen = "127.0.0.1:8081";
        metrics.listen = "127.0.0.1:9000";
        # No storage.maxBytes (ADR 0027): expiry alone bounds this cache.
        retention.days = retentionDays;
        tokens.builder = {
          sha256 = builtins.hashString "sha256" token;
          scopes = [
            "nix:write"
            "nix:pin"
          ];
        };
        nix.trustedKeys = [ publicKey ];
      };
      # The test moves this clock; nothing may move it back.
      services.timesyncd.enable = pkgs.lib.mkForce false;
      networking.firewall.allowedTCPPorts = [ 8080 ];
      # Stands in for the deployment's proxy: oroshi accepts loopback peers only.
      systemd.services.oroshi-relay = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${pkgs.socat}/bin/socat TCP-LISTEN:8080,fork,reuseaddr TCP:127.0.0.1:8081";
      };
      environment.systemPackages = [ pkgs.curl ];
    };

    builder = {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = [ ];
        builders = pkgs.lib.mkForce null;
        secret-key-files = [ "${signingKey}" ];
        post-build-hook = hook;
      };
      environment.systemPackages = [
        pkgs.bash
        pkgs.coreutils
        pkgs.curl
      ];
      # `a` references `b` and itself.
      environment.etc."oroshi-closure.nix".text = ''
        { nonce }:
        let
          b = builtins.derivation {
            name = "oroshi-b-''${nonce}";
            system = "${pkgs.stdenv.hostPlatform.system}";
            builder = (builtins.storePath "${pkgs.bash}") + "/bin/bash";
            args = [ "-c" "echo dependency-''${nonce} > $out" ];
          };
          a = builtins.derivation {
            name = "oroshi-a-''${nonce}";
            system = "${pkgs.stdenv.hostPlatform.system}";
            builder = (builtins.storePath "${pkgs.bash}") + "/bin/bash";
            args = [ "-c" "printf '%s\\n' ''${b} $out > $out" ];
          };
        in { inherit a b; }
      '';
      # Large enough for its upload to be in flight while gc runs.
      environment.etc."oroshi-large.nix".text = ''
        { nonce }:
        builtins.derivation {
          name = "oroshi-large-''${nonce}";
          system = "${pkgs.stdenv.hostPlatform.system}";
          builder = (builtins.storePath "${pkgs.bash}") + "/bin/bash";
          args = [ "-c" "''${builtins.storePath "${pkgs.coreutils}"}/bin/head -c 67108864 /dev/urandom > $out" ];
        }
      '';
    };

    host = {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = [ cacheUrl ];
        trusted-public-keys = [ publicKey ];
        builders = pkgs.lib.mkForce null;
        max-jobs = 0;
        fallback = false;
      };
    };
  };

  testScript = ''
    import json
    import shlex

    empty = shlex.quote(str())
    start_all()
    cache.wait_for_unit("oroshi.service")
    cache.wait_for_open_port(8080)
    builder.succeed("umask 077; printf %s " + shlex.quote(
        "machine cache login builder password ${token}\n"
    ) + " > /root/cache.netrc")

    def store_hash(path):
        return path.removeprefix("/nix/store/").split("-", 1)[0]

    def status(target):
        return cache.succeed(f"curl -sS -o /dev/null -w '%{{http_code}}' ${localUrl}/{target}")

    def nar_url(path):
        info = cache.succeed(f"curl -fsS ${localUrl}/{store_hash(path)}.narinfo")
        return next(line.removeprefix("URL: ") for line in info.splitlines() if line.startswith("URL: "))

    def metric(series):
        out = cache.succeed("curl -fsS ${metrics}")
        samples = [line.removeprefix(series + " ") for line in out.splitlines() if line.startswith(series + " ")]
        assert len(samples) == 1, (series, out)
        return float(samples[0])

    def journal(unit):
        return f"journalctl _SYSTEMD_UNIT={unit} -o cat --no-pager"

    def events(unit):
        cache.succeed("journalctl --sync")
        return [json.loads(line) for line in cache.succeed(journal(unit)).splitlines()]

    blobs = "find ${stateDir}/blobs -type f | sort"

    def blob_files():
        return set(cache.succeed(blobs).splitlines())

    def build(nonce, attr):
        drv = builder.succeed(f"nix-instantiate /etc/oroshi-closure.nix --argstr nonce {nonce} -A {attr}").strip()
        return builder.succeed(f"nix-store -q --outputs {drv}").strip(), drv

    def build_closure(nonce):
        (a, drv), (b, _) = build(nonce, "a"), build(nonce, "b")
        builder.succeed(f"nix-store --realise {drv} --option substituters {empty} --option builders {empty}")
        assert set(builder.succeed(f"nix-store -qR {a}").splitlines()) == {a, b}
        assert store_hash(a) in builder.succeed(f"nix-store -q --references {a}")
        for path in (a, b):
            assert status(f"{store_hash(path)}.narinfo") == "200", path
        return a, b

    with subtest("the gc units are installed, hardened and scheduled"):
        assert cache.succeed("systemctl show -P ActiveState oroshi-gc.timer").strip() == "active"
        assert cache.succeed("systemctl show -P Persistent oroshi-gc.timer").strip() == "yes"
        assert cache.succeed("systemctl show -P Type oroshi-gc.service").strip() == "oneshot"
        assert cache.succeed("systemctl show -P User oroshi-gc.service").strip() == "oroshi"
        cache.succeed("systemd-analyze security --threshold=30 --no-pager oroshi-gc.service")
        # The test starts gc itself; the clock move must not trigger the timer.
        cache.succeed("systemctl stop oroshi-gc.timer")

    with subtest("upload a rooted and an unrooted closure through the hook"):
        rooted_a, rooted_b = build_closure("rooted")
        builder.succeed("printf '%s\\n' " + rooted_a + " > /root/root-body")
        assert builder.succeed(
            "curl -sS -o /dev/null -w '%{http_code}' -X PUT -H 'Authorization: Bearer ${token}' "
            "--data-binary @/root/root-body ${cacheUrl}/_oroshi/roots/release"
        ) == "204"
        kept_blobs = blob_files()
        unrooted_a, unrooted_b = build_closure("unrooted")
        unrooted_nars = [nar_url(path) for path in (unrooted_a, unrooted_b)]
        expired_blobs = blob_files() - kept_blobs
        assert len(expired_blobs) == 2, expired_blobs
        assert any(e["event"] == "root-set" and e["name"] == "release" for e in events("oroshi.service"))
        assert metric("oroshi_gc_last_success_timestamp_seconds") == 0

    with subtest("move the cache clock past retention"):
        cache.succeed("date -s @$(( $(date +%s) + ${toString clockMove} ))")
        moved = int(cache.succeed("date +%s"))

    with subtest("an upload started after the clock move commits while gc runs"):
        drv_large = builder.succeed("nix-instantiate /etc/oroshi-large.nix --argstr nonce concurrent").strip()
        large = builder.succeed(f"nix-store -q --outputs {drv_large}").strip()
        builder.succeed(f"systemd-run --unit=oroshi-concurrent-build ${concurrentBuild} {drv_large}")
        # Best effort (ADR 0028): start gc once the NAR is in flight, or committed if it was faster.
        cache.succeed(
            "timeout 600 sh -c 'until ls -A ${stateDir}/tmp | grep -q . || "
            f"curl -fsS -o /dev/null ${localUrl}/{store_hash(large)}.narinfo; do sleep 0.05; done'"
        )
        cache.succeed("systemctl start oroshi-gc.service")
        builder.wait_until_succeeds("test -e /root/concurrent-result")
        assert builder.succeed("cat /root/concurrent-result").strip() == "ok", builder.succeed(
            "journalctl -u oroshi-concurrent-build --no-pager"
        )
        assert status(f"{store_hash(large)}.narinfo") == "200"
        assert status(nar_url(large)) == "200"

    with subtest("the unrooted closure and its blobs are gone, the rooted one stays"):
        for path in (unrooted_a, unrooted_b):
            assert status(f"{store_hash(path)}.narinfo") == "404", path
        for nar in unrooted_nars:
            assert status(nar) == "404", nar
        remaining = blob_files()
        assert not (expired_blobs & remaining), remaining
        assert kept_blobs <= remaining, remaining
        runs = [e for e in events("oroshi-gc.service") if e["event"] == "gc-run"]
        assert len(runs) == 1 and "err" not in runs[0], runs
        assert runs[0]["deleted_entries"] == 2 and runs[0]["deleted_blobs"] == 2, runs

    with subtest("host substitutes the rooted closure"):
        out = host.succeed(
            f"nix-store --realise {rooted_a} --option substituters ${cacheUrl} "
            f"--option builders {empty} --option max-jobs 0 --option fallback false 2>&1"
        )
        for path in (rooted_a, rooted_b):
            assert f"copying path '{path}' from '${cacheUrl}'" in out, out
        assert host.succeed(f"cat {rooted_a}") == f"{rooted_b}\n{rooted_a}\n"

    with subtest("gc metrics moved and every log line validates"):
        cache.succeed("${validateMetrics} validate --metrics ${metrics}")
        assert metric("oroshi_gc_last_success_timestamp_seconds") >= moved
        assert metric('oroshi_gc_runs_total{result="ok"}') == 1
        for unit in ("oroshi.service", "oroshi-gc.service"):
            pipeline = journal(unit) + " | ${validateLogs} validate --logs"
            cache.succeed(f"bash -o pipefail -c {shlex.quote(pipeline)}")
  '';
}
