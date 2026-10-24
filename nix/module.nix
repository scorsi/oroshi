self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.services.oroshi;
  toml = pkgs.formats.toml { };
  settings = lib.recursiveUpdate (
    lib.optionalAttrs (cfg.listen != "127.0.0.1:8080") { server.listen = cfg.listen; }
    // lib.optionalAttrs (cfg.metrics.listen != null) { metrics.listen = cfg.metrics.listen; }
    //
      lib.optionalAttrs
        (
          cfg.stateDir != "/var/lib/oroshi"
          || cfg.storage.maxBytes != null
          || cfg.storage.gcTargetRatio != 0.9
        )
        {
          storage =
            lib.optionalAttrs (cfg.stateDir != "/var/lib/oroshi") { dir = cfg.stateDir; }
            // lib.optionalAttrs (cfg.storage.maxBytes != null) { max_bytes = cfg.storage.maxBytes; }
            // lib.optionalAttrs (cfg.storage.gcTargetRatio != 0.9) {
              gc_target_ratio = cfg.storage.gcTargetRatio;
            };
        }
    // lib.optionalAttrs (cfg.gc.orphanGrace != 3600) { gc.orphan_grace = cfg.gc.orphanGrace; }
    // lib.optionalAttrs (cfg.retention.days != 30) { retention.days = cfg.retention.days; }
    // lib.optionalAttrs (cfg.tokens != { }) { tokens = cfg.tokens; }
    //
      lib.optionalAttrs
        (cfg.nix.priority != 30 || cfg.nix.trustedKeys != [ ] || cfg.nix.maxNarBytes != 17179869184)
        {
          nix =
            lib.optionalAttrs (cfg.nix.priority != 30) { priority = cfg.nix.priority; }
            // lib.optionalAttrs (cfg.nix.trustedKeys != [ ]) { trusted_keys = cfg.nix.trustedKeys; }
            // lib.optionalAttrs (cfg.nix.maxNarBytes != 17179869184) { max_nar_bytes = cfg.nix.maxNarBytes; };
        }
    // lib.optionalAttrs (cfg.log.level != "info" || cfg.log.access) {
      log =
        lib.optionalAttrs (cfg.log.level != "info") { level = cfg.log.level; }
        // lib.optionalAttrs cfg.log.access { access = true; };
    }
  ) cfg.settings;
  stateDir = settings.storage.dir or cfg.stateDir;
  metricsListen = settings.metrics.listen or cfg.metrics.listen;
  serverListen = settings.server.listen or cfg.listen;
  isLoopback =
    address:
    builtins.match "(127\\.[0-9]+\\.[0-9]+\\.[0-9]+|\\[(0*:)*:?(0*:)*0*1]):[0-9]+" address != null;
  configFile =
    let
      raw = toml.generate "oroshi.toml" settings;
    in
    pkgs.runCommand "oroshi.toml" { } ''
      ${lib.getExe cfg.package} config check --config ${raw}
      cp ${raw} "$out"
    '';
in
{
  options.services.oroshi = {
    enable = lib.mkEnableOption "oroshi, the package registry";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.oroshi;
      defaultText = lib.literalExpression "oroshi.packages.\${system}.oroshi";
      description = "The oroshi package.";
    };
    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
      description = "Plain HTTP socket address; the deployment repository configures the internal TLS proxy and network boundary.";
    };
    metrics.listen = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Loopback socket address for metrics; null disables the listener.";
    };
    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/oroshi";
      description = ''
        Durable local state directory. A custom path must already exist, owned by
        oroshi and inaccessible to other users. The entire directory must share
        one filesystem supporting atomic rename, fsync and SQLite WAL locking.
      '';
    };
    persistPaths = mkOption {
      type = types.listOf types.str;
      readOnly = true;
      default = [ stateDir ];
      description = "Paths the deployment repository must persist.";
    };
    log = {
      level = mkOption {
        type = types.str;
        default = "info";
        description = "Tracing target filter (overridden by RUST_LOG).";
      };
      access = mkOption {
        type = types.bool;
        default = false;
        description = "Log successful and not-found reads as well as writes and failures.";
      };
    };
    storage = {
      maxBytes = mkOption {
        type = types.nullOr types.ints.unsigned;
        default = null;
        description = "Maximum stored blob bytes; null disables the capacity limit.";
      };
      gcTargetRatio = mkOption {
        type = types.float;
        default = 0.9;
        description = "Target fraction of the storage capacity after garbage collection.";
      };
    };
    gc.orphanGrace = mkOption {
      type = types.ints.unsigned;
      default = 3600;
      description = "Grace period in seconds before an unreferenced blob can be removed.";
    };
    retention.days = mkOption {
      type = types.ints.unsigned;
      default = 30;
      description = "Retention period in days.";
    };
    tokens = mkOption {
      type = types.attrsOf (
        types.submodule {
          options = {
            sha256 = mkOption {
              type = types.str;
              description = "SHA-256 hash of the token as 64 lowercase hexadecimal characters.";
            };
            scopes = mkOption {
              type = types.listOf types.str;
              description = "Token scopes, validated by oroshi config check.";
            };
          };
        }
      );
      default = { };
      description = "Named token hashes and scopes; never store plaintext tokens here.";
    };
    nix = {
      priority = mkOption {
        type = types.ints.unsigned;
        default = 30;
        description = "Binary cache priority advertised in nix-cache-info.";
      };
      trustedKeys = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Trusted public signing keys in name:base64 format.";
      };
      maxNarBytes = mkOption {
        type = types.ints.unsigned;
        default = 17179869184;
        description = "Maximum bytes per NAR upload.";
      };
    };
    settings = mkOption {
      inherit (toml) type;
      default = { };
      description = "Raw TOML merged last, overriding typed fields; validated at build time by oroshi config check. Contains no secrets.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = metricsListen == null || isLoopback metricsListen;
        message = "services.oroshi.metrics.listen must be a loopback socket address.";
      }
      {
        # Clients reach oroshi only through the TLS proxy (ADR 0010); the unit also denies non-local peers.
        assertion = isLoopback serverListen;
        message = "services.oroshi.listen must be a loopback socket address.";
      }
      {
        assertion = lib.hasPrefix "/" stateDir;
        message = "services.oroshi.stateDir must be an absolute path.";
      }
    ];
    environment.etc."oroshi/oroshi.toml".source = configFile;
    users.users.oroshi = {
      isSystemUser = true;
      group = "oroshi";
    };
    users.groups.oroshi = { };
    systemd.services.oroshi = {
      description = "oroshi package registry";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      restartTriggers = [ configFile ];
      serviceConfig = {
        Type = "notify";
        ExecStart = "${lib.getExe cfg.package} serve --config ${configFile}";
        Restart = "on-failure";
        User = "oroshi";
        Group = "oroshi";
        StateDirectory = lib.mkIf (stateDir == "/var/lib/oroshi") "oroshi";
        StateDirectoryMode = "0750";
        ReadWritePaths = [ stateDir ];
        ProtectSystem = "strict";
        PrivateTmp = true;
        NoNewPrivileges = true;
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        UMask = "0027";
        ProtectHome = true;
        PrivateDevices = true;
        PrivateUsers = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RemoveIPC = true;
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
          "~@resources"
        ];
        SystemCallArchitectures = "native";
        # Only the co-located proxy and the loopback metrics scraper may connect.
        IPAddressAllow = "localhost";
        IPAddressDeny = "any";
        # Above the 1024 connections oroshi accepts, each with a blob and a SQLite reader.
        LimitNOFILE = 8192;
        # Tokio's blocking pool tops out at 512 threads.
        TasksMax = 1024;
        # A core dump could hold a presented token.
        LimitCORE = 0;
        LogRateLimitIntervalSec = lib.mkDefault "30s";
        LogRateLimitBurst = lib.mkDefault 30000;
      };
    };
  };
}
