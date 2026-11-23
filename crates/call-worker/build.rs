//! Link flags for the worker's own targets: the executable and its tests.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    // The libwebrtc Objective-C SDK adds methods to Foundation classes in
    // categories, for example `+[NSString stringForStdString:]`. The macOS
    // linker drops an archive member that holds only a category unless it
    // gets `-ObjC`. Without the flag, the first peer connection factory
    // aborts with an unrecognized selector. webrtc-sys sets `-ObjC` in its
    // own build script, but Cargo gives `rustc-link-arg` only to the targets
    // of the package that sets it, not to a package that depends on it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg=-ObjC");
    }
}
