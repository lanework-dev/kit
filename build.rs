// Drop dylibs nothing calls (the `libc` crate links libiconv on macOS), so
// lwk links only libSystem and starts as fast as it can.
fn main() {
    // Without this, cargo reruns the script (and relinks lwk) whenever any
    // file in the package changes, e.g. a dist-manifest.json written at the
    // root, which undid the release workflow's signature.
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-dead_strip_dylibs");
    }
}
