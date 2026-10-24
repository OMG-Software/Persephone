# Release builds

This guide describes how Persephone release builds are made, signed and
published, what the owner must set up first, and how users verify a download.
The workflow is [`.github/workflows/release.yml`](../.github/workflows/release.yml).

> A signed release is not a security approval. The project has open
> feasibility and production security gates, and releases are not for
> high-risk use. A passing release run shows that the bundles were built,
> signed and checksummed. It does not show that the application is safe.

## What a release run produces

| Platform | Runner | Files |
| --- | --- | --- |
| Windows x64 | `windows-latest` | NSIS installer (per-user, `currentUser`) and MSI |
| Linux x64 | `ubuntu-22.04` | AppImage, `.deb`, `.rpm` |
| Linux arm64 | `ubuntu-22.04-arm` | AppImage, `.deb`, `.rpm` |
| macOS universal (arm64 + x86_64) | `macos-latest` | `.dmg` and the same app as `.app.zip` |

Every release also has `SHA256SUMS` over all files, `SHA256SUMS.sigstore.json`
(cosign keyless signature), `SHA256SUMS.asc` (GPG signature), a CycloneDX SBOM
(`*-sbom.cdx.json`, from syft) and the third-party notices
(`*-THIRD-PARTY-NOTICES.txt`, from cargo-about). Each file in `SHA256SUMS` also
has a GitHub build-provenance attestation.

Each bundle installs `persephone` (the core), `persephone-ui` and
`persephone-media-worker` side by side. `persephone-ui` is the bundle's main
binary; the core and worker are Tauri sidecars. Each bundle also contains
`LICENSE.txt` and `THIRD-PARTY-NOTICES.txt`.

### Jobs

1. `gate`: on a tag, `python tools/release.py check-tag <tag>`; then
   `python tools/dev.py bootstrap`, `check` and `audit`; then the third-party
   notices with cargo-about ([`tools/about.toml`](../tools/about.toml),
   [`tools/about.hbs`](../tools/about.hbs)).
2. `preflight`: on a tag, fails if any signing secret is missing. It names the
   missing secrets and never prints values.
3. `windows`, `linux` (x64 and arm64), `macos`: build the frontend, the
   release Wasm decoder guest, the core and worker, then
   `cargo tauri build --no-bundle` and `cargo tauri bundle`. Signing secrets
   reach only the bundling and signing steps, after all dependency code has
   been compiled.
4. `checksums`: SBOM, `SHA256SUMS`, cosign and GPG signatures, attestations.
5. `release`: on a tag only, `gh release create --draft --prerelease`. This is
   the only job with `contents: write`.

### Dry runs

Run the workflow by hand (`workflow_dispatch`) to test it. A dry run builds
every bundle but skips every signing step, cosign, GPG and attestations, and
creates no release. Every artifact name ends in `-unsigned`. `SHA256SUMS` and
the SBOM are still produced. A dry run needs no secrets.

The release-only Tauri settings (`bundle.active`, sidecars, bundled licence
files, the Windows signing hook) come from
`python tools/release.py bundle-config`, which the workflow merges with
`cargo tauri --config`. `src-tauri/tauri.conf.json` holds the other bundle
settings but keeps `bundle.active` false, as the IPC configuration test
requires. The sidecars and resources are not in it because tauri-build copies
them on every build and fails when they are missing, which would break
development builds. The workflow creates `src-tauri/binaries/` itself; do not commit it.

## Owner set-up

Do these once. Claude and other agents never handle these keys or secrets.

### The `release` environment

Create a GitHub environment named `release` (Settings, Environments). Add the
secrets below as **environment secrets** of `release`, not repository secrets.
Tag runs use this environment for the `preflight`, `windows`, `macos` and
`checksums` jobs; dry runs do not use it.

Recommended protection:

- **Required reviewers**: add yourself, so every tag run waits for your
  approval before a job can read the signing secrets.
- **Deployment branches and tags**: allow only tags that match `v*`.
- Protect `v*` tags with a tag ruleset, so only maintainers can push them.

### Secrets

| Secret | Value |
| --- | --- |
| `WINDOWS_CERTIFICATE` | Base64 of the Windows code-signing PFX |
| `WINDOWS_CERTIFICATE_PASSWORD` | Password of that PFX |
| `APPLE_CERTIFICATE` | Base64 of the Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | Password of that `.p12` |
| `APPLE_SIGNING_IDENTITY` | For example `Developer ID Application: Name (TEAMID)` |
| `APPLE_API_KEY` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_PRIVATE_KEY` | Contents of the `AuthKey_<ID>.p8` file |
| `GPG_PRIVATE_KEY` | ASCII-armoured secret key that signs `SHA256SUMS` |
| `GPG_PASSPHRASE` | Passphrase of that key |

No repository variables are needed. cosign needs no secret: it signs with the
workflow's OpenID Connect identity. Set a secret from a file with, for
example, `gh secret set WINDOWS_CERTIFICATE --env release < certificate.b64`.

### Windows certificate

The workflow signs with `signtool` from the Windows SDK on the runner, using
SHA-256 and an RFC 3161 timestamp from `WINDOWS_TIMESTAMP_URL`
(`http://timestamp.digicert.com`). It signs `persephone.exe`,
`persephone-ui.exe`, `persephone-media-worker.exe` and the NSIS uninstaller
before Tauri packs them, then the NSIS and MSI installers. It then extracts
both installers and verifies every file with `signtool verify /pa /v`.

A self-signed certificate is enough for the current releases. Create one on a
trusted Windows machine:

```powershell
$cert = New-SelfSignedCertificate -Type CodeSigningCert `
    -Subject 'CN=OMG-Software Persephone (self-signed)' `
    -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
    -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(2) `
    -CertStoreLocation Cert:\CurrentUser\My
$password = Read-Host -AsSecureString 'PFX password'
Export-PfxCertificate -Cert $cert -FilePath persephone-signing.pfx -Password $password
[Convert]::ToBase64String([IO.File]::ReadAllBytes('persephone-signing.pfx')) |
    Set-Content -NoNewline persephone-signing.b64
Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)"
```

Store the PFX and its password offline. Delete `persephone-signing.b64` after
you set the secret.

With a self-signed certificate, verification fails only because the chain ends
in an untrusted root. The workflow accepts exactly that failure when the
signer is the configured certificate, and records `self-signed`. Any other
failure (a changed file, a missing signature or timestamp, another signer)
fails the run. The release notes then state:

> The Windows files are signed with a self-signed certificate. Windows will
> still show an unknown-publisher warning. Do not add this certificate to
> your trusted roots. Verify downloads with the signed SHA256SUMS or
> `gh attestation verify`.

The notes also give the certificate subject and its SHA-256 thumbprint,
computed from the public certificate. Windows SmartScreen will warn about
these files. Never install the certificate to make the warning go away.

A publicly trusted code-signing certificate needs its private key on hardware
(a token or HSM) under the CA/Browser Forum rules. Such a key cannot be
exported as a PFX, so it cannot be used through these secrets.

**All Windows signing is in one script.**
[`tools/sign_windows.ps1`](../tools/sign_windows.ps1) takes a list of files to
sign (or `-VerifyOnly` to verify them). Tauri calls it through
`bundle.windows.signCommand` for each file it signs, and the verify step calls
it for the installers and the programs inside them. To move to a cloud
signing service (for example Azure Artifact Signing or SignPath) when the
project qualifies, replace that script and the certificate step, and keep the
"files to sign" interface.

### Apple Developer ID and notarization

1. In an Apple Developer account, create a **Developer ID Application**
   certificate. Export it with its private key from Keychain Access as a
   `.p12` with a password. Set `APPLE_CERTIFICATE` to
   `base64 -i certificate.p12`, and `APPLE_CERTIFICATE_PASSWORD`.
2. Set `APPLE_SIGNING_IDENTITY` to the certificate's common name
   (`security find-identity -v -p codesigning` shows it).
3. In App Store Connect, Users and Access, Integrations, create a team API
   key with the Developer role. Download `AuthKey_<ID>.p8` (it downloads
   once). Set `APPLE_API_PRIVATE_KEY` to its contents, `APPLE_API_KEY` to the
   key ID and `APPLE_API_ISSUER` to the issuer ID.

Tauri imports the certificate into a temporary keychain, signs the two
sidecars, the main binary and the app with the hardened runtime, notarizes
the app and staples the ticket. The workflow then checks
`codesign --verify --deep --strict`, the hardened-runtime flag on all three
binaries, both architectures, `stapler validate` and `spctl --assess`. The
app uses no entitlements. The `.dmg` contains the notarized, stapled app.

### GPG key

```sh
gpg --quick-generate-key "Persephone release signing <release@example.org>" ed25519 sign 2y
gpg --list-secret-keys --keyid-format long          # note the fingerprint
gpg --armor --export-secret-keys FINGERPRINT | gh secret set GPG_PRIVATE_KEY --env release
gh secret set GPG_PASSPHRASE --env release
gpg --armor --export FINGERPRINT > persephone-release-key.asc
```

Use a real address you control. Publish `persephone-release-key.asc` and its
full fingerprint where users can find them independently of a release: the
repository, the project page and a key server such as keys.openpgp.org. Each
set of release notes names the fingerprint that signed it.

## Cutting a release

1. Choose the version. Use `X.Y.Z` or a pre-release that ends in a number,
   such as `0.1.0-alpha.1`. The MSI version carries that number as its fourth
   field (`0.1.0.1`); Windows Installer ignores the fourth field when it
   compares versions.
2. Set it everywhere and review the diff:

   ```sh
   python tools/release.py version 0.1.0-alpha.1
   git diff
   ```

   This sets `src-tauri/tauri.conf.json` (`version` and
   `bundle.windows.wix.version`), `frontend/package.json`, the root entries of
   `frontend/package-lock.json`, every workspace member `Cargo.toml` and the
   matching first-party `Cargo.lock` entries. It checks the lockfile with
   `cargo metadata --locked --offline`.
3. Run `python tools/dev.py check`, commit, and merge to `main` as usual.
4. Optionally run the workflow by hand on that commit as a dry run.
5. Tag and push the tag:

   ```sh
   git tag -a v0.1.0-alpha.1 -m "Persephone 0.1.0-alpha.1"
   git push origin v0.1.0-alpha.1
   ```

6. Approve the `release` environment when the run asks for it.
7. Review the draft pre-release: files, notes, the Windows signature line and
   the GPG fingerprint. Download and verify at least one file as below.
8. Publish the draft by hand. Nothing is published automatically.

## Verifying a download

Download the file, `SHA256SUMS`, `SHA256SUMS.sigstore.json` and
`SHA256SUMS.asc` from the same release. Check that `SHA256SUMS` is genuine,
then check the file against it.

```sh
# cosign keyless: the signature must come from this workflow and tag
cosign verify-blob --bundle SHA256SUMS.sigstore.json \
  --certificate-identity "https://github.com/OMG-Software/Persephone/.github/workflows/release.yml@refs/tags/v0.1.0-alpha.1" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS

# GPG: import the published key first and compare its fingerprint
gpg --verify SHA256SUMS.asc SHA256SUMS

# The file's checksum
sha256sum --ignore-missing -c SHA256SUMS

# GitHub build provenance for the file itself
gh attestation verify Persephone_0.1.0-alpha.1_amd64.AppImage --repo OMG-Software/Persephone
```

On Windows, compare `(Get-FileHash -Algorithm SHA256 <file>).Hash` with the
line in `SHA256SUMS`, and run `gh attestation verify` as above. The installer's
signature shows the subject and thumbprint named in the release notes; if it is
self-signed, Windows still warns, as described above.

On macOS, `spctl --assess --type execute -vv /Applications/Persephone.app`
should report a notarized Developer ID source.

## Limits

- **No updater.** Tauri's updater is not enabled; users install each version
  by hand.
- **macOS has not been tested live.** No Mac is available; macOS bundles are
  built, signed and checked in CI only.
- **Windows signature.** A self-signed certificate does not establish a
  publisher; SmartScreen warns. Users must rely on the signed `SHA256SUMS` and
  attestations.
- **Linux packages** are not signed themselves; only `SHA256SUMS` is.
- **Build tools fetched during bundling.** The Tauri CLI downloads NSIS and
  WiX on Windows, and linuxdeploy, its plugins and AppRun on Linux, from
  GitHub. One linuxdeploy plugin is a moving `continuous` release. The
  attestations record the workflow run, not these tools.
- **Notices** list the Rust crates only. The bundled frontend's npm
  dependencies are not listed yet.
- **SBOM** is made from the repository lockfiles. It also lists development
  and test dependencies that do not ship.
- **Icons**: `icon.icns` was generated from the 256 px `icon.png`; its larger
  sizes are upscaled.
- Workflow behaviour is verified only by a run on GitHub. Local checks cover
  YAML parsing and the `tools/release.py` and `tools/sign_windows.ps1` logic.

## Icons

`src-tauri/icons/32x32.png`, `128x128.png`, `128x128@2x.png` (256 px) and
`icon.icns` were generated from `src-tauri/icons/icon.png` with Pillow 12.2.0:

```python
from PIL import Image
src = Image.open("src-tauri/icons/icon.png").convert("RGBA")
for name, size in (("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)):
    (src if src.size == (size, size) else src.resize((size, size), Image.LANCZOS)).save(
        f"src-tauri/icons/{name}", optimize=True)
src.save("src-tauri/icons/icon.icns")
```

`icon.ico` is unchanged. `icon.png` stays first in `bundle.icon`, so the Linux
and macOS window icon is the same as before.
