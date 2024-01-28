use miette::{IntoDiagnostic, Result};

fn main() -> Result<()> {
    prost_build::compile_protos(&["src/update_metadata.proto"], &["src/"]).into_diagnostic()?;

    const INCLUDES: [&str; 2] = [".", "./bsdiff/include"];

    autocxx_build::Builder::new("src/extract/bspatch.rs", &INCLUDES)
        .auto_allowlist(true)
        .extra_clang_args(&["-D__off_t=long"])
        .build()?
        .std("c++14")
        .opt_level(3)
        .pic(true)
        .extra_warnings(false)
        .file("bsdiff/brotli_decompressor.cc")
        .file("bsdiff/bspatch.cc")
        .file("bsdiff/bz2_decompressor.cc")
        .file("bsdiff/buffer_file.cc")
        .file("bsdiff/decompressor_interface.cc")
        .file("bsdiff/extents.cc")
        .file("bsdiff/extents_file.cc")
        .file("bsdiff/file.cc")
        .file("bsdiff/logging.cc")
        .file("bsdiff/memory_file.cc")
        .file("bsdiff/patch_reader.cc")
        .file("bsdiff/sink_file.cc")
        .file("bsdiff/utils.cc")
        .compile("bspatch");

    println!("cargo:rustc-link-lib=bz2");
    println!("cargo:rustc-link-lib=brotlidec");
    println!("cargo:rerun-if-changed=src/extract/bspatch.rs");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
