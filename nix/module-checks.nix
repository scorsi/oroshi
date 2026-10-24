{
  inputs,
  self,
  pkgs,
  oroshi,
  system,
}:
let
  inherit (pkgs) lib;
  linux = if pkgs.stdenv.hostPlatform.isLinux then system else "aarch64-linux";
  evaluate =
    native: extra:
    inputs.nixpkgs.lib.nixosSystem {
      modules = [
        self.nixosModules.default
        {
          nixpkgs.hostPlatform = if native then system else linux;
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
          system.stateVersion = "26.05";
          services.oroshi = {
            enable = true;
            package = if native then oroshi else self.packages.${linux}.oroshi;
            listen = "127.0.0.1:8081";
            metrics.listen = "127.1.2.3:9000";
            stateDir = "/var/lib/oroshi-example";
            log = {
              level = "debug";
              access = true;
            };
          };
        }
        extra
      ];
    };
  evaluated = (evaluate false { }).config;
  unit = evaluated.systemd.services.oroshi;
  configFile = extra: (evaluate true extra).config.environment.etc."oroshi/oroshi.toml".source;
  assertionsPass = extra: lib.all (a: a.assertion) (evaluate false extra).config.assertions;
  defaultOptions = {
    services.oroshi = {
      listen = lib.mkForce "127.0.0.1:8080";
      metrics.listen = lib.mkForce null;
      stateDir = lib.mkForce "/var/lib/oroshi";
      log = {
        level = lib.mkForce "info";
        access = lib.mkForce false;
      };
      settings = lib.mkForce { };
    };
  };
  defaults = (evaluate false defaultOptions).config;
  rawState =
    (evaluate false {
      services.oroshi.settings.storage.dir = "/var/lib/oroshi-raw";
    }).config;
in
{
  # As in masu, discard the Linux toplevel context so Darwin never builds it.
  module-eval =
    assert assertionsPass { };
    assert unit.serviceConfig.Type == "notify";
    assert unit.serviceConfig.User == "oroshi" && unit.serviceConfig.Group == "oroshi";
    assert evaluated.users.users.oroshi.isSystemUser;
    assert evaluated.users.users.oroshi.group == "oroshi";
    assert unit.serviceConfig.ProtectSystem == "strict";
    assert unit.serviceConfig.PrivateTmp && unit.serviceConfig.NoNewPrivileges;
    assert unit.serviceConfig.CapabilityBoundingSet == "";
    assert
      unit.serviceConfig.RestrictAddressFamilies == [
        "AF_UNIX"
        "AF_INET"
        "AF_INET6"
      ];
    assert unit.serviceConfig.UMask == "0027";
    assert unit.serviceConfig.LogRateLimitIntervalSec == "30s";
    assert unit.serviceConfig.LogRateLimitBurst == 30000;
    assert unit.serviceConfig.ReadWritePaths == evaluated.services.oroshi.persistPaths;
    assert evaluated.services.oroshi.persistPaths == [ "/var/lib/oroshi-example" ];
    assert unit.restartTriggers == [ evaluated.environment.etc."oroshi/oroshi.toml".source ];
    assert defaults.services.oroshi.metrics.listen == null;
    assert defaults.systemd.services.oroshi.serviceConfig.StateDirectory == "oroshi";
    assert defaults.systemd.services.oroshi.serviceConfig.StateDirectoryMode == "0750";
    assert rawState.services.oroshi.persistPaths == [ "/var/lib/oroshi-raw" ];
    assert rawState.systemd.services.oroshi.serviceConfig.ReadWritePaths == [ "/var/lib/oroshi-raw" ];
    assert !(assertionsPass { services.oroshi.settings.storage.dir = "relative"; });
    assert
      (evaluate false {
        systemd.services.oroshi.serviceConfig = {
          LogRateLimitIntervalSec = "60s";
          LogRateLimitBurst = 100;
        };
      }).config.systemd.services.oroshi.serviceConfig.LogRateLimitBurst == 100;
    pkgs.writeText "oroshi-module-eval" (
      builtins.unsafeDiscardStringContext evaluated.system.build.toplevel.drvPath
    );

  module-metrics-refusal =
    assert lib.all
      (
        address:
        !(assertionsPass {
          services.oroshi.metrics.listen = lib.mkForce address;
        })
      )
      [
        "0.0.0.0:9000"
        "192.0.2.1:9000"
        "[::]:9000"
        "[::10]:9000"
        "[2001:db8::1]:9000"
      ];
    assert !(assertionsPass { services.oroshi.settings.metrics.listen = "0.0.0.0:9000"; });
    assert lib.all
      (
        address:
        assertionsPass {
          services.oroshi.metrics.listen = lib.mkForce address;
        }
      )
      [
        "[::1]:9000"
        "[0:0::1]:9000"
        "[0:0:0:0:0:0:0:1]:9000"
      ];
    pkgs.runCommand "oroshi-module-metrics-refusal" { } ''touch "$out"'';

  module-config = pkgs.runCommand "oroshi-module-config" { } ''
    ${lib.getExe oroshi} config check --config ${configFile { }}
    cmp ${configFile { }} ${
      (pkgs.formats.toml { }).generate "expected-oroshi.toml" {
        server.listen = "127.0.0.1:8081";
        metrics.listen = "127.1.2.3:9000";
        storage.dir = "/var/lib/oroshi-example";
        log = {
          level = "debug";
          access = true;
        };
      }
    }
    cmp ${configFile { services.oroshi.settings.log.level = "warn"; }} ${
      (pkgs.formats.toml { }).generate "raw-oroshi.toml" {
        server.listen = "127.0.0.1:8081";
        metrics.listen = "127.1.2.3:9000";
        storage.dir = "/var/lib/oroshi-example";
        log = {
          level = "warn";
          access = true;
        };
      }
    }
    cmp ${configFile defaultOptions} ${(pkgs.formats.toml { }).generate "default-oroshi.toml" { }}
    touch "$out"
  '';

  module-config-unknown = pkgs.testers.testBuildFailure (configFile {
    services.oroshi.settings.unexpected = true;
  });
}
