# Privacy policy

Last updated: 26 October 2026

This policy tells you what data the Persephone project collects, what the
Persephone app sends and stores, and where other parties process data. It
describes the project as it is today. Persephone is in early development and
is not ready for real conversations.

## Who this policy applies to

This policy applies to:

* the Persephone desktop app for Windows, Linux and macOS;
* this source repository, `OMG-Software/Persephone` on GitHub;
* the project website, <https://omg-software.github.io/Persephone/>;
* the release downloads that the project publishes on GitHub.

This policy does not apply to:

* **Matrix homeservers.** You choose the homeserver that you sign in to. Its
  operator processes your account and your messages under their own policy.
* **GitHub.** GitHub hosts the repository, the website and the downloads.
  GitHub processes data under the
  [GitHub General Privacy Statement](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement).

## What the project collects

The app and the project collect no data about you on their own.

* The app has no telemetry, no analytics and no usage statistics.
* The app does not upload crash reports.
* The app does not check for updates. You install a new version by hand.
* The app contacts no server that the project operates. The project operates
  no such server.

The project receives only what you choose to send it: a public issue or
pull request, or a private vulnerability report. The maintainers can read a
private report, and GitHub stores it. Send only what the report needs (see
[Contact](#contact)).

## What the app sends, and to whom

The app connects only to the homeserver that you enter at sign-in. The
sign-in screen suggests `https://matrix.org`; you can enter a different
server. The app accepts only `https://` addresses. Every connection uses TLS
1.3 or later, with certificate checks. The app does not follow redirects and
does not use a proxy that is set in the system.

The homeserver receives what the Matrix protocol needs. This includes:

* your user ID, your password at sign-in and the session's access token;
* a new device for this sign-in, with the device name `Persephone`;
* the rooms that you are in, and your room membership;
* the messages that you send;
* your device's public keys, and the room keys that the app sends, encrypted,
  to other devices;
* the images that you send;
* requests for each image that you choose to load.

If you set up a key backup or a recovery key, the homeserver also stores your
encrypted room-key backup. Persephone does not do this automatically; you
start it. Persephone shows a new recovery key once and does not store it.

The homeserver shares room data with the other servers whose users are in
the room. The other people in a room receive your messages and can see your
membership. This is how Matrix works, and the app cannot change it.

### Starting a conversation

When you start a conversation and you already have an encrypted conversation
with only that person, the app opens it and sends no request to start a new
one. Otherwise, the app needs the person's cross-signing identity. If the app
does not have it yet, it sends the Matrix ID that you enter to your homeserver
to ask for it. Only if the person has a cross-signing identity does the app
ask the homeserver to create an encrypted room and to invite them. Having
encryption keys without a cross-signing identity is not enough. The invitation
also reaches the person's homeserver, as any Matrix invitation does.

Search for people is off each time you open the dialog. If you turn it on, the
app sends what you type to your homeserver's user directory, 0.4 seconds after
you stop typing, once you have typed at least two characters. Your homeserver
can keep a record of these searches. Results show names and Matrix IDs as
text. The app fetches no avatars.

When you accept a direct-message invitation, the app joins the room. When you
decline it, the app leaves the room. Your homeserver and the person who
invited you can see either answer.

### What encryption protects

In an encrypted room, the app encrypts message content and images before it
sends them. The homeserver cannot read that content.

Encryption does not hide all metadata. The homeserver, and other servers in
the room, can still see who is in a room, who sends messages, when, and how
large the messages and files are. Persephone does not make you anonymous and
does not hide your traffic patterns.

In a room that is not encrypted, the homeserver can read your messages. The
app labels such a room and asks you to confirm before you send the first
message there. An encrypted send never falls back to plain text.

### Images

* **Received images.** The app shows a placeholder for each received image.
  It downloads the image only when you choose to load it. A download tells
  your homeserver that you opened the image. If the image came from another
  server, your homeserver can fetch it from that server.
* **Sent images.** Before it sends an image, the app makes a copy without
  location data and other metadata, for example EXIF, XMP, comments and
  colour profiles. Of the metadata, the copy keeps only the orientation tag,
  so that the photo does not appear rotated. The pixels do not change. The app
  sends the copy with a generic file name, `image.png` or `image.jpg`, so the
  original file name does not leave your device. The app never changes your
  original file.

The app does not show link previews and does not load content from the web.

## What the app stores on your device

The app keeps your account in a local profile. The profile holds the Matrix
SDK's databases: account state, the message cache, the media cache and the
encryption keys.

* A random 32-byte key encrypts the values in these databases.
* Your profile passphrase protects that key. You choose the passphrase when
  you create the profile. The app has no way to recover a forgotten
  passphrase.
* The operating system's keystore adds a second layer. On Windows this is
  DPAPI. The design uses Secret Service on Linux and the Keychain on macOS;
  these two are not complete yet. If no keystore is available, the profile
  stays locked. The app never writes an unprotected key instead.

The app keeps images that you load in memory only. It drops them when you
lock the profile or sign out.

The app does not write log files. It does not keep your login password.

### Signing out and resetting

When you sign out, the app asks the homeserver to end the session. It then
deletes the profile's key, so the remaining files cannot be read. The next
time the app starts, it deletes the profile's files.

If you forget your passphrase, "Reset profile" deletes the profile without
the passphrase. The app cannot end the old session on the homeserver in this
case. Remove the old device from another Matrix client.

### Limits

* The databases encrypt their values, not the whole files. Table names and
  some lookup keys stay visible.
* Deleting a file does not erase it securely. Copies can remain on SSDs, in
  backups, in system snapshots or in swap space.
* The app cannot protect data on a device that an attacker controls, or data
  that the other people in a room keep.
* Components of the operating system, such as the system web view, can keep
  their own files. This policy does not describe them.

## The website

GitHub Pages hosts the project website. The website's pages:

* set no cookies;
* have no analytics, no scripts and no tracking;
* load no fonts, images or other files from third parties;
* tell your browser not to send a referrer when you follow a link.

GitHub can process technical data, such as your IP address, when you visit
the website. See the
[GitHub General Privacy Statement](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement).

## Release downloads

The project publishes release downloads on GitHub Releases. GitHub can
process technical data, such as your IP address, when you download a file.

### Release signing transparency

The release workflow signs the `SHA256SUMS` file with Sigstore cosign,
without a stored key, through the workflow's own identity. The signature is
recorded in Sigstore's public transparency log. The record holds the
identity of the release workflow (the repository, the workflow file and the
release tag), not data about you. Each file also has a build-provenance
attestation, made through the same workflow identity.

## Children

Persephone is not made for children. The project collects no data from
anyone, so it holds no data about children. Your homeserver can have its own
age rules.

## Changes to this policy

This file is kept in the project's git repository. Its history shows every
change. Each version shows the date of its last update at the top.

## Contact

To report a privacy or security problem privately, use GitHub's
[private vulnerability reporting](https://github.com/OMG-Software/Persephone/security/advisories/new).
Only you and the project's maintainers can see the report. You need a GitHub
account. The [security policy](SECURITY.md) says what to include.

Ask general questions about this policy in the repository's
[issue tracker](https://github.com/OMG-Software/Persephone/issues). Issues are
public. Do not put personal data, chat content or security details in an
issue.
