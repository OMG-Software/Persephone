# Security policy

Last updated: 24 October 2026

Persephone is in early development. Its security gates are still open, and it
is not ready for high-risk use. We still want to hear about every security
problem you find.

## Report a vulnerability privately

Use GitHub's private vulnerability reporting:

1. Open the repository's [Security](https://github.com/OMG-Software/Persephone/security)
   tab.
2. Select **Report a vulnerability**, or go directly to
   <https://github.com/OMG-Software/Persephone/security/advisories/new>.
3. Describe the problem. Only you and the project's maintainers can see the
   report.

Do not report a vulnerability in a public issue, pull request or discussion.

## What to include

* What is affected: the app, a release download, the release workflow, or the
  website. Give the version, commit or release tag, and the operating system.
* What an attacker can do, and what they need first (for example, a
  malicious homeserver, a malicious contact, a compromised web view, or local
  access).
* Steps to reproduce, or a proof of concept.

Use synthetic accounts and test data only. Do not send real messages, real
keys, passwords, recovery keys or access tokens, yours or anyone else's.

## What is in scope

* The Persephone desktop app: the core, the UI process, the media worker and
  the WebAssembly decoder.
* The release workflow and the published release files, including their
  signatures and checksums.
* The project website.

Report problems in the Matrix Rust SDK, Tauri, a homeserver or another
dependency to that project as well. Tell us if Persephone is affected.

## What happens next

The maintainers reply in the private advisory, work on a fix there, and agree
a disclosure date with you. The fix and the advisory are then published.
Persephone has no bug bounty.

## Supported versions

There is no stable release yet. Fixes go to the `main` branch and to the next
pre-release. Older pre-releases are not fixed.
