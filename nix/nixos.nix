{self}: {
  config,
  lib,
  ...
}: let
  cfg = config.programs.perch;
in {
  imports = [(import ./options.nix {inherit self;})];

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    systemd.user.services.perch = {
      enableDefaultPath = false;
      description = "Resident Perch image viewer";
      after = ["graphical-session.target"];
      partOf = ["graphical-session.target"];
      wantedBy = ["graphical-session.target"];
      serviceConfig = {
        Type = "exec";
        ExecStart = "${lib.getExe cfg.package} --daemon";
        Restart = "on-failure";
        RestartSec = 1;
      };
    };
  };
}
