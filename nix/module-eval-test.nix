{ pkgs, serviceModules }:
let
  inherit (pkgs) lib;
  package = pkgs.writeShellScriptBin "cara" "exit 0";
  attrsOption = lib.mkOption {
    type = lib.types.attrs;
    default = { };
  };
  assertionsOption = lib.mkOption {
    type = lib.types.listOf lib.types.attrs;
    default = [ ];
  };
  packageListOption = lib.mkOption {
    type = lib.types.listOf lib.types.package;
    default = [ ];
  };
  eval =
    module: base: config:
    (lib.evalModules {
      specialArgs = { inherit pkgs; };
      modules = [
        module
        base
        config
      ];
    }).config;
  repositories = [
    "/home/harry/.cacophony/caravan-worktrees/cacophony"
    "/home/harry/.cacophony/caravan-worktrees/caravan"
    "/home/harry/.cacophony/caravan-worktrees/pi-daemon"
  ];
  nixos =
    eval serviceModules.nixos
      {
        options.assertions = assertionsOption;
        options.environment.systemPackages = packageListOption;
        options.systemd.user.services = attrsOption;
      }
      {
        services.caravan.web = {
          enable = true;
          inherit package repositories;
          bind = "100.64.0.42";
          port = 4774;
          interval = 45;
          readOnly = true;
        };
      };
  darwin =
    eval serviceModules.darwin
      {
        options.assertions = assertionsOption;
        options.environment.systemPackages = packageListOption;
        options.launchd.user.agents = attrsOption;
        options.system.primaryUser = lib.mkOption {
          type = lib.types.str;
          default = "test-user";
        };
        options.users.users = attrsOption;
      }
      {
        users.users.test-user.home = "/Users/test-user";
        services.caravan.web = {
          enable = true;
          inherit package repositories;
          bind = "::1";
          readOnly = false;
        };
      };
  nixOnDroid =
    eval serviceModules.nixOnDroid
      {
        options.assertions = assertionsOption;
        options.environment.packages = packageListOption;
        options.supervisord.programs = attrsOption;
      }
      {
        services.caravan.web = {
          enable = true;
          inherit package repositories;
          homeDir = "/data/data/com.termux.nix/files/home";
        };
      };
  systemdExec = nixos.systemd.user.services.caravan-web.serviceConfig.ExecStart;
  launchdCommand = darwin.launchd.user.agents.caravan-web.command;
  supervisorCommand = nixOnDroid.supervisord.programs.caravan-web.command;
in
assert lib.elem "default.target" nixos.systemd.user.services.caravan-web.wantedBy;
assert lib.hasInfix "--listen 100.64.0.42:4774" systemdExec;
assert lib.hasInfix "--poll-seconds 45" systemdExec;
assert lib.hasInfix "--read-only" systemdExec;
assert lib.all (repository: lib.hasInfix "--repo ${repository}" systemdExec) repositories;
assert lib.hasInfix "--listen '[::1]:4774'" launchdCommand;
assert !(lib.hasInfix "--read-only" launchdCommand);
assert lib.hasInfix "cara web" supervisorCommand;
pkgs.runCommand "caravan-service-module-eval" { } ''
  touch "$out"
''
