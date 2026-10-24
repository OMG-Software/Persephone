# Encrypted text with Element Web

Checked 24 October 2026 on Windows 11 Pro x64 (build 26300), debug builds of
Persephone at commit `e38007b` (Rust 1.99.1, Matrix Rust SDK 0.19.1),
against disposable Merovingian 0.12.29 and Synapse 1.163.0 in WSL Ubuntu
24.04. The other client was Element Web v1.12.32 in the Claude desktop app's
built-in browser (Chromium). This closes the "another Matrix client" P4 exit
item for Windows only. No product code changed.
**Result: on both servers, Persephone and Element Web verified each other by
emoji in an encrypted direct-message room and read each other's encrypted
text. A new Element session that Bob had not cross-signed got no key from
Persephone (`withheld`); after Bob cross-signed it, it read Persephone's next
message and not the earlier one.**

## Inputs

| Item | Value |
| --- | --- |
| Element archive | `element-v1.12.32.tar.gz`, 53,236,279 bytes, SHA-256 `27599ca285d85edd841c04b5c662ce4e773f562bc98090b4b9e2d58cff6b308c` |
| Signature | `element-v1.12.32.tar.gz.asc` (833 bytes): `gpg --verify` exit 0, "Good signature from Element Releases", signing subkey `E95B 7699 E80B 68A9 EAD9 A19A 2BAA 9B85 52BD 9047` |
| Signing key | `https://packages.element.io/element-release-key.asc`, primary fingerprint `712B FBEE 92DC A452 52DB 17D7 C7BE 97EF A179 B100`. The key came from Element's own site and is not certified by any other key, so the check only shows that the archive matches the key that site publishes. |
| Element crypto | Element's console named `matrix-sdk-crypto-0.19.1`, the same crypto crate version as Persephone. |
| Persephone | `persephone.exe` with `--features dev-disposable-servers`, `persephone-desktop`, `persephone-media-worker` and `p4_peer`, built from the worktree with `--locked --offline` (debug). |

The archive was unpacked into its own directory; no script from it ran on the
host. A small static server (`serve.py`, kept outside the archive directory)
served the files on `127.0.0.1` only and answered `/config.json` with a
configuration that names the disposable homeserver and turns off
integrations, widgets, VoIP, Element Call, URL previews, image display,
identity server, analytics opt-in, registration and federation by default. It
sets no PostHog, Sentry or bug-report endpoint. The first page load requested
only `127.0.0.1` addresses; later Element traffic was not checked for other
hosts.

## Method

One harness start per server (`--server merovingian`, then `--server
synapse`). Persephone signed in as the harness account `p0_alice` ("A") and
set up encryption from Settings; the driver (`pctl.py`) acts on the real page
through WebView2's DevTools port, one action per call. Element signed in as
`p0_bob` ("B") by typing the synthetic credentials. Each new Element session
used a new origin (`127.0.0.1:<port>`), so it had its own browser storage
and was a new device.

Persephone cannot accept invitations yet. Element created the
direct-message room with its first message, and a second device of A (the
SDK peer, `login alice`, `alice-join-dm <room>`, `quit`) joined it and marked
it as direct, as in the [contact verification runs](contact-verification.md).
Element states were read from its page and its client object
(`mxMatrixClientPeg`) in the browser's JavaScript console; screenshots of
Persephone are in `.local/` only.

Before step 4 Bob's key storage (server key backup) was turned off in his
first Element session, so that a new session could get a key only from the
sender (on Merovingian this was done for the second new session, 4c below).

## Results

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| 1. Sign-in and set-up | pass. Persephone: identity verified, recovery on. Element: cross-signing set up on sign-in after a password prompt. | pass. Element set up cross-signing without a password prompt. |
| 2. Encrypted DM | pass. Room encrypted; Persephone lists it with B as contact. | pass |
| 2. Emoji verification | pass, started from Persephone's header. Same 7 emoji on both sides; both confirmed; Persephone "is verified", contact `verified`. Element marked A's identity verified only after a later key query (finding 1). | pass, started from Element ("Verify User", "Start Verification"). Persephone showed the request, accepted; same 7 emoji; both done; Element saw A cross-signing verified at once. |
| 3. A to B | pass. Element read `p4e-M1` decrypted (wire type `m.room.encrypted`, no warning shield, no decryption failure). | pass (`p4e-S1`) |
| 3. B to A | pass. Persephone showed `p4e-M2` as a message (not a notice), room marked encrypted, no untrusted-sender notice. | pass (`p4e-S2`) |
| 4a. B signs in a new session, not yet cross-signed; A sends | pass. New session: `MEGOLM_KEY_WITHHELD_FOR_UNVERIFIED_DEVICE` for `p4e-M3`; Bob's first session read it. | pass (`p4e-S3`, same) |
| 4b. B cross-signs the new session by emoji from his first session; A sends | `p4e-M4` read on the new session; `p4e-M1` and `p4e-M3` also became readable from the key backup that was still on. Not conclusive (finding 3). | pass. `p4e-S4` read on the new session; `p4e-S3` stayed withheld and earlier messages read `HISTORICAL_MESSAGE_NO_KEY_BACKUP`. |
| 4c. Key storage off; a third session, then cross-signed | pass. `p4e-M5` withheld before; after cross-signing, `p4e-M6` read and `p4e-M5` stayed withheld. | not run (4b already ran without key storage) |
| 4. What Persephone shows | Nothing about Bob's new device: the header stays "Verified" and sending is not paused. | same |
| Servers stopped | runner exit 0 | runner exit 0 |

In 4b and 4c all of A's messages used the same Megolm session as the
withheld one. A session key also opens later messages of the same session,
so the new device could read the next message only because Persephone sent
it the session from that message on. The withheld message stayed closed to
it.

### Findings

1. **Element showed A as not verified after a successful verification on
   Merovingian.** Element's log said the flow "verified the identity of the
   remote user" and its signature upload returned no failures, and a key
   query from the page showed Bob's user-signing signature on A's master key.
   Element's status still read not verified until A's device list changed
   (when the stale peer device below was deleted). Inference: a key query
   that was answered before the signature was stored replaced Element's local
   copy. On Synapse the status was verified at once. No Persephone defect was
   seen here.
2. **Merovingian kept a logged-out device's keys.** After the peer's
   `quit` (logout), `/devices` still listed the peer device. After it was
   deleted with `POST /delete_devices` (password round), `/devices` no longer
   listed it, but `/keys/query` still returned its device keys, and Element
   kept it as an unverified device of A. Synapse removed the device on logout.
   Element shares keys with unverified devices by default, so on Merovingian
   Element may still encrypt to that removed device. This is a server finding
   from the test set-up, not a Persephone defect.
3. **Key backup can hide the result of 4b.** With Bob's key storage on, the
   new session restored the earlier messages from the backup after it was
   verified, so it could not show where the key of the next message came from.
   Runs 4c (Merovingian) and 4b (Synapse) ran without key storage.
4. **Persephone UI observations (not defects of the tested properties):**
   * The in-room verification request shows in the timeline as "A kind of
     message Persephone does not show yet."
   * After Persephone accepts a request that Element started, the page says
     "Waiting for … to accept the request." while Element waits for its user
     to start the emoji comparison. The words describe the wrong party.
   * The direct-message row in the room list shows the `#` room icon.

### Attempts not counted

* Synapse, first Persephone start: the core printed `the window failed:
  WebViewGone` before the page could be reached, and then waited (window
  retry). The UI and core processes were stopped; this account had not signed
  in. The second start, with a new profile directory, ran every step. MSI
  Afterburner and RTSS were running, as in earlier runs; no crash dump was
  checked, so the cause is not known.

## Commands

From the repository root (the worktree). `<pw>`, `<room>` and ports are
placeholders; credentials stay in the harness `profile.json` under `.local/`.

```text
mkdir .local/element-web/download-v1.12.32 ; cd there
curl -fsSL --proto =https --tlsv1.2 -O https://github.com/element-hq/element-web/releases/download/v1.12.32/element-v1.12.32.tar.gz
curl -fsSL --proto =https --tlsv1.2 -O https://github.com/element-hq/element-web/releases/download/v1.12.32/element-v1.12.32.tar.gz.asc
curl -fsSL --proto =https --tlsv1.2 -o .local/element-web/key/element-release-key.asc https://packages.element.io/element-release-key.asc
gpg --homedir .local/element-web/gnupg --import .local/element-web/key/element-release-key.asc
gpg --homedir .local/element-web/gnupg --verify element-v1.12.32.tar.gz.asc element-v1.12.32.tar.gz
tar -xzf element-v1.12.32.tar.gz -C .local/element-web/unpacked-v1.12.32 --no-same-owner

npm ci --no-audit --no-fund --offline ; npm run build            # in frontend/
cargo build --locked --offline --release -p persephone-media-decoder --target wasm32-unknown-unknown
cargo build --locked --offline -p persephone-media-worker -p persephone-desktop -p persephone-test-matrix --bins
cargo build --locked --offline -p persephone-core --features dev-disposable-servers --bin persephone

wsl -d Ubuntu-24.04 -u root -- /usr/bin/setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all --reuid=1000 --regid=1000 --init-groups --reset-env --no-new-privs /usr/bin/python3 /mnt/c/dev/Persephone/tools/servers/run.py --merovingian-binary /mnt/c/dev/Persephone/.local/servers/merovingian/merovingian-server --synapse-python /mnt/c/dev/Persephone/.local/servers/synapse/bin/python --server <merovingian|synapse> --output /mnt/c/dev/Persephone/.local/runs/p4-element-01/<server>

python -I serve.py .local/element-web/unpacked-v1.12.32/element-v1.12.32 config-<server>.json <port>

PERSEPHONE_DEV_PROFILE=<profile.json> PERSEPHONE_DEV_PASSPHRASE_FILE=<.local file>
PERSEPHONE_DEV_RECOVERY_FILE=<.local file> WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>
.local/build/rust/debug/persephone.exe --profile <new .local directory>

python -E -P -X utf8 pctl.py <port> signin|setup|open <room>|click <label>|send <text>|timeline <room>|contact <room>|emoji
p4_peer.exe <profile.json> <server>   < login alice / alice-join-dm <room> / quit
python -I devices.py <profile.json> <server> alice list|delete <device id>
wsl -d Ubuntu-24.04 -u root -- kill -TERM <runner pid>
```

Raw output, the scripts, the Element configuration and Persephone
screenshots are in `.local/runs/p4-element-01` (`merovingian`, `synapse`,
`merovingian-run`, `synapse-run`, `scripts`). The Element sessions' browser
storage was deleted at the end of each server run.

## Limits

* Windows only, debug builds, one host. Linux and macOS, and the hand checks
  of native windows, are not covered.
* One Element version in one Chromium-based browser. Element's service worker
  did not load ("Failed to load service worker"); it is needed only for
  authenticated media, which text messages do not use.
* The direct-message room was joined for A by a second SDK device, because
  Persephone has no invitation UI. On Merovingian that device's keys stayed on
  the server (finding 2).
* The Element side was driven through its page and inspected through its
  JavaScript client object; the comparison of emoji was made by the test
  operator reading both screens, not by two people.
* Persephone shows nothing about a contact's new, unsigned device; that the
  device got no key was observed on the Element side only.
* Not rerun. The ledger (`verification.json`) and the documentation index are
  not updated by this report.
