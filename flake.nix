{
  description = "A tiny native Wayland image pinning daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {self, nixpkgs, ...}: let
    systems = [
      "aarch64-linux"
      "x86_64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    homeModules.default = import ./home-module.nix {inherit self;};

    packages = forAllSystems (system: let
      pkgs = import nixpkgs {inherit system;};
      perch = pkgs.callPackage ./package.nix {};
    in {
      inherit perch;
      default = perch;
    });

    apps = forAllSystems (system: let
      perch = {
        type = "app";
        program = "${nixpkgs.lib.getExe self.packages.${system}.default}";
      };
    in {
      inherit perch;
      default = perch;
    });
  };
}
