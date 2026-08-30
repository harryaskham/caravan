{ self }:
let
  packageFor = pkgs: pkgs.caravan or self.packages.${pkgs.system}.caravan;

  commonOptions =
    {
      lib,
      pkgs,
      home,
    }:
    {
      enable = lib.mkEnableOption "the persistent Caravan web dashboard";
      package = lib.mkOption {
        type = lib.types.package;
        default = packageFor pkgs;
        description = "Caravan package that provides the cara executable.";
      };
      bind = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        example = "100.64.0.1";
        description = "Address for the web listener. Arbitrary values, including tailnet addresses, are passed through without policy restrictions.";
      };
      port = lib.mkOption {
        type = lib.types.port;
        default = 4774;
        description = "TCP port for the web listener.";
      };
      readOnly = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Disable every Caravan web mutation endpoint.";
      };
      interval = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = "Seconds between bounded dashboard status refreshes.";
      };
      repositories = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [
          "${home}/.cacophony/caravan-worktrees/cacophony"
          "${home}/.cacophony/caravan-worktrees/caravan"
          "${home}/.cacophony/caravan-worktrees/pi-daemon"
        ];
        description = "Pre-provisioned repository or worktree paths shown by the dashboard.";
      };
      extraArgs = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Additional arguments appended to cara web.";
      };
    };

  listenFor =
    lib: cfg:
    if lib.hasPrefix "[" cfg.bind && lib.hasSuffix "]" cfg.bind then
      "${cfg.bind}:${toString cfg.port}"
    else if lib.hasInfix ":" cfg.bind then
      "[${cfg.bind}]:${toString cfg.port}"
    else
      "${cfg.bind}:${toString cfg.port}";

  argsFor =
    { lib, cfg }:
    [
      (lib.getExe' cfg.package "cara")
      "web"
      "--listen"
      (listenFor lib cfg)
      "--poll-seconds"
      (toString cfg.interval)
    ]
    ++ lib.optionals cfg.readOnly [ "--read-only" ]
    ++ lib.concatMap (repository: [
      "--repo"
      repository
    ]) cfg.repositories
    ++ cfg.extraArgs;

  repositoryAssertion = cfg: {
    assertion = cfg.repositories != [ ];
    message = "services.caravan.web.repositories must contain at least one repository path";
  };
in
{
  nixos =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.caravan.web;
      args = argsFor { inherit lib cfg; };
    in
    {
      options.services.caravan.web = commonOptions {
        inherit lib pkgs;
        home = "%h";
      };
      config = lib.mkIf cfg.enable {
        assertions = [ (repositoryAssertion cfg) ];
        environment.systemPackages = [ cfg.package ];
        systemd.user.services.caravan-web = {
          description = "Caravan web dashboard";
          wantedBy = [ "default.target" ];
          unitConfig.ConditionUser = "!@system";
          serviceConfig = {
            ExecStart = lib.escapeShellArgs args;
            Restart = "on-failure";
            RestartSec = 5;
            RestartSteps = 5;
            RestartMaxDelaySec = 300;
          };
        };
      };
    };

  darwin =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.caravan.web;
      primaryUser = config.system.primaryUser or "nobody";
      home = config.users.users.${primaryUser}.home or "/Users/${primaryUser}";
      args = argsFor { inherit lib cfg; };
    in
    {
      options.services.caravan.web = commonOptions { inherit lib pkgs home; };
      config = lib.mkIf cfg.enable {
        assertions = [ (repositoryAssertion cfg) ];
        environment.systemPackages = [ cfg.package ];
        launchd.user.agents.caravan-web = {
          command = lib.escapeShellArgs args;
          serviceConfig = {
            KeepAlive = true;
            RunAtLoad = true;
            ProcessType = "Background";
            ThrottleInterval = 5;
            StandardOutPath = "${home}/Library/Logs/caravan-web.log";
            StandardErrorPath = "${home}/Library/Logs/caravan-web.err.log";
          };
        };
      };
    };

  nixOnDroid =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.caravan.web;
      args = argsFor { inherit lib cfg; };
    in
    {
      options.services.caravan.web =
        (commonOptions {
          inherit lib pkgs;
          home = cfg.homeDir;
        })
        // {
          homeDir = lib.mkOption {
            type = lib.types.str;
            default = "/home/nix-on-droid";
            description = "Home directory exported to Caravan under supervisord.";
          };
        };
      config = lib.mkIf cfg.enable {
        assertions = [ (repositoryAssertion cfg) ];
        environment.packages = [ cfg.package ];
        supervisord.programs.caravan-web = {
          command = lib.escapeShellArgs args;
          directory = cfg.homeDir;
          path = [ cfg.package ];
          autostart = true;
          autorestart = true;
          startsecs = 2;
          stopwaitsecs = 15;
          environment.HOME = cfg.homeDir;
        };
      };
    };
}
