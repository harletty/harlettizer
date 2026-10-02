//! Links libopus when the `opus` feature asks for it, and does nothing
//! otherwise.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    #[cfg(feature = "opus")]
    if let Err(e) = pkg_config::Config::new()
        .atleast_version("1.1")
        .probe("opus")
    {
        panic!(
            "the `opus` feature links the system's libopus, and pkg-config could not find \
             it ({e}); install libopus and its development files (`libopus-dev`, `opus`)"
        );
    }
}
