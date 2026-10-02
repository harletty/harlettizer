//! Links libopus when the `opus` feature asks for it, and does nothing
//! otherwise.
//!
//! Two ways to find it. `pkg-config`, the default, which is how Linux and
//! macOS ship it — and which links it statically when `OPUS_STATIC` is set,
//! as the release builds do so that the binary carries no dependency. Or
//! `OPUS_LIB_DIR`, a directory holding the library, for a platform with no
//! `pkg-config` — the Windows release takes vcpkg's static build this way —
//! linked statically under `OPUS_STATIC` and dynamically otherwise.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=OPUS_LIB_DIR");
    println!("cargo::rerun-if-env-changed=OPUS_STATIC");
    #[cfg(feature = "opus")]
    link_opus();
}

#[cfg(feature = "opus")]
fn link_opus() {
    if let Some(dir) = std::env::var_os("OPUS_LIB_DIR") {
        let kind = if std::env::var_os("OPUS_STATIC").is_some() {
            "static"
        } else {
            "dylib"
        };
        println!("cargo::rustc-link-search=native={}", dir.to_string_lossy());
        println!("cargo::rustc-link-lib={kind}=opus");
        return;
    }
    // pkg-config reads OPUS_STATIC itself.
    if let Err(e) = pkg_config::Config::new()
        .atleast_version("1.1")
        .probe("opus")
    {
        panic!(
            "the `opus` feature links libopus, and pkg-config could not find it ({e}); \
             install libopus and its development files (`libopus-dev`, `opus`), or point \
             OPUS_LIB_DIR at a directory holding the library"
        );
    }
}
