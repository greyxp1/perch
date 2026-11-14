fn main() {
    cc::Build::new()
        .opt_level(2)
        .file("src/webp.c")
        .file("src/jpeg.c")
        .compile("perch_codecs");
    println!("cargo:rustc-link-lib=static=jpeg");
    println!("cargo:rerun-if-changed=src/webp.c");
    println!("cargo:rerun-if-changed=src/jpeg.c");
}
