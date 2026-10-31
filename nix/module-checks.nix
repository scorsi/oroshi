{
  inputs,
  self,
  pkgs,
  oroshi-server,
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
            package = if native then oroshi-server else self.packages.${linux}.oroshi-server;
            listen = "127.0.0.1:8081";
            metrics.listen = "127.1.2.3:9000";
            stateDir = "/var/lib/oroshi-example";
            storage = {
              maxBytes = 2048;
              gcTargetRatio = 0.8;
            };
            gc = {
              schedule = "*-*-* 03:00:00";
              orphanGrace = 7200;
            };
            retention.days = 7;
            tokens.builder = {
              sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
              scopes = [
                "nix:write"
                "nix:pin"
              ];
            };
            nix = {
              priority = 20;
              trustedKeys = [ "example.org:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" ];
              maxNarBytes = 1024;
            };
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
  gcUnit = evaluated.systemd.services.oroshi-gc;
  withoutRole = lib.flip builtins.removeAttrs [
    "Type"
    "ExecStart"
    "Restart"
  ];
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
      storage = {
        maxBytes = lib.mkForce null;
        gcTargetRatio = lib.mkForce 0.9;
      };
      gc = {
        schedule = lib.mkForce "daily";
        orphanGrace = lib.mkForce 3600;
      };
      retention.days = lib.mkForce 30;
      tokens = lib.mkForce { };
      nix = {
        priority = lib.mkForce 30;
        trustedKeys = lib.mkForce [ ];
        maxNarBytes = lib.mkForce 17179869184;
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
    assert !(assertionsPass { services.oroshi.listen = lib.mkForce "0.0.0.0:8080"; });
    assert !(assertionsPass { services.oroshi.settings.server.listen = "192.0.2.1:8080"; });
    assert assertionsPass { services.oroshi.listen = lib.mkForce "[::1]:8080"; };
    # ADR 0027: maxBytes is optional.
    assert assertionsPass { services.oroshi.storage.maxBytes = lib.mkForce null; };
    # ADR 0030: the units run the server binary.
    assert lib.hasPrefix "${lib.getExe evaluated.services.oroshi.package} serve "
      unit.serviceConfig.ExecStart;
    assert lib.hasSuffix "/bin/oroshi-server" (lib.getExe evaluated.services.oroshi.package);
    assert gcUnit.serviceConfig.Type == "oneshot";
    assert
      gcUnit.serviceConfig.ExecStart == "${lib.getExe evaluated.services.oroshi.package} gc --config ${
        evaluated.environment.etc."oroshi/oroshi.toml".source
      }";
    assert withoutRole gcUnit.serviceConfig == withoutRole unit.serviceConfig;
    assert gcUnit.after == [ "oroshi.service" ] && gcUnit.wantedBy == [ ];
    assert evaluated.systemd.timers.oroshi-gc.wantedBy == [ "timers.target" ];
    assert
      evaluated.systemd.timers.oroshi-gc.timerConfig == {
        OnCalendar = "*-*-* 03:00:00";
        Persistent = true;
      };
    assert defaults.systemd.timers.oroshi-gc.timerConfig.OnCalendar == "daily";
    assert assertionsPass {
      services.oroshi.storage.maxBytes = lib.mkForce null;
      services.oroshi.settings.storage.max_bytes = 4096;
    };
    assert
      unit.serviceConfig.IPAddressAllow == "localhost" && unit.serviceConfig.IPAddressDeny == "any";
    assert unit.serviceConfig.LimitCORE == 0 && unit.serviceConfig.LimitNOFILE == 8192;
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
    ${lib.getExe oroshi-server} config check --config ${configFile { }}
    cmp ${configFile { }} ${
      (pkgs.formats.toml { }).generate "expected-oroshi.toml" {
        server.listen = "127.0.0.1:8081";
        metrics.listen = "127.1.2.3:9000";
        storage = {
          dir = "/var/lib/oroshi-example";
          max_bytes = 2048;
          gc_target_ratio = 0.8;
        };
        gc.orphan_grace = 7200;
        retention.days = 7;
        tokens.builder = {
          sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
          scopes = [
            "nix:write"
            "nix:pin"
          ];
        };
        nix = {
          priority = 20;
          trusted_keys = [ "example.org:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" ];
          max_nar_bytes = 1024;
        };
        log = {
          level = "debug";
          access = true;
        };
      }
    }
    cmp ${
      configFile {
        services.oroshi.settings = {
          log.level = "warn";
          storage.max_bytes = 4096;
        };
      }
    } ${
      (pkgs.formats.toml { }).generate "raw-oroshi.toml" {
        server.listen = "127.0.0.1:8081";
        metrics.listen = "127.1.2.3:9000";
        storage = {
          dir = "/var/lib/oroshi-example";
          max_bytes = 4096;
          gc_target_ratio = 0.8;
        };
        gc.orphan_grace = 7200;
        retention.days = 7;
        tokens.builder = {
          sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
          scopes = [
            "nix:write"
            "nix:pin"
          ];
        };
        nix = {
          priority = 20;
          trusted_keys = [ "example.org:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" ];
          max_nar_bytes = 1024;
        };
        log = {
          level = "warn";
          access = true;
        };
      }
    }
    cmp ${configFile defaultOptions} ${(pkgs.formats.toml { }).generate "default-oroshi.toml" { }}
    touch "$out"
  '';

  module-config-token-hash = pkgs.testers.testBuildFailure (configFile {
    services.oroshi.tokens.builder.sha256 = lib.mkForce "malformed";
  });

  module-config-token-scope = pkgs.testers.testBuildFailure (configFile {
    services.oroshi.tokens.builder.scopes = lib.mkForce [ "nix:admin" ];
  });

  module-config-trusted-key = pkgs.testers.testBuildFailure (configFile {
    services.oroshi.nix.trustedKeys = lib.mkForce [ "example.org:YQ==" ];
  });

  module-config-unknown = pkgs.testers.testBuildFailure (configFile {
    services.oroshi.settings.unexpected = true;
  });
}
