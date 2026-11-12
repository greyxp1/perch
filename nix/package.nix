{
  cmake,
  dejavu_fonts,
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "perch";
  version = "0.2.0";
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../LICENSE
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  strictDeps = true;
  stripAllList = ["bin"];
  nativeBuildInputs = [cmake];
  PERCH_FALLBACK_FONT = "${dejavu_fonts.minimal}/share/fonts/truetype/DejaVuSans.ttf";
  postInstall = ''
    install -Dm644 LICENSE "$out/share/licenses/perch/LICENSE"
  '';
  meta = {
    description = "Tiny native Wayland image pinning daemon";
    homepage = "https://github.com/greyxp1/perch";
    license = lib.licenses.mit;
    mainProgram = "perch";
    platforms = lib.platforms.linux;
  };
}
