# O0 task 8: the live service contract of ADRs 0011 and 0012.
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
  health = "http://127.0.0.1:8080/healthz";
  metrics = "http://127.0.0.1:9000/metrics";
in
pkgs.testers.runNixOSTest {
  name = "oroshi-service";

  nodes.machine = {
    imports = [ self.nixosModules.default ];
    services.oroshi = {
      enable = true;
      inherit stateDir;
      metrics.listen = "127.0.0.1:9000";
      storage.maxBytes = 1073741824;
    };
    environment.systemPackages = [ pkgs.curl ];
  };

  testScript = ''
    import json
    import shlex

    machine.wait_for_unit("oroshi.service")

    with subtest("notify readiness and service properties"):
        for prop, expected in {
            "Type": "notify",
            "ActiveState": "active",
            "LogRateLimitBurst": "30000",
            "ProtectSystem": "strict",
            "User": "oroshi",
        }.items():
            actual = machine.succeed(f"systemctl show -P {prop} oroshi.service").strip()
            assert actual == expected, f"{prop}: {actual!r} != {expected!r}"

    with subtest("health returns 200 with an empty body"):
        assert machine.succeed("curl -fsS -w '%{http_code}' ${health}") == "200"

    def requests_total():
        out = machine.succeed("curl -fsS ${metrics}")
        prefix = 'oroshi_requests_total{module="other",method="GET",code="2xx"} '
        samples = [line[len(prefix):] for line in out.splitlines() if line.startswith(prefix)]
        assert len(samples) == 1, out
        return int(samples[0])

    with subtest("live metrics validate and count a health request"):
        machine.succeed("${validateMetrics} validate --metrics ${metrics}")
        before = requests_total()
        assert machine.succeed("curl -fsS -w '%{http_code}' ${health}") == "200"
        assert requests_total() == before + 1

    with subtest("state remains writable by oroshi and survives a restart"):
        machine.succeed("su -s /bin/sh oroshi -c 'echo persistent > ${stateDir}/service-test-marker'")
        pid = machine.succeed("systemctl show -P MainPID oroshi.service").strip()
        machine.succeed("systemctl restart oroshi.service")
        machine.wait_for_unit("oroshi.service")
        assert machine.succeed("systemctl show -P MainPID oroshi.service").strip() != pid
        assert machine.succeed("curl -fsS -w '%{http_code}' ${health}") == "200"
        assert machine.succeed("su -s /bin/sh oroshi -c 'cat ${stateDir}/service-test-marker'").strip() == "persistent"

    with subtest("startup events exist and every service log line validates"):
        machine.succeed("journalctl --sync")
        # Select process output: -u also includes systemd's non-JSON status messages.
        journal = "journalctl _SYSTEMD_UNIT=oroshi.service -o cat --no-pager"
        out = machine.succeed(journal)
        events = {json.loads(line)["event"] for line in out.splitlines()}
        assert {"start", "listening", "metrics-listening", "ready"} <= events, out
        pipeline = journal + " | ${validateLogs} validate --logs"
        machine.succeed(f"bash -o pipefail -c {shlex.quote(pipeline)}")
  '';
}
