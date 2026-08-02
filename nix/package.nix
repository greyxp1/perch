{
  cmake,
  cxxRuntime,
  dejavu_fonts,
  lib,
  libjpeg,
  libheif,
  imagemagick,
  libraw,
  lcms2,
  libde265,
  openjpeg,
  libxkbcommon,
  libwebp,
  dav1d,
  pkg-config,
  libtiff,
  jbigkit,
  rustPlatform,
  zlib,
}:
let
  cms = lcms2.overrideAttrs (_: {
    buildInputs = [];
    propagatedBuildInputs = [];
  });
  jpeg2000 = openjpeg.overrideAttrs (old: {
    buildInputs = [];
    propagatedBuildInputs = [];
    cmakeFlags = old.cmakeFlags ++ ["-DBUILD_CODEC=OFF"];
    # Upstream tests require the omitted command-line codecs.
    doCheck = false;
  });
  heif = libheif.overrideAttrs (old: {
    outputs = ["out" "dev" "lib"];
    nativeBuildInputs = [cmake pkg-config];
    propagatedBuildInputs = [dav1d libde265 jpeg jpeg2000];
    cmakeFlags = (old.cmakeFlags or []) ++ [
      "-DENABLE_PLUGIN_LOADING=OFF" "-DWITH_EXAMPLES=OFF" "-DWITH_GDK_PIXBUF=OFF"
      "-DWITH_X265=OFF" "-DWITH_X264=OFF" "-DWITH_AOM_ENCODER=OFF" "-DWITH_AOM_DECODER=OFF"
      "-DWITH_DAV1D=ON" "-DWITH_JPEG_DECODER=ON" "-DWITH_OpenJPEG_DECODER=ON"
    ];
    postInstall = "";
  });
  photoshop = (imagemagick.override {
    bzip2Support = false;
    libX11Support = false;
    libXtSupport = false;
    fontconfigSupport = false;
    freetypeSupport = false;
    libjpegSupport = false;
    djvulibreSupport = false;
    openexrSupport = false;
    libjxlSupport = false;
    libpngSupport = false;
    liblqr1Support = false;
    libraqmSupport = false;
    librawSupport = false;
    librsvgSupport = false;
    libtiffSupport = false;
    libultrahdrSupport = false;
    libxml2Support = false;
    openjpegSupport = false;
    libwebpSupport = false;
    libheifSupport = false;
    fftwSupport = false;
  }).overrideAttrs (old: {
    outputs = ["out" "dev"];
    # Only Photoshop uses this library. Other formats have their own decoders.
    postPatch = (old.postPatch or "") + ''
      sed -i '/^AddMagickCoder(/ { /AddMagickCoder(PSD)/!d; }' coders/coders-list.h
      grep -qx 'AddMagickCoder(PSD)' coders/coders-list.h
    '';
    env.NIX_CFLAGS_COMPILE = "-ffunction-sections -fdata-sections";
    buildInputs = [zlib cms];
    propagatedBuildInputs = [zlib cms];
    configureFlags = old.configureFlags ++ [
      "--disable-openmp" "--without-magick-plus-plus" "--enable-zero-configuration"
      "--without-png" "--without-jpeg" "--without-tiff" "--without-webp" "--without-lzma"
      "--without-dps" "--without-x" "--without-fontconfig" "--without-freetype"
      "--without-xml" "--without-perl"
    ];
    # The embedded decoder has no runtime configuration or command-line tools.
    postInstall = old.postInstall + ''
      moveToOutput "lib/*.a" "$dev"
      for pc in "$dev"/lib/pkgconfig/*.pc; do
        substituteInPlace "$pc" --replace-fail "$out/lib" "$dev/lib"
      done
      rm -rf "$out/bin" "$out/etc" "$out/share" "$out/lib"
    '';
    doInstallCheck = false;
  });
  # Nixpkgs replaces the upstream static targets with shared targets on Linux.
  jbig = jbigkit.overrideAttrs (old: {
    patches = builtins.filter (patch:
      !lib.hasInfix "shared_lib.patch" (toString patch)
      && !lib.hasInfix "ldflags.patch" (toString patch)
    ) old.patches;
    buildPhase = ''
      runHook preBuild
      make -C libjbig $makeFlags libjbig.a libjbig85.a
      runHook postBuild
    '';
    checkPhase = ''
      runHook preCheck
      make -C libjbig $makeFlags test
      runHook postCheck
    '';
    installPhase = ''
      runHook preInstall
      install -Dm644 libjbig/*.h -t "$out/include"
      install -Dm644 libjbig/*.a -t "$out/lib"
      runHook postInstall
    '';
  });
  jpeg = libjpeg.overrideAttrs (old: {
    cmakeFlags = (old.cmakeFlags or []) ++ ["-DWITH_SYSTEM_ZLIB=ON"];
    # Rust catches libjpeg's fatal errors through the C-unwind ABI.
    env.NIX_CFLAGS_COMPILE = "-fexceptions";
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
      ../build.rs
      ../src
      ../LICENSE
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  strictDeps = true;
  stripAllList = ["bin"];
  nativeBuildInputs = [cmake pkg-config rustPlatform.bindgenHook];
  buildInputs = [
    # Bundled zlib would override Perch's zlib-ng symbols.
    jpeg
    dav1d
    heif
    ((libraw.override {lcms2 = cms;}).overrideAttrs (old: {
      configureFlags = (old.configureFlags or []) ++ ["--disable-examples" "--disable-openmp"];
    }))
    photoshop
    ((libxkbcommon.override {withWaylandTools = false;}).overrideAttrs (old: {
      outputs = ["out" "dev"];
      # Wayland supplies the complete keymap. Perch disables default includes.
      buildInputs = [];
      propagatedBuildInputs = [];
      mesonFlags = builtins.filter (flag:
        !(lib.hasPrefix "-Dxkb-config-root=" flag || lib.hasPrefix "-Dx-locale-root=" flag)
      ) (old.mesonFlags or []) ++ [
        "-Ddatadir=/usr/share"
        "-Denable-x11=false" "-Denable-xkbregistry=false"
        "-Denable-tools=false" "-Denable-docs=false"
      ];
    }))
    ((libtiff.override { libjpeg = jpeg; withLerc = true; }).overrideAttrs (old: {
      propagatedBuildInputs = old.propagatedBuildInputs ++ [jbig];
    }))
    (libwebp.override {tiffSupport = false;})
  ];
  # The native musl C++ runtime supports static PIE, unlike the cross runtime.
  PERCH_CXX_LIB = "${cxxRuntime}/lib";
  PERCH_FALLBACK_FONT = "${dejavu_fonts.minimal}/share/fonts/truetype/DejaVuSans.ttf";
  postInstall = ''
    install -Dm644 LICENSE "$out/share/licenses/perch/LICENSE"
    install -Dm644 ${libraw.src}/LICENSE.CDDL "$out/share/licenses/perch/LibRaw"
    install -Dm644 ${photoshop.src}/LICENSE "$out/share/licenses/perch/ImageMagick"
    install -Dm644 ${heif.src}/COPYING "$out/share/licenses/perch/libheif"
    install -Dm644 ${libde265.src}/COPYING "$out/share/licenses/perch/libde265"
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
