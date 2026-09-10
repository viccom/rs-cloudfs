fn main() {
    // `cydrive.exe` is the artifact that actually links winfsp-sys: the
    // WinFsp DLL import has to be delay-loaded for the binary to start on
    // a machine where `winfsp-x64.dll` is not on the search path (it
    // lives in the WinFsp install dir, which is not on PATH). rustc-link-arg
    // does not travel across library boundaries, so every crate that
    // produces a linkable artifact in the winfsp graph emits these two
    // lines itself — cloudkit-winfsp/build.rs does the same for its tests.
    // See phase 3 K38/K40 (docs/plans/2026-09-10-phase3-winfsp.md).
    //
    // Hand-written instead of winfsp::build::winfsp_link_delayload():
    // that helper sits behind the winfsp `delayload` feature, and an
    // optional build-dependency cannot be cfg'd from within the build
    // script (build scripts do not see their own package's features).
    //
    // Until WF4 gives the CLI an actual call into cloudkit-winfsp, the
    // link prints `LNK4199: /DELAYLOAD:winfsp-x64.dll ignored; no
    // imports found` — that warning is the *correct* report of an exe
    // with no WinFsp import yet, not a broken link.
    if std::env::var_os("CARGO_FEATURE_WINFSP").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }
}
