{ config, lib, pkgs, ... }:

let
  cfg = config.services.fractal-agent;
in
{
  options.services.fractal-agent = {
    enable = lib.mkEnableOption "the fractal-agent configuration daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.fractal-agent;
      defaultText = lib.literalExpression "pkgs.fractal-agent";
      description = "The fractal-agent package to run.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "fractal-agent";
      description = ''
        Unix user the agent runs as. It owns the configuration repository and
        the generations database, and it is the only caller the trigger's D-Bus
        policy admits. It holds no key and cannot activate anything.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
      description = "Fractal Linux configuration daemon";
    };
    users.groups.${cfg.user} = { };

    systemd.services.fractal-agent = {
      description = "Fractal Linux configuration daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "dbus.service" "network.target" ];
      requires = [ "dbus.service" ];

      # Evaluating and building the configuration is the agent's whole job.
      path = [ pkgs.lix ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/fractal-agent";
        User = cfg.user;
        Group = cfg.user;
        Restart = "on-failure";

        # /var/lib/fractal-agent holds the configuration repository, the
        # generations database, build logs and garbage-collection roots.
        StateDirectory = "fractal-agent";
        RuntimeDirectory = "fractal-agent";

        # Unprivileged and kept that way: the agent brokers activation but can
        # neither sign nor perform it.
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectControlGroups = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        RestrictSUIDSGID = true;
      };
    };
  };
}
