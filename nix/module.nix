flake:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.pantheon;
  format = pkgs.formats.toml { };
  configuration = format.generate "pantheon.toml" (
    lib.recursiveUpdate cfg.settings {
      state_dir = cfg.stateDirectory;
      workspace = cfg.workspace;
      discord = {
        application_id = cfg.applicationId;
        allowed_users = cfg.allowedUsers;
      };
      browser = {
        port_start = cfg.browserPortStart;
        port_end = cfg.browserPortEnd;
      };
      skills = {
        directories = map toString cfg.skillDirectories;
        bundled = cfg.bundledSkills;
      };
    }
  );
in
{
  options.services.pantheon = {
    enable = lib.mkEnableOption "Pantheon always-on Discord agents";
    package = lib.mkOption {
      type = lib.types.package;
      default = flake.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "Pantheon package, including the Camoufox runtime.";
    };
    applicationId = lib.mkOption {
      type = lib.types.ints.unsigned;
      description = "Discord application ID.";
    };
    allowedUsers = lib.mkOption {
      type = lib.types.listOf lib.types.ints.unsigned;
      default = [ ];
      description = "Discord user IDs permitted to invoke the harness.";
    };
    environmentFile = lib.mkOption {
      type = lib.types.str;
      description = "Absolute file outside the Nix store containing DISCORD_TOKEN and any API provider keys. Codex models use the service account's separate ChatGPT login cache.";
    };
    stateDirectory = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/pantheon";
      description = "Durable state, context journals and browser profiles.";
    };
    workspace = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/pantheon/workspace";
      description = "Agent working directory.";
    };
    browserPortStart = lib.mkOption {
      type = lib.types.port;
      default = 6080;
    };
    browserPortEnd = lib.mkOption {
      type = lib.types.port;
      default = 6180;
    };
    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open all browser viewer TCP ports on every interface. Prefer interface-specific firewall rules for Tailscale-only access.";
    };
    settings = lib.mkOption {
      type = format.type;
      default = { };
      description = "Additional Pantheon TOML settings. Dedicated module options define paths, Discord authorization and browser ports. Secrets belong in environmentFile.";
    };
    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "Additional commands exposed to agents through the service PATH.";
    };
    skillDirectories = lib.mkOption {
      type = lib.types.listOf lib.types.path;
      default = [ ];
      description = "Skill libraries or individual folders containing SKILL.md. Index and main guides are frozen at service startup. Repository-local or packaged skills work without runtime downloads.";
    };
    bundledSkills = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Include Pantheon's research, browser activities, learning and engineering guides.";
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.allowedUsers != [ ];
        message = "services.pantheon.allowedUsers must explicitly authorize at least one Discord user.";
      }
      {
        assertion =
          lib.hasPrefix "/" cfg.environmentFile && !(lib.hasPrefix "/nix/store/" cfg.environmentFile);
        message = "Pantheon environmentFile must be an absolute secret path outside the Nix store.";
      }
      {
        assertion = cfg.browserPortEnd >= cfg.browserPortStart;
        message = "Pantheon browser port range is invalid.";
      }
    ];
    users.groups.pantheon = { };
    users.users.pantheon = {
      isSystemUser = true;
      group = "pantheon";
      home = cfg.stateDirectory;
    };
    systemd.tmpfiles.rules = [
      "d ${cfg.stateDirectory} 0700 pantheon pantheon -"
      "d ${cfg.workspace} 0700 pantheon pantheon -"
    ];
    systemd.services.pantheon = {
      description = "Pantheon Discord agent harness";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = cfg.extraPackages;
      serviceConfig = {
        Type = "simple";
        User = "pantheon";
        Group = "pantheon";
        WorkingDirectory = cfg.workspace;
        ExecStart = "${cfg.package}/bin/pantheon --config ${configuration} run";
        EnvironmentFile = cfg.environmentFile;
        Restart = "always";
        RestartSec = 5;
        TimeoutStopSec = 30;
        KillMode = "control-group";
        UMask = "0077";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [
          cfg.stateDirectory
          cfg.workspace
        ];
        # Firefox needs user namespaces and AF_UNIX; do not use PrivateNetwork,
        # MemoryDenyWriteExecute or restrictive namespace syscall filters.
        LimitNOFILE = 65536;
      };
      environment = {
        HOME = cfg.stateDirectory;
        PYTHONDONTWRITEBYTECODE = "1";
      };
    };
    networking.firewall.allowedTCPPortRanges = lib.mkIf cfg.openFirewall [
      {
        from = cfg.browserPortStart;
        to = cfg.browserPortEnd;
      }
    ];
  };
}
