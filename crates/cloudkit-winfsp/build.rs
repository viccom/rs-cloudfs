fn main() {
    // Emits `/DELAYLOAD:winfsp-x64.dll` (+ delayimp) for *this package's*
    // linkable artifacts — its tests and benches. `rustc-link-arg` does
    // not travel across library boundaries, so the crate that produces
    // the final binary has to emit it: cloudkit-cli/build.rs does the
    // same two lines for the shipped `cydrive.exe` (K38/K40; spike-
    // verified: the exe must start with the DLL absent from the search
    // path, which only holds under delay-load linking).
    //
    // Hand-written instead of winfsp::build::winfsp_link_delayload():
    // that helper lives behind the winfsp `delayload` feature, and an
    // optional build-dependency cannot be feature-gated from the build
    // script itself (build scripts cannot see their own package's
    // features through cfg).
    if std::env::var_os("CARGO_FEATURE_WINFSP").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }
}
