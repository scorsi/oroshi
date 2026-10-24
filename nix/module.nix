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
    // lib.optionalAttrs (cfg.stateDir != "/var/lib/oroshi") { storage.dir = cfg.stateDir; }
    // lib.optionalAttrs (cfg.log.level != "info" || cfg.log.access) {
      log =
        lib.optionalAttrs (cfg.log.level != "info") { level = cfg.log.level; }
        // lib.optionalAttrs cfg.log.access { access = true; };
    }
  ) cfg.settings;
  stateDir = settings.storage.dir or cfg.stateDir;
  metricsListen = settings.metrics.listen or cfg.metrics.listen;
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
    settings = mkOption {
      inherit (toml) type;
      default = { };
      description = "Raw TOML merged last, overriding typed fields; validated at build time by oroshi config check. Contains no secrets.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          metricsListen == null
          ||
            builtins.match "(127\\.[0-9]+\\.[0-9]+\\.[0-9]+|\\[(0*:)*:?(0*:)*0*1]):[0-9]+" metricsListen
            != null;
        message = "services.oroshi.metrics.listen must be a loopback socket address.";
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
        LogRateLimitIntervalSec = lib.mkDefault "30s";
        LogRateLimitBurst = lib.mkDefault 30000;
      };
    };
  };
}
