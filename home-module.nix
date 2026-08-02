{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.perch;
in {
  options.programs.perch = {
    enable = lib.mkEnableOption "the Perch Wayland image viewer";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "Perch package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];
    systemd.user.services.perch = {
      Install.WantedBy = ["graphical-session.target"];
      Unit = {
        Description = "Resident Perch image viewer";
        After = ["graphical-session.target"];
        PartOf = ["graphical-session.target"];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/perch --daemon";
        Restart = "on-failure";
        RestartSec = 1;
      };
    };
  };
}
