# Unlock the profile with a passkey through the PRF extension

* Status: accepted
* Deciders: project owner
* Date: 2026-11-05

Technical Story: on 31 October 2026 the owner asked for passkeys to unlock
profiles, and on 5 November 2026 asked for this record. On 5 November the owner
also made these decisions:

* Passkey unlock is in the first release, which must include macOS and Linux
  packages.
* Linux supports USB security keys.
* The relying party is a subdomain of `omg-software.org`.
* The owner supplies the Apple account details.
* Synced passkeys are allowed.
* If a gate fails on one OS, that OS's package ships with the passphrase only, and
  the release does not wait. The packages for the other OSes ship with the feature.

The owner accepted this record on 5 November 2026. The acceptance covers the design.
The feature is not implemented, the Linux crate is not chosen, and the gates are open.

## Context and Problem Statement

ADR-0006 requires a passphrase to open the profile. An Argon2id key seals the
store key, and the OS vault wraps the sealed form (ADR-0006, ADR-0010,
ADR-0011). The user must type the passphrase at each start and each Unlock. An
attacker who has the key file and the vault can guess offline. The real limit
is the Argon2id cost and the passphrase strength.

ADR-0006 deferred Windows Hello, for three reasons. Hello only signs, so a
wrapping key needs a construction close to custom cryptography. A gate in front
of the vault adds nothing. A key is not bound to Persephone.

The WebAuthn PRF extension (`hmac-secret` in CTAP2) removes the first reason.
The authenticator returns a 32-byte secret for an input, after user
verification. The WebAuthn Level 3 text names symmetric encryption keys as a
use. The second reason does not apply when the secret seals the key. The third
reason stays.

Can a passkey open the profile, on which authenticators, and what happens to
the passphrase? In this record, "passkey" means a WebAuthn credential on a
platform authenticator or on a security key.

## Decision Drivers

* The secret that opens the store key must come from the authenticator. A
  yes/no answer from an OS API is not sufficient.
* Sessions, keys and the secret stay in the core (ADR-0004). The web view and
  `src-tauri` must not receive the PRF output, even if that is easier on macOS.
* No custom cryptography. Reviewed crates and platform APIs only, with explicit
  features and the committed lockfile (ADR-0002).
* Fail closed. A missing, cancelled or wrong passkey never opens the profile
  and never changes it.
* A lost or broken passkey must not lose the profile.
* Each OS keeps its vault as the outer layer (ADR-0006, ADR-0010, ADR-0011).
* The `restart_required` rule does not change (`docs/architecture/sdk-lifecycle.md`).
* The owner requires this feature in the first release, with macOS and Linux
  packages. `docs/planning/project-plan.md` records the request. The first-release
  list in `AGENTS.md` does not name the later owner requests, and this is one.
* A package must not offer this feature on an OS where its gates have not passed.

## Considered Options

* **A. No change.** The passphrase only.
* **B. PRF slot beside the passphrase.** Either secret opens the store key.
* **C. PRF slot instead of the passphrase.** Only a passkey opens the store key.
* **D. PRF and passphrase together.** Both secrets are needed.
* **E. A passkey signature as a gate**, with no key derived from it.
* **F. An OS key with a presence policy** (ADR-0006, option B).

## Decision Outcome

Chosen option: "B. PRF slot beside the passphrase", because it removes the
typed secret from normal use on all three OSes, it keeps the recovery path of
ADR-0006, and it does not change the existing key file. Options C and D can be
decided later on top of it.

### Slot file and cryptography

* `store-key.v2` and the passphrase rules of ADR-0006 do not change. The store
  key does not change, and the stores are not re-encrypted.
* A new file, `store-key-passkey.v1`, holds a magic line and a vault blob. The
  core checks a limit of 8 KiB before it reads the file.
* Inside the blob are a format byte, one random 32-byte PRF input for the file,
  and 1 to 4 slots. Each slot has a credential ID (1 to 1023 bytes, with its
  length), a 24-byte random nonce, and the store key sealed with
  XChaCha20-Poly1305. The associated data is the format byte, the PRF input,
  the credential ID and the nonce.
* The key-encryption key is the 32-byte PRF output. There is no extra KDF: the
  input is random and used only for this file, and each credential has its own
  PRF. Any other output length is a failure. Review can add HKDF if it finds a
  reason; `hkdf` is in the lockfile but is not a direct dependency of the core.
* The PRF input is inside the vault blob. WebAuthn Level 3 notes that an
  unpredictable input stops a person who has short access to the authenticator
  from learning the key without that input.
* The attestation type is `none`. The core stores no public key and does not
  verify the assertion signature, because no server takes part. The AEAD check
  is the proof. A fake authenticator cannot produce the output that opens a
  slot.
* User verification is required for each creation and assertion. WebAuthn
  Level 3 selects the PRF that needs user verification. On Linux, the core
  requests the same PRF directly.

### Relying party

* The relying party ID is `persephone.omg-software.org` on all three OSes, as one
  constant. The owner chose a subdomain, which keeps these credentials apart from
  any later web login on `omg-software.org`. Windows and Linux use the same
  ID.
* The profile does not move between OSes, because the vault is per OS. The one
  ID gives one name in OS prompts and one rule to review.
* A credential is fixed to its ID. If the ID changes later, every slot stops
  working, and each user must use the passphrase and add the passkey again.
* The Microsoft header states no limit on the `pwszRpId` string. So the ID does
  not bind a credential to Persephone on Windows. We assume the same for Linux,
  where the core talks to the key directly. See "Negative Consequences".

### Synced passkeys

* The owner decided to allow synced passkeys. A provider can copy a passkey to the
  user's account with that provider, and so copy the secret behind its PRF. macOS
  platform passkeys use iCloud Keychain. A password manager that the user selects as
  a passkey provider does the same in its own account. A USB security key does not
  sync.
* The vault item and the profile file are not part of that copy, so an attacker with
  the provider account also needs the profile file and the vault.
* The core reads the backup-eligible flag at the add and warns in a native prompt. That flag is not verified against attestation, so it is advice
  only, and it does not block the add.

### Add and remove a passkey

* The page only asks the core to start. The two new commands,
  `add_unlock_passkey` and `remove_unlock_passkeys`, have no key material and
  no free-form input. The page sees only whether the feature is supported and
  the number of slots.
* The steps to add a passkey are, in this order:
  1. The core creates a credential, with PRF enabled.
  2. The core makes one assertion, which gives the PRF output. WebAuthn
     Level 3 states that creation may not return an output.
  3. The core asks for the passphrase in its native prompt, under the limits
     of ADR-0006. `store::open` drops the store key after it opens the stores,
     so the key must be unsealed again.
  4. The core seals the key, writes a temporary file, reads it back, opens the
     new slot, compares the key, and then renames the file.
* The store key is unsealed only for the last two steps, and no prompt is open
  while it is in memory after the passphrase check.
* A missing PRF output or missing user verification ends the action with a
  named result (`prf_unavailable`, `no_user_verification`), and nothing is
  written. The credential that remains on the authenticator has no use, and
  the core tells the user this.
* The core holds no session lease during a prompt, as in ADR-0030. A lock or an
  exit cancels the action, and the worker drops any secret first.
* "Remove all passkeys" deletes the file after a native Yes/No warning. The
  store key does not change, so a copy of the file in a backup still opens with
  that passkey and vault. A user who needs a clean break signs out and signs in
  again, which makes a new key, as in ADR-0006.
* The slots carry no names, which keeps metadata low. The core therefore offers
  no action that removes one passkey. This can change later.

### Unlock

* The passkey request runs on the unlock worker of ADR-0006. The serving loop is
  never blocked, and Lock, Unlock and Reset return `unlocking` while it runs.
  This also applies at start, before the window opens.
* `store-key.v2` is read and unwrapped first, as today. If it is damaged, the
  state is `key_unreadable`, even with a good slot, because the passphrase is the
  recovery path.
* If the slot file is present and valid, the core makes one assertion, with all
  credential IDs in the allow list and a timeout of 60 seconds. The response
  names the credential, and that slot alone is tried.
* The OS prompt or the core's own prompt has Cancel. These cases all go to the
  passphrase prompt: Cancel, a timeout, no device, a failed request, a missing or
  wrong-length output, an AEAD failure, and a malformed slot file. A malformed
  file is not changed or deleted. The passphrase rules of ADR-0006 then apply, so
  five wrong entries end in `secret_rejected`. A cancelled passphrase prompt
  leaves `locked`.
* No store opens until the key is held. A passkey success has the same next step
  as a passphrase success: the stores must accept the key.

### Windows

* The core calls `webauthn.dll` through a new feature of the pinned crate:
  `Win32_Networking_WindowsWebServices` in `windows-sys =0.61.2`. No new crate is
  needed. The binding has `bEnablePrf`, `pHmacSecretSaltValues`, `pHmacSecret` and
  `WebAuthNCancelCurrentOperation`.
* The binding stops at API version 7, and Microsoft's header is at version 9. The
  version 8 field `pPRFGlobalEval` gives PRF at creation, which this design does
  not need.
* The core requires `WebAuthNGetApiVersionNumber()` to return 6 or more, because
  `bEnablePrf` arrives with that API version. Otherwise the feature is unsupported.
* Windows Hello and security keys both use the OS prompt.
* **Weak evidence:** Microsoft's header and its passkeys page do not state PRF
  support for Hello. Only third-party reports do (Windows 11 25H2, build 26200).
  The add action therefore tests the real credential, and refuses with
  `prf_unavailable`. Gate 1 below must confirm it.
* The call needs an owner window. The core's current dialogs must be checked for
  this during implementation.

### macOS

* The associated domain is `persephone.omg-software.org`. Apple's documentation
  states that a passkey request returns an error without an associated domain of
  the `webcredentials` type.
* On 5 November 2026 `persephone.omg-software.org` did not resolve, although
  `https://omg-software.org/` answered 200. The name needs a DNS record and an HTTPS
  certificate. It must also serve `/.well-known/apple-app-site-association`, with a
  `webcredentials` entry for the core's App ID.
* **The core needs a new package layout.** The core is a standalone sidecar
  (`docs/release.md`). TN3125 states that a standalone executable cannot claim a
  restricted entitlement, and that it must be wrapped in an app-like structure.
  The core would become a nested bundle with its own App ID and embedded profile.
  We infer that the associated-domains entitlement is restricted, from Apple's
  sample setup; gate 3 must confirm this and the nested behavior.
* The `persephone-ui` bundle must not make this call, even if the signing is
  easier there, because the PRF output must stay out of the keyless host
  (ADR-0004).
* Apple's macOS capability table lists associated domains for Developer ID. The
  owner supplies the Apple account details: the Team ID, the App ID and the Developer
  ID provisioning profile. No such values go in the repository. The release workflow
  reads the profile from a new secret, and must sign the new layout.
  `docs/release.md` now says the app uses no entitlements, so it must change. A
  profile has an expiry date, which the release process must track.
* Availability is checked at run time, because the app's minimum is macOS 13.0.
  Apple's pages list PRF on a platform assertion request at macOS 15.0, and PRF on
  a security key assertion request at macOS 26.4. Below those versions, that
  authenticator type is unsupported.
* Development and unsigned builds have no entitlement, so the feature is
  unsupported there. This matches the limit in ADR-0011.
* New dependency: `objc2-authentication-services =0.3.2`, which matches the
  `objc2-foundation` version in the core. It is not in the lockfile. Requests run
  on the main thread's AppKit run loop (ADR-0011), and the worker waits.
* The entitlement document states that macOS 11 and later request the association
  file through Apple's CDN. That request is made by the OS and does not use the
  profile's proxy (ADR-0028). `PRIVACY.md` must state this.

### Linux

* The owner decided that Linux supports USB security keys. There is no platform
  passkey, and this record adds no Bluetooth, NFC or hybrid transport.
* The core uses CTAP2 over USB HID. It sets the salt to
  `SHA-256("WebAuthn PRF" || 0x00 || input)`, as WebAuthn Level 3 specifies. It
  requests user verification.
* A key with no PIN or built-in verification is refused. The core does not set or
  change a PIN.
* The PIN is asked in the core's GTK prompt (ADR-0010), and is held in zeroizing
  memory, limited to 63 bytes, and never stored. A wrong PIN uses the key's own
  retry count and can block the key. So the core never repeats a PIN by itself,
  and it stops at a blocked or temporarily blocked error and goes to the
  passphrase prompt.
* A GTK window asks for the touch and has Cancel, which cancels the CTAP request.
* The bundles are AppImage, `.deb` and `.rpm`, so no Flatpak or Snap portal
  applies. The core needs access to `hidraw`. systemd's `60-fido-id.rules` tags
  security tokens. Whether a logged-in user gets access on the target
  distributions is not verified. This change adds no root rights, no setuid
  helper and no installed udev rule. A key that the core cannot open gives "no key
  found", and the passphrase prompt follows.
* **The crate is not chosen.** The owner must accept one, as for `argon2` in
  ADR-0006. It goes under a Linux-only target with a pinned version and explicit
  features. It must support `hmac-secret` with user verification, PIN protocol 2
  and cancellation, and it must bound what it reads from a device. Data from a USB
  device reaches the key-holding core, so memory-safe parsing is preferred. The
  candidates on crates.io on 5 November 2026 follow. The READMEs read for the first
  three do not state `hmac-secret` support, so review must check the source.
  * `ctap-hid-fido2` 3.6.1 (updated 2 November 2026)
  * `webauthn-authenticator-rs` 0.6.2 (updated 31 October 2026)
  * `authenticator` 0.5.0 (updated 22 October 2025)
  * `fido2-rs` 0.6.0 (updated 7 September 2026), a binding to the C library
    libfido2; its README was not read
* `sha2` is in the lockfile but would be a new direct dependency on Linux, unless
  the chosen crate provides the hash.

### Reset, sign-out and older builds

* The new file joins `KEY_FILES` in `crates/core/src/store.rs`, so reset and
  sign-out delete it with the other key files.
* A build from before this record ignores the file, and the passphrase still
  works. It does not delete the file at sign-out or reset. So a slot file without
  `store-key.v2` is deleted at open and is never used. The store key in such a file
  is the old key, and the stores it opened are wiped.
* A forgotten passphrase with a working passkey still opens the profile. This
  record adds no way to change the passphrase from that state.

### Gates before a package offers the feature

This record does not establish security or readiness. A package can offer the feature
only after the gates for its OS pass. The results must go in `docs/evidence/`, with
the OS, build type, versions and date. ADR-0011 lets a macOS release be marked as not
tested live. That mark does not meet gate 3, so a person must run this check on a Mac
before the macOS package offers the feature.

1. Windows Hello: a real add and unlock on Windows 11, which confirms PRF, the
   minimum build, and the result on an older build.
2. Windows: a real add and unlock with a security key, with no elevation.
3. macOS: a signed and notarized nested core bundle, with the association file
   published, which adds and unlocks with both authenticator types. No signed and
   notarized macOS build has run yet (ADR-0011), so this gate also covers that.
4. Linux: a real add and unlock with a USB key from the AppImage and from a package
   on each supported distribution family, as a normal user.
5. The Linux crate review, and `python tools/dev.py audit` with the new
   dependencies.
6. Behavior tests with a stand-in authenticator, in the form of `KeyProtector`, run
   on each OS. They must cover every changed byte, a wrong output, a wrong output length,
   more than four slots, an oversized file, an unknown credential, each fallback
   to the passphrase, a lock during an add, a slot file without `store-key.v2`,
   and reset. CI cannot run real authenticators, so tests alone do not meet gates
   1 to 4.

### A failed gate

The owner decided that a failed gate does not hold the release. The package for that
OS ships with the passphrase only, and the packages for the other OSes ship with the
feature. These rules apply that decision:

* A gate that was not run counts as failed, because an unrun check is not a pass. A
  package offers the feature only when `docs/evidence/` records that its gates passed.
* Gates 1 and 2 apply to Windows, gate 3 to macOS, and gate 4 and the crate review to
  Linux. The audit in gate 5 applies to each OS that uses the dependency. The result
  of gate 6 applies to the OS that it ran on. A defect in shared code fails gate 6 on
  each OS where it appears, and only on those.
* A partial result is a failure for that OS. If Windows Hello fails and security keys
  pass, the Windows package ships with the passphrase only. A later decision can split
  this by authenticator type.
* The feature is off at build time for that OS, not through a run-time or server
  setting. The package has no add action and makes no passkey request at unlock, and
  the page sees that the feature is not supported.
* A macOS package without the feature keeps the current layout, with no entitlement,
  no nested core bundle and no association file request. A Linux package without the
  feature does not compile the CTAP2 crate.
* The file stays in `KEY_FILES` in every build, so reset and sign-out still delete a
  slot file that a test build wrote. A build without the feature otherwise ignores it.
* The release notes and the evidence record state which package lacks the feature and
  which gate failed.
* A later release can turn the feature on with no migration, because the slot file is
  added beside `store-key.v2`.

This rule covers the passkey feature only. It does not waive a check that the package
itself needs. Gate 3 also covers the first signed and notarized macOS build, and
removing the feature does not repair a failure in that part. ADR-0011 and the release
process then decide the state of the macOS package.

### Positive Consequences

* Normal unlock needs a touch, a PIN or a biometric, and no typed passphrase. A
  keylogger sees no passphrase in normal use.
* A passkey with a hardware authenticator limits PIN guessing in that hardware.
* A forgotten passphrase is less of a risk while a passkey works.
* Existing profiles and older builds keep working, and there is no migration.
* The vault stays as the outer layer on each OS.

### Negative Consequences

* **Offline guessing stays possible.** The passphrase slot remains, so a person
  with the key file and the vault can still guess at Argon2id cost. Option B adds
  convenience and does not raise that limit. Options C or D are needed to raise it.
* **The third ADR-0006 objection stays.** A process that runs as the user can read
  the PRF input through the vault, request an assertion for the same relying party
  and show a real OS prompt. If the user approves it, the process gets the key. The
  user cannot tell which process asked. This is similar to a fake passphrase
  prompt, and it does not protect an open profile.
* Users may type the passphrase rarely and forget it, so a lost passkey then costs
  the local profile. The add action must warn about this.
* The profile depends on a domain on macOS. If the domain or the association file
  is lost, passkey use on macOS can stop, and the passphrase still works.
* There are three platform paths, a new macOS package layout, and new dependencies
  on macOS and Linux. Each needs review and live checks that CI cannot run. All of
  this is planned for the first release, which adds schedule risk. A failed gate does
  not hold the release, but the work done for that OS then ships unused.
* The first release may offer the feature on some OSes and not on others, so unlock
  can differ between packages of the same version. The documentation must state this
  for each package.
* The feature on macOS depends on the owner's Apple account details and on the
  subdomain, and neither is in place yet. Without them, gate 3 fails and the macOS
  package ships with the passphrase only.
* A synced passkey adds the provider and its account to the people and systems that
  can obtain the PRF secret. A person who has that account, the profile file and the
  vault can open the profile.
* Copies of the PRF output in the OS and in crates are outside the zeroization
  guarantee, as for the passphrase in ADR-0006.

## Pros and Cons of the Options

### A. No change

* Good, because it adds no code, dependency or platform work.
* Bad, because the user types a secret at each unlock, which encourages a weak one.
* Bad, because it does not meet the owner's first-release requirement.

### B. PRF slot beside the passphrase

* Good, because the authenticator supplies the key, with a method that WebAuthn
  specifies.
* Good, because a lost passkey does not lose the profile.
* Good, because there is no migration, and older builds still open the profile.
* Bad, because the security of a copied file does not improve.
* Bad, because it has the platform costs listed above.

### C. PRF slot instead of the passphrase

* Good, because offline guessing is no longer possible.
* Bad, because a lost authenticator loses the local profile, unless the user has
  added a spare. ADR-0006 then requires key backup and the recovery key.
* Bad, because many computers have no authenticator, so it cannot be the only path.

### D. PRF and passphrase together

* Good, because an attacker needs both, so it gives the strongest protection for a
  closed profile.
* Bad, because the user still types a passphrase, and also needs the authenticator.
* Bad, because the loss of either one loses the local profile.

### E. A passkey signature as a gate

* Good, because it needs no PRF support, so all authenticators work.
* Bad, because the check is in the core's own code and no key depends on it. A
  process that runs as the user can skip the check and read the file (ADR-0006).

### F. An OS key with a presence policy

* Good, because it can be hardware-bound.
* Bad, because of the reasons in ADR-0006: it needs a design for each OS, and no
  key is bound to Persephone.
* Bad, because it gives no path for USB security keys on Linux.

## Links

* Refines [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md):
  adds a second way to open the store key, and revisits its option B reasoning.
* Relates to [ADR-0002](0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md),
  [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md),
  [ADR-0010](0010-linux-platform-layer.md),
  [ADR-0011](0011-macos-platform-layer.md),
  [ADR-0012](0012-sign-and-publish-releases-as-drafts.md),
  [ADR-0028](0028-route-every-connection-through-the-profiles-socks5-proxy.md) and
  [ADR-0030](0030-sign-in-with-oidc-through-the-browser-and-a-loopback-redirect.md).
* Sources read on 5 November 2026:
  * [WebAuthn Level 3, PRF extension](https://www.w3.org/TR/webauthn-3/#prf-extension)
  * [Microsoft `webauthn.h`](https://github.com/microsoft/webauthn/blob/master/webauthn.h)
  * [Microsoft passkeys overview](https://learn.microsoft.com/en-us/windows/security/identity-protection/passkeys/)
  * [Apple: Supporting passkeys](https://developer.apple.com/documentation/authenticationservices/supporting-passkeys)
  * [Apple: associated domains entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.associated-domains)
  * [Apple: TN3125, provisioning profiles](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles)
  * [Apple: supported capabilities on macOS](https://developer.apple.com/help/account/reference/supported-capabilities-macos)
  * [systemd `60-fido-id.rules`](https://github.com/systemd/systemd/blob/main/rules.d/60-fido-id.rules)
* Third-party reports on Windows Hello PRF, not confirmed by Microsoft:
  [Corbado](https://www.corbado.com/blog/passkeys-prf-webauthn) and
  [Bitwarden community](https://community.bitwarden.com/t/encryption-prf-via-windows-hello-passkey/94236/21).
