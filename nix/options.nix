{self}: {
  lib,
  pkgs,
  ...
}: {
  options.programs.perch = {
    enable = lib.mkEnableOption "the Perch Wayland image viewer";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.perch;
      defaultText = lib.literalExpression "inputs.perch.packages.\${pkgs.stdenv.hostPlatform.system}.perch";
      description = "Perch package to use.";
    };
  };
}
