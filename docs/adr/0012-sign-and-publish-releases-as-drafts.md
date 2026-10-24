# Sign releases and publish them as drafts

* Status: accepted
* Deciders: project owner
* Date: 2026-10-24

## Context and Problem Statement

Persephone needs builds that people can install and check. Users at high risk
must be able to verify that an installer came from this repository and was not
changed. The project's security gates are still open, so a release must not
look more ready than it is. Persephone needs a rule for what is built, how it
is signed, and how a release becomes public.

## Decision Drivers

* Every artifact can be traced to a commit and a workflow run.
* Signing keys are not kept in the repository, and the agent never handles them.
* Nothing becomes public without a person deciding to publish it.
* Release notes state the open gates and untested platforms.

## Considered Options

* Windows signing: Azure Artifact Signing; a PFX certificate in secrets;
  SignPath.
* Linux signing: Sigstore keyless; GPG; both.
* macOS: one universal package; separate packages per architecture.
* Publishing: a draft on tag; a published pre-release on tag; artifacts only.

## Decision Outcome

Chosen options (owner decisions, 24 October 2026):

* **Targets:** Windows x64 installer; Linux x64 and arm64 AppImage, .deb and
  .rpm; one universal macOS .dmg.
* **Windows: a PFX certificate in GitHub secrets, applied with `signtool`.**
  SignPath's open-source programme was chosen first, but it does not accept a
  project this new, and Let's Encrypt does not issue code-signing
  certificates. The executables and the installers are signed with an RFC 3161
  timestamp. A publicly trusted certificate needs a hardware-backed key and
  cannot be exported as a PFX, so this certificate is expected to be
  self-signed: it shows that releases share one key, but Windows still shows
  an unknown publisher. The release notes give the certificate's thumbprint,
  say when it is self-signed, and tell users never to add it to their trusted
  roots. The owner intends to move to a cloud signing service with a publicly
  trusted certificate (for example Azure Artifact Signing or SignPath) once
  the project qualifies; the signing step is kept in one place so that only it
  changes.
* **macOS:** signed with an Apple Developer ID and notarized.
* **Linux:** a `SHA256SUMS` file over every artifact, signed with Sigstore
  (keyless, through the workflow's identity) and with a project GPG key.
* **Provenance:** a build-provenance attestation for each artifact, a CycloneDX
  SBOM, and third-party licence notices in each package.
* **Publishing:** a `v*` tag builds everything into a draft GitHub
  pre-release. The owner reviews it and publishes it by hand. A manual run
  builds unsigned artifacts only, named as unsigned, and creates no release.
* **Keys:** the owner creates the accounts and adds the secrets in GitHub. A
  tag run fails if a required secret is missing (Windows certificate, Apple or
  GPG); it never builds a release with an unsigned artifact. Signing runs in
  separate jobs (`windows-sign-binaries`, `windows-sign-installers`,
  `macos-sign`, and GPG in `checksums`) that build no code and run only the
  pinned actions, this repository's scripts and the OS signing tools on
  artifacts from earlier jobs. The jobs that build and bundle have no
  secrets. Only the signing jobs and the secret check use a GitHub
  environment, so the owner can require a review before the secrets are used.

### Positive Consequences

* Users can check every artifact through the signed checksums and its build
  provenance, and macOS artifacts also through Apple's notarization.
* No release becomes public by accident.

### Negative Consequences

* With a self-signed certificate, Windows shows SmartScreen and
  unknown-publisher warnings. Users must not install the certificate to
  silence them; doing so would let its key vouch for any code.
* The PFX private key is stored in GitHub secrets. A leak of the repository's
  secrets would let someone sign files with the same key.
* A release depends on Apple and Sigstore, and an Apple Developer account must
  be in place before the first tag.
* There is no update mechanism yet; users install new versions by hand.

## Links

* [ADR-0010](0010-linux-platform-layer.md), [ADR-0011](0011-macos-platform-layer.md)
* [Security design: release gates](../architecture/security-design.md)
