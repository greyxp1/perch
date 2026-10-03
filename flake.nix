{
  description = "A tiny native Wayland image pinning daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
    ...
  }: let
    systems = [
      "aarch64-linux"
      "x86_64-linux"
    ];
    eachSystem = f:
      nixpkgs.lib.genAttrs systems
      (system: f nixpkgs.legacyPackages.${system});
  in {
    homeModules.default = import ./nix/home-manager.nix {inherit self;};
    nixosModules.default = import ./nix/nixos.nix {inherit self;};

    packages = eachSystem (pkgs: let
      perch = pkgs.callPackage ./nix/package.nix {};
    in {
      inherit perch;
      default = perch;
    });

    devShells = eachSystem (pkgs: {
      default = pkgs.mkShell {
        inputsFrom = [self.packages.${pkgs.stdenv.hostPlatform.system}.perch];
      };
    });

    apps = eachSystem (pkgs: let
      perch = {
        type = "app";
        program = nixpkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.perch;
      };
    in {
      inherit perch;
      default = perch;
    });
  };
}
