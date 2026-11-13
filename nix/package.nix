{
  cmake,
  dejavu_fonts,
  lib,
  libjpeg,
  libxkbcommon,
  libwebp,
  libtiff,
  rustPlatform,
  zlib,
}:
let
  jpeg = libjpeg.overrideAttrs (old: {
    cmakeFlags = (old.cmakeFlags or []) ++ ["-DWITH_SYSTEM_ZLIB=ON"];
    buildInputs = (old.buildInputs or []) ++ [zlib];
  });
in lib.overrideDerivation (rustPlatform.buildRustPackage {
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
  buildInputs = [
    # Bundled zlib would override Perch's zlib-ng symbols.
    jpeg
    ((libxkbcommon.override {withWaylandTools = false;}).overrideAttrs (old: {
      outputs = ["out" "dev"];
      mesonFlags = (old.mesonFlags or []) ++ [
        "-Denable-x11=false" "-Denable-xkbregistry=false"
        "-Denable-tools=false" "-Denable-docs=false"
      ];
    }))
    (libtiff.overrideAttrs (old: {
      propagatedBuildInputs = [jpeg zlib];
      cmakeFlags = (old.cmakeFlags or []) ++ map (name: lib.cmakeBool name false) [
        "jbig" "libdeflate" "lzma" "zstd" "webp" "lerc"
      ];
    }))
    (libwebp.override {
      pngSupport = false;
      jpegSupport = false;
      tiffSupport = false;
      gifSupport = false;
      libwebpmuxSupport = false;
    })
  ];
  PERCH_FALLBACK_FONT = "${dejavu_fonts.minimal}/share/fonts/truetype/DejaVuSans.ttf";
  postInstall = ''
    install -Dm644 LICENSE "$out/share/licenses/perch/LICENSE"
  '';
  # Static applications do not propagate their link-time dependencies.
  postFixup = ''
    rm -f "$out/nix-support/propagated-build-inputs"
  '';
  meta = {
    description = "Tiny native Wayland image pinning daemon";
    homepage = "https://github.com/greyxp1/perch";
    license = lib.licenses.mit;
    mainProgram = "perch";
    platforms = lib.platforms.linux;
  };
}) (old: {name = "${old.pname}-${old.version}";})
