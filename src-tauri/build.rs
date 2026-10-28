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
            "reset_profile",
            "sign_in",
            "register",
            "sign_out",
            "security_status",
            "set_up_encryption",
            "recover_with_key",
            "send_message",
            "edit_message",
            "delete_message",
            "react",
            "mark_read",
            "room_activity",
            "set_typing",
            "open_link",
            "pending_messages",
            "retry_message",
            "discard_message",
            "allow_unencrypted",
            "set_up_recovery",
            "verify_this_device",
            "accept_verification",
            "confirm_verification",
            "cancel_verification",
            "contact_status",
            "verify_contact",
            "identity_changes",
            "accept_identity",
            "load_image",
            "image_band",
            "release_image",
            "pick_image",
            "send_image",
            "discard_image",
            "invite_list",
            "accept_invite",
            "decline_invite",
            "start_conversation",
            "search_users",
            "create_room",
            "invite_user",
            "room_settings",
            "room_members",
            "change_room",
            "display_name",
            "set_display_name",
            "device_list",
            "account_settings",
            "set_account_settings",
            "change_password",
            "three_pids",
            "request_email_token",
            "request_phone_token",
            "add_three_pid",
            "remove_three_pid",
            "verify_device",
            "change_recovery_key",
            "notification_level",
            "set_notification_level",
            "profile_list",
            "add_profile",
            "switch_profile",
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
