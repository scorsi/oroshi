# O1 task 17: the internal TLS boundary (ADRs 0006, 0008, 0010, 0012 and 0014).
# `cache` runs oroshi on loopback behind nginx: a TLS internal listener on the
# internal network (vlan 1) and a public listener with no route to oroshi
# (vlan 2). `client` is an internal peer; `outside` sits on the public network.
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
  # Test driver addressing: 192.168.<vlan>.<node number>, nodes numbered by name.
  internalAddress = "192.168.1.1";
  publicAddress = "192.168.2.1";
  cacheUrl = "https://oroshi.example.org/nix";
  signingKey = ./fixtures/tls-boundary/signing.sec;
  publicKey = pkgs.lib.fileContents ./fixtures/tls-boundary/signing.pub;
  token = "oroshi-tls-boundary-write-token";
  tls = pkgs.runCommand "oroshi-tls-boundary-certs" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir "$out" && cd "$out"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
      -subj /CN=oroshi-test-ca -keyout ca.key -out ca.pem
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj /CN=oroshi.example.org -keyout server.key -out server.csr
    printf 'subjectAltName=DNS:oroshi.example.org\n' > ext
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 \
      -extfile ext -out server.pem
  '';
  peer = {
    security.pki.certificateFiles = [ "${tls}/ca.pem" ];
    networking.hosts.${internalAddress} = [ "oroshi.example.org" ];
    environment.systemPackages = [ pkgs.curl ];
  };
in
pkgs.testers.runNixOSTest {
  name = "oroshi-tls-boundary";

  nodes = {
    cache = {
      imports = [ self.nixosModules.default ];
      virtualisation.vlans = [
        1
        2
      ];
      services.oroshi = {
        enable = true;
        # Default listen: 127.0.0.1:8080, reachable only by the co-located proxy.
        metrics.listen = "127.0.0.1:9000";
        storage.maxBytes = 1073741824;
        tokens.uploader = {
          sha256 = builtins.hashString "sha256" token;
          scopes = [ "nix:write" ];
        };
        nix.trustedKeys = [ publicKey ];
      };
      services.nginx = {
        enable = true;
        # HTTP/1.1 upstream and a 60 s proxy timeout, matching ADR 0010's idle timeout.
        recommendedProxySettings = true;
        virtualHosts.internal = {
          serverName = "oroshi.example.org";
          listen = [
            {
              addr = internalAddress;
              port = 443;
              ssl = true;
            }
          ];
          onlySSL = true;
          sslCertificate = "${tls}/server.pem";
          sslCertificateKey = "${tls}/server.key";
          locations."/".proxyPass = "http://127.0.0.1:8080";
          # ADR 0010: stream bodies without a total size limit.
          extraConfig = ''
            client_max_body_size 0;
            proxy_request_buffering off;
            proxy_buffering off;
          '';
        };
        virtualHosts.public = {
          serverName = "oroshi.example.org";
          listen = [
            {
              addr = publicAddress;
              port = 80;
            }
          ];
          locations."/".return = "404";
        };
      };
      # nginx binds the vlan addresses, which may come up after it starts.
      boot.kernel.sysctl."net.ipv4.ip_nonlocal_bind" = 1;
      # The firewall, per interface, is the network boundary: eth1 internal, eth2 public.
      networking.firewall.interfaces.eth1.allowedTCPPorts = [ 443 ];
      networking.firewall.interfaces.eth2.allowedTCPPorts = [ 80 ];
      environment.systemPackages = [
        pkgs.curl
        pkgs.iptables
      ];
    };

    client = {
      imports = [ peer ];
      virtualisation.vlans = [ 1 ];
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = pkgs.lib.mkForce [ ];
        trusted-public-keys = pkgs.lib.mkForce [ publicKey ];
        ssl-cert-file = "/etc/ssl/certs/ca-certificates.crt";
      };
      environment.etc."oroshi-signing.sec".source = signingKey;
      # Only uploads pass this file; reads stay anonymous.
      environment.etc."oroshi-upload.netrc".text = ''
        machine oroshi.example.org login uploader password ${token}
      '';
    };

    outside = {
      imports = [ peer ];
      virtualisation.vlans = [ 2 ];
      # A public peer that knows the internal address and routes to it through the cache.
      networking.interfaces.eth1.ipv4.routes = [
        {
          address = "192.168.1.0";
          prefixLength = 24;
          via = publicAddress;
        }
      ];
    };
  };

  testScript = ''
    import json
    import shlex

    curl = "curl -sS --connect-timeout 5 --cacert /etc/ssl/certs/ca-certificates.crt"
    start_all()
    cache.wait_for_unit("oroshi.service")
    cache.wait_for_unit("nginx.service")
    cache.wait_for_open_port(443, "${internalAddress}")
    cache.wait_for_open_port(80, "${publicAddress}")
    client.wait_for_unit("multi-user.target")
    outside.wait_for_unit("multi-user.target")

    def unreachable(node, url):
        # 7: connection refused or no route; 28: connect timeout (firewall drop).
        status, out = node.execute(f"{curl} -o /dev/null {url} 2>&1")
        assert status in (7, 28), (url, status, out)

    def code(node, args):
        return node.succeed(f"{curl} -o /dev/null -w '%{{http_code}}' {args}")

    def store_hash(path):
        return path.removeprefix("/nix/store/").split("-", 1)[0]

    def signed_path(name):
        nonce = client.succeed("cat /proc/sys/kernel/random/uuid").strip()
        client.succeed(f"echo {name}-{nonce} > /root/{name}")
        path = client.succeed(f"nix store add-file --name oroshi-{name}-{nonce} /root/{name}").strip()
        client.succeed(f"nix store sign --key-file /etc/oroshi-signing.sec {path}")
        return path

    with subtest("the plain HTTP backend listens on loopback only"):
        sockets = cache.succeed("ss -Hltn 'sport = :8080'").splitlines()
        assert sockets and all(s.split()[3] == "127.0.0.1:8080" for s in sockets), sockets

    with subtest("anonymous reads work through the internal TLS listener"):
        info = client.succeed(f"{curl} -f ${cacheUrl}/nix-cache-info")
        assert "StoreDir: /nix/store\n" in info, info

    with subtest("authenticated writes work through TLS and anonymous writes are refused"):
        p = signed_path("uploaded")
        nar = "nar/" + "0" * 52 + ".nar"
        assert code(client, f"-X PUT -H 'Expect: 100-continue' -T /root/uploaded ${cacheUrl}/{nar}") == "401"
        client.succeed(
            f"nix copy --option netrc-file /etc/oroshi-upload.netrc --to '${cacheUrl}?compression=zstd' {p}"
        )
        narinfo = client.succeed(f"{curl} -f ${cacheUrl}/{store_hash(p)}.narinfo")
        assert f"StorePath: {p}\n" in narinfo and "Sig: tls-boundary.example.org-1:" in narinfo, narinfo

    with subtest("nix copy substitutes anonymously over TLS with the test CA trusted"):
        client.succeed(f"nix store delete {p}")
        client.fail(f"nix-store --check-validity {p}")
        out = client.succeed(f"nix copy --option narinfo-cache-negative-ttl 0 --from ${cacheUrl} {p} 2>&1")
        client.succeed(f"nix-store --check-validity {p}")
        assert client.succeed(f"cat {p}").startswith("uploaded-"), out

    with subtest("a miss is 404 without upstream traffic and clients use their next substituter"):
        q = signed_path("next")
        client.succeed(f"nix copy --to file:///root/next-cache {q}")
        client.succeed(f"nix store delete {q}")
        # Count every new connection oroshi opens, DNS included.
        cache.succeed("iptables -w -I OUTPUT 1 -m owner --uid-owner oroshi -m conntrack --ctstate NEW")
        assert code(client, f"${cacheUrl}/{store_hash(q)}.narinfo") == "404"
        out = client.succeed(
            f"nix-store --realise {q} --option substituters '${cacheUrl} file:///root/next-cache' 2>&1"
        )
        assert f"copying path '{q}' from 'file:///root/next-cache'" in out, out
        rule = cache.succeed("iptables -w -nvxL OUTPUT 1").split()
        assert rule[0] == "0", rule

    with subtest("internal peers cannot reach the backend or a plain listener directly"):
        unreachable(client, "http://${internalAddress}:8080/healthz")
        unreachable(client, "http://oroshi.example.org/nix/nix-cache-info")

    with subtest("the outside peer has no route to oroshi"):
        unreachable(outside, "${cacheUrl}/nix-cache-info")
        unreachable(outside, "https://${publicAddress}/nix/nix-cache-info")
        for address in ("${internalAddress}", "${publicAddress}"):
            unreachable(outside, f"http://{address}:8080/healthz")
        # The public listener is reachable but routes nothing to oroshi.
        body = outside.succeed(
            "curl -sS -H 'Host: oroshi.example.org' -w '%{http_code}' http://${publicAddress}/nix/nix-cache-info"
        )
        assert body.endswith("404") and "StoreDir" not in body, body

    with subtest("cache logs and metrics validate"):
        cache.succeed("${validateMetrics} validate --metrics ${metrics}")
        cache.succeed("journalctl --sync")
        journal = "journalctl _SYSTEMD_UNIT=oroshi.service -o cat --no-pager"
        events = {json.loads(line)["event"] for line in cache.succeed(journal).splitlines()}
        assert {"upload-committed", "auth-refused"} <= events, events
        pipeline = journal + " | ${validateLogs} validate --logs"
        cache.succeed(f"bash -o pipefail -c {shlex.quote(pipeline)}")
  '';
}
