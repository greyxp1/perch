{
  stdenv,
  lib,
  gdk-pixbuf,
  gnome,
  libjxl,
  librsvg,
  pkg-config,
  wayland,
  wayland-protocols,
  wayland-scanner,
  webp-pixbuf-loader,
}: let
  pixbufModules = gnome._gdkPixbufCacheBuilder_DO_NOT_USE {
    extraLoaders = [
      libjxl
      librsvg
      webp-pixbuf-loader
    ];
  };
in
stdenv.mkDerivation {
  pname = "perch";
  version = "0.2.0";
  src = ./perch.c;
  dontUnpack = true;
  strictDeps = true;

  nativeBuildInputs = [
    pkg-config
    wayland-scanner
  ];
  buildInputs = [
    gdk-pixbuf
    wayland
  ];

  buildPhase = let
    protocols = "${wayland-protocols}/share/wayland-protocols";
  in ''
    runHook preBuild

    scan() {
      wayland-scanner client-header "$1" "$2-client-protocol.h"
      wayland-scanner private-code "$1" "$2-protocol.c"
    }

    scan "${protocols}/stable/xdg-shell/xdg-shell.xml" xdg-shell
    scan "${protocols}/stable/viewporter/viewporter.xml" viewporter
    scan "${protocols}/unstable/xdg-output/xdg-output-unstable-v1.xml" xdg-output-unstable-v1

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
    runHook postInstall
  '';

  meta = {
    description = "Tiny native Wayland image pinning daemon";
    mainProgram = "perch";
    platforms = lib.platforms.linux;
  };
}
