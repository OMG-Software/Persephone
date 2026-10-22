//! Registers the app's commands with Tauri's permission system. Only commands
//! listed here get generated `allow-*`/`deny-*` permissions; capabilities in
//! `capabilities/` decide which windows may use them.
//!
//! On Windows, `common-controls-v6` imports functions that exist only in
//! Common Controls 6, which an executable gets only through its application
//! manifest. tauri-build embeds that manifest in the app binary alone, so test
//! executables loaded version 5 and failed with STATUS_ENTRYPOINT_NOT_FOUND.
//! The same manifest is embedded here in every artifact this crate links.

fn main() {
    let mut attributes =
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "app_status",
            "room_list",
            "timeline_page",
            "subscribe_room",
            "unsubscribe_room",
            "watch_session",
            "lock_session",
            "unlock_session",
            "sign_in",
            "sign_out",
            "security_status",
            "set_up_encryption",
            "recover_with_key",
        ]));
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("windows-app-manifest.xml");
            println!("cargo::rerun-if-changed={}", manifest.display());
            println!("cargo::rustc-link-arg=/MANIFEST:EMBED");
            println!(
                "cargo::rustc-link-arg=/MANIFESTINPUT:{}",
                manifest.display()
            );
        }
    }
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
