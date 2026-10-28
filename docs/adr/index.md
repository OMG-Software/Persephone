<!-- Maintained by hand. Add a line here in the same commit as a new ADR. -->
# Architecture Decision Records

## Accepted Records

* [0000 - Use Tauri for the desktop client](0000-use-tauri-for-the-desktop-client.md)
* [0001 - Prefer Sliding Sync with classic sync fallback](0001-prefer-sliding-sync-with-classic-sync-fallback.md)
* [0002 - Use SDK-owned Matrix cryptography and explicit features](0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md)
* [0003 - Run untrusted media decoding in a WebAssembly sandbox](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [0004 - Separate the key-holding core from the web view process](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [0005 - Share room keys only with cross-signed devices](0005-share-room-keys-only-with-cross-signed-devices.md)
* [0006 - Require an unlock secret for the local profile](0006-require-an-unlock-secret-for-the-local-profile.md)
* [0007 - Block sending to a changed contact identity until approved](0007-block-sending-to-a-changed-contact-identity-until-approved.md)
* [0009 - Strip sent image metadata and load received images on request](0009-strip-sent-image-metadata-and-load-received-images-on-request.md)
* [0010 - Protect keys and contain processes on Linux](0010-linux-platform-layer.md)
* [0011 - Protect keys and contain processes on macOS](0011-macos-platform-layer.md)
* [0012 - Sign releases and publish them as drafts](0012-sign-and-publish-releases-as-drafts.md)
* [0014 - Start encrypted direct conversations by Matrix ID](0014-start-encrypted-direct-conversations-by-matrix-id.md)
* [0015 - Keep accounts in separate profiles and switch by restart](0015-keep-accounts-in-separate-profiles-and-switch-by-restart.md)

## Proposed Records

* [0008 - Restart the core process to finish a lock after the client ran](0008-restart-the-core-process-to-finish-a-lock-after-the-client-ran.md)
* [0013 - Open a default profile when none is given](0013-open-a-default-profile-when-none-is-given.md)
* [0016 - Create encrypted rooms and answer every invitation](0016-create-encrypted-rooms-and-answer-every-invitation.md)
* [0017 - Configure joined rooms through one closed change call](0017-configure-joined-rooms-through-one-closed-change-call.md)
* [0018 - Register accounts with secrets entered in the core](0018-register-accounts-with-secrets-in-the-core.md)
* [0019 - Change the password with secrets entered in the core](0019-change-the-password-with-secrets-entered-in-the-core.md)
* [0020 - Add email addresses and phone numbers without an identity server](0020-add-email-addresses-and-phone-numbers-without-an-identity-server.md)
* [0021 - Save the recovery key to a file from the core](0021-save-the-recovery-key-to-a-file-from-the-core.md)
* [0022 - Notify through push rules and content-free desktop notifications](0022-notify-through-push-rules-and-content-free-desktop-notifications.md)

## Superseded Records

* None

## Records with non-standard statuses

* None

Use [template.md](template.md) for new records. Follow Merovingian's MADR 2.1.2
layout: `NNNN-title-in-lowercase-with-dashes.md`, no numbers in headings, and
mandatory status and date. Add each record to this index; preserve superseded
records and their numbers.
