// Drop dylibs nothing calls (the `libc` crate links libiconv on macOS), so
// lwk links only libSystem and starts as fast as it can.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-dead_strip_dylibs");
    }
}
