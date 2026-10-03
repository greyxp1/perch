{self}: {
  config,
  lib,
  ...
}: let
  cfg = config.programs.perch;
in {
  imports = [(import ./options.nix {inherit self;})];

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
        Type = "exec";
        ExecStart = "${lib.getExe cfg.package} --daemon";
        Restart = "on-failure";
        RestartSec = 1;
      };
    };
  };
}
