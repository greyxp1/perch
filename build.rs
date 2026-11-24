use std::{env, path::PathBuf};

fn main() {
    let libraries = [
        "libheif",
        "libraw",
        "MagickWand",
        "libtiff-4",
        "libjpeg",
        "libwebpmux",
    ];
    let mut bindings = bindgen::Builder::default()
        .header_contents(
            "perch-codecs.h",
            "#include <libraw/libraw.h>\n#include <MagickWand/MagickWand.h>\n#include <lcms2.h>\n#include <webp/decode.h>\n#include <webp/encode.h>\n#include <webp/mux.h>\n#include <tiffio.h>\n#include <stdio.h>\n#include <jpeglib.h>\nenum PerchCmsFormat { RGBA8 = TYPE_RGBA_8, GRAY8 = TYPE_GRAY_8, CMYK8 = TYPE_CMYK_8 };",
        )
        .allowlist_function("libraw_(init|open_file|close|unpack|set_progress_handler|dcraw_(process|make_mem_image|clear_mem))")
        .allowlist_function("(New|Destroy)MagickWand|Magick(WandGenesis|SetProgressMonitor|ReadImage|SetFirstIterator|GetImage(Width|Height|Profile)|RelinquishMemory|ProfileImage|TransformImageColorspace|ExportImagePixels)")
        .allowlist_function("cms(Create_sRGBProfile|SaveProfileToMem|CloseProfile|OpenProfileFromMem|GetColorSpace|CreateTransform|DeleteTransform|DoTransform)")
        .allowlist_function("jpeg_(CreateDecompress|destroy_decompress|std_error|mem_src|save_markers|read_(header|icc_profile|scanlines|coefficients)|start_decompress|(crop|skip)_scanline[s]?|CreateCompress|destroy_compress|set_defaults|set_quality|simple_progression|start_compress|write_(scanlines|marker|icc_profile|coefficients)|finish_compress|copy_critical_parameters)")
        .allowlist_function("TIFF(Close|FdOpen|(Get|Set)Field|CurrentDirOffset|SetSubDirectory|ReadDirectory|IsTiled|IsCODECConfigured|GetStrile(Offset|ByteCount)|ReadScanline|RGBAImageOK|ReadRGBA(ImageOriented|StripExt|TileExt))")
        .allowlist_type("PerchCmsFormat")
        .allowlist_var("JPEG_(LIB_VERSION|APP0|COM)|INTENT_RELATIVE_COLORIMETRIC")
        .override_abi(bindgen::Abi::CUnwind, "jpeg_.*")
        .allowlist_function("WebP(InitDecoderConfigInternal|GetFeaturesInternal|Decode|FreeDecBuffer|Free)")
        .allowlist_function("WebP(ConfigInitInternal|ConfigLosslessPreset|ValidateConfig|PictureInitInternal|PictureImportRGBA|PictureFree|Encode|MemoryWrite(rInit|rClear)?)")
        .allowlist_function("WebPMux(CreateInternal|(Get|Set|Delete)Chunk|Assemble|Delete)")
        .allowlist_var("WEBP_(DECODER|ENCODER|MUX)_ABI_VERSION")
        .layout_tests(false);
    for library in libraries {
        let mut config = pkg_config::Config::new();
        if library == "libheif" {
            config.atleast_version("1.23.1");
        }
        let library = config.statik(true).probe(library).unwrap();
        for path in library.include_paths {
            bindings = bindings.clang_arg(format!("-I{}", path.display()));
        }
        for (name, value) in library.defines {
            bindings = bindings.clang_arg(match value {
                Some(value) => format!("-D{name}={value}"),
                None => format!("-D{name}"),
            });
        }
    }
    bindings
        .generate()
        .expect("could not generate codec bindings")
        .write_to_file(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("codecs.rs"))
        .unwrap();

    if let Some(path) = std::env::var_os("PERCH_CXX_LIB") {
        println!("cargo:rustc-link-search=native={}", path.to_string_lossy());
    }
    println!("cargo:rustc-link-lib=static=stdc++");
    // HEVC CPU dispatch needs GCC helpers after the codec archives.
    println!("cargo:rustc-link-arg=-lgcc");
    println!("cargo:rustc-link-arg=-Wl,-z,pack-relative-relocs");
    println!("cargo:rerun-if-env-changed=PERCH_CXX_LIB");
    println!("cargo:rerun-if-changed=build.rs");
}
