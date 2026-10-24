# O1 task 15: stock-client round trips (ADRs 0006, 0007 and 0012).
{ self, pkgs }:
let
  cacheUrl = "http://cache:8080/nix";
  uploadUrl = "${cacheUrl}?compression=zstd";
  # Valid configuration placeholders, replaced with VM-generated credentials.
  templateKey = builtins.readFile ../crates/oroshi-nix/tests/fixtures/narinfo/test.example.org-1.pub;
  templateHash = builtins.hashString "sha256" "oroshi-test-placeholder";
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
        tokens.builder = {
          sha256 = templateHash;
          scopes = [ "nix:write" ];
        };
        nix.trustedKeys = [ templateKey ];
      };
      networking.firewall.allowedTCPPorts = [ 8080 ];
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
      environment.systemPackages = [ pkgs.bash ];
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
    import shlex

    empty = shlex.quote(str())
    start_all()

    with subtest("provision VM-only signing keys and write token"):
        builder.succeed("umask 077; nix key generate-secret --key-name builder.example.org-1 > /root/builder.sec")
        builder.succeed("umask 077; nix key generate-secret --key-name second.example.org-1 > /root/second.sec")
        public_key = builder.succeed("nix key convert-secret-to-public < /root/builder.sec").strip()
        token = builder.succeed("head -c 32 /dev/urandom | base64 -w0").strip()
        token_hash = hashlib.sha256(token.encode()).hexdigest()
        builder.succeed("umask 077; printf %s " + shlex.quote(
            f"machine cache login builder password {token}\n"
        ) + " > /root/cache.netrc")
        config = cache.succeed("cat /etc/oroshi/oroshi.toml")
        assert "${templateKey}" in config and "${templateHash}" in config, config
        config = config.replace("${templateKey}", public_key).replace("${templateHash}", token_hash)
        cache.succeed("printf %s " + shlex.quote(config) + " > /run/oroshi-test.toml")
        cache.succeed("systemctl start oroshi.service")
        cache.wait_for_unit("oroshi.service")
        cache.wait_for_open_port(8080)
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
  '';
}
