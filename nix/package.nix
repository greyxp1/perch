{
  buildPackages,
  gdk-pixbuf,
  lib,
  libjxl,
  librsvg,
  pkg-config,
  runCommand,
  stdenv,
  wayland,
  wayland-protocols,
  wayland-scanner,
  webp-pixbuf-loader,
}: let
  pixbufModules = runCommand "perch-pixbuf-loaders.cache" {preferLocalBuild = true;} ''
    for loader in ${lib.escapeShellArgs [gdk-pixbuf libjxl librsvg webp-pixbuf-loader]}; do
      module_dir="$loader/${gdk-pixbuf.moduleDir}"
      test -d "$module_dir"
      GDK_PIXBUF_MODULEDIR="$module_dir" \
        ${stdenv.hostPlatform.emulator buildPackages} ${gdk-pixbuf.dev}/bin/gdk-pixbuf-query-loaders
    done > "$out"
  '';
in
  stdenv.mkDerivation {
    pname = "perch";
    version = "0.2.0";
    src = ../perch.c;
    dontUnpack = true;
    strictDeps = true;

    nativeBuildInputs = [
      pkg-config
      wayland-scanner
    ];
    buildInputs = [
      gdk-pixbuf
      wayland
      wayland-protocols
    ];

    buildPhase = ''
      runHook preBuild
      protocols=$(pkg-config --variable=pkgdatadir wayland-protocols)

      scan() {
        wayland-scanner client-header "$1" "$2-client-protocol.h"
        wayland-scanner private-code "$1" "$2-protocol.c"
      }

      scan "$protocols/stable/xdg-shell/xdg-shell.xml" xdg-shell
      scan "$protocols/stable/viewporter/viewporter.xml" viewporter
      scan "$protocols/unstable/xdg-output/xdg-output-unstable-v1.xml" xdg-output-unstable-v1

      $CC -std=c11 -O2 -flto -Wall -Wextra -Wpedantic -Wconversion -Werror \
        -Wno-unused-parameter -DPERCH_PIXBUF_MODULE_FILE='"${pixbufModules}"' \
        -Wshadow -Wformat=2 -Wstrict-prototypes \
        "$src" ./*-protocol.c -I. -o perch \
        $(pkg-config --cflags --libs gdk-pixbuf-2.0 wayland-client) -lm

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      install -Dm755 perch "$out/bin/perch"
      install -Dm644 ${../LICENSE} "$out/share/licenses/perch/LICENSE"
      runHook postInstall
    '';

    meta = {
      description = "Tiny native Wayland image pinning daemon";
      homepage = "https://github.com/greyxp1/perch";
      license = lib.licenses.mit;
      mainProgram = "perch";
      platforms = lib.platforms.linux;
    };
  }
