{
  brotli,
  cmake,
  cxxRuntime,
  dejavu_fonts,
  lib,
  libaom,
  libjpeg,
  libheif,
  libhwy,
  libjxl,
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
  # Dav1d decodes AV1, so libaom only encodes.
  aom = (libaom.override {enableVmaf = false;}).overrideAttrs (old: {
    cmakeFlags = old.cmakeFlags ++ ["-DCONFIG_AV1_DECODER=0" "-DENABLE_EXAMPLES=0" "-DENABLE_TOOLS=0"];
  });
  heif = (libheif.override {libaom = aom;}).overrideAttrs (old: {
    outputs = ["out" "dev" "lib"];
    nativeBuildInputs = [cmake pkg-config];
    propagatedBuildInputs = [aom dav1d libde265 jpeg jpeg2000];
    cmakeFlags = (old.cmakeFlags or []) ++ [
      "-DENABLE_PLUGIN_LOADING=OFF" "-DWITH_EXAMPLES=OFF" "-DWITH_GDK_PIXBUF=OFF"
      "-DWITH_X265=OFF" "-DWITH_X264=OFF" "-DWITH_AOM_ENCODER=ON" "-DWITH_AOM_DECODER=OFF"
      "-DWITH_DAV1D=ON" "-DWITH_JPEG_DECODER=ON" "-DWITH_OpenJPEG_DECODER=ON"
    ];
    postInstall = "";
  });
  # Only the encoder is used. Jxl-oxide decodes.
  jxl = (libjxl.override {enablePlugins = false;}).overrideAttrs (old: {
    outputs = ["out" "dev"];
    nativeBuildInputs = [cmake pkg-config];
    depsBuildBuild = [];
    buildInputs = [];
    propagatedBuildInputs = [brotli libhwy cms];
    cmakeFlags = lib.remove "-DJPEGXL_FORCE_SYSTEM_GTEST=ON" old.cmakeFlags ++ [
      "-DJPEGXL_ENABLE_TOOLS=OFF" "-DJPEGXL_ENABLE_BENCHMARK=OFF" "-DJPEGXL_ENABLE_EXAMPLES=OFF"
      "-DJPEGXL_ENABLE_DOXYGEN=OFF" "-DJPEGXL_ENABLE_MANPAGES=OFF" "-DJPEGXL_ENABLE_TCMALLOC=OFF"
    ];
    postInstall = "";
    doCheck = false;
  });
  # Inputs are replaced below, so only switches that add configure flags matter.
  photoshop = (imagemagick.override {
    libjxlSupport = false;
    liblqr1Support = false;
    librsvgSupport = false;
    libultrahdrSupport = false;
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
  jpeg = libjpeg.overrideAttrs (_: {
    # Rust catches libjpeg's fatal errors through the C-unwind ABI.
    env.NIX_CFLAGS_COMPILE = "-fexceptions";
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
  nativeBuildInputs = [pkg-config rustPlatform.bindgenHook];
  buildInputs = [
    jpeg
    dav1d
    heif
    jxl
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
    install -Dm644 ${aom.src}/LICENSE "$out/share/licenses/perch/libaom"
    install -Dm644 ${jxl.src}/LICENSE "$out/share/licenses/perch/libjxl"
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
