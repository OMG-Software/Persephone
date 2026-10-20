# Slint and iced: security research for Persephone

Research date: 19 October 2026. Target: a Rust Matrix desktop client for high-risk users on Windows, Linux, and macOS.

Decision update: accepted [ADR-0000](../adr/0000-use-tauri-for-the-desktop-client.md)
selects Tauri. The provisional recommendation below is retained as research
history and does not override the accepted framework decision.

## Recommendation

Neither framework has earned approval for a high-risk release. Retain **Slint with its software renderer as the first feasibility candidate**, with **iced's software renderer as the alternative**. This is a provisional engineering judgment: Slint has an explicit security-update/reporting policy and accessibility integration; iced offers a cleaner way to omit image codecs and SVG. The evidence does not establish an overall security winner.

Do not choose based on Rust syntax, advisory counts, dependency counts, or release recency alone. The default feature sets are not appropriate security specifications for Persephone. Our selected content paths, sandbox, dependencies, and release process need verification together.

My earlier Slint preference was based on a shortlist, not this deeper dependency investigation. This research adds substantive reservations, particularly its enabled desktop parsers and Linux dependency maintenance warnings.

## Scope and evidence

| Item | Slint | iced |
| --- | --- | --- |
| Latest GitHub release observed | [1.18.1, 21 September 2026](https://github.com/slint-ui/slint/releases/tag/v1.18.1) | [0.14.0, 7 December 2025](https://github.com/iced-rs/iced/releases/tag/0.14.0) |
| Source commit inspected | `372cf0ee5577c3dfec309a45e7b778ba4e81b734` | `3997291f318a8bc06fa522f5579836fb3feb94df` |
| Research candidate | Explicit Winit + software renderer + accessibility | Explicit tiny-skia + codec-free image widget |

I inspected primary documentation, tagged manifests/source, security policies and advisories, and generated fresh Cargo lockfiles. I traced normal dependency graphs excluding proc macros for Windows x64, Linux x64, and macOS ARM64. An additional Windows graph traced iced's optional GPU path.

OSV version queries covered **574 distinct public crate/version pairs** across the two fresh lockfiles. Lockfiles include optional, build, and other-target packages; a match is not automatically a shipped dependency. Dependency graphs establish selection, not final binary contents or exploitability. These probes pin the top-level framework; compatible transitive releases may be newer than the tagged upstream lockfiles.

The [evidence bundle](slint-iced-evidence.zip) includes probe manifests, locks, graphs, advisory-query requests/results, source snapshots with hashes, and command-result records. Its third-party source files are research evidence, not dependencies vendored for the application.

## Comparison

| Criterion | Slint 1.18.1 | iced 0.14.0 | Implication |
| --- | --- | --- | --- |
| UI execution | Trusted `.slint` normally compiles into Rust; runtime interpretation is a separate choice | Rust application code constructs UI/state | Neither requires executing received messages as UI code |
| Hostile raster media | Decoded-pixel API available, but desktop Winit enables image/SVG parsing features | Decoded-pixel API plus `image-without-codecs`; SVG separately optional | iced provides stronger feature-level parser removal for this path |
| Rendering | Default FemtoVG/OpenGL plus software renderer; other renderers optional | Default wgpu plus tiny-skia; either can be selected explicitly | Evaluate a software-only candidate for both; this does not eliminate the OS compositor or native code |
| Security response | Explicit policy and private reporting; latest-release security support | No repository policy detected in this pass; public audit workflow exists | Slint has clearer public support commitments; a workflow is not proof its latest run passed |
| Accessibility | Feature integrates OS accessibility APIs | Native accessibility support remains an open upstream issue | Slint is a stronger starting point for accessible trust/verification UI; test actual controls |
| Containment | No app-level sandbox established by the reviewed APIs | Same | Persephone must supply and test OS-enforced restrictions |

Source anchors: [Slint public feature manifest](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/api/rs/slint/Cargo.toml), [iced feature manifest](https://github.com/iced-rs/iced/blob/3997291f318a8bc06fa522f5579836fb3feb94df/Cargo.toml), [Slint integration methods](https://docs.rs/slint/1.18.1/slint/), [iced accessibility issue](https://github.com/iced-rs/iced/issues/552).

## Findings that affect the decision

### 1. Slint's software configuration still enables desktop media parsers

The selected native Winit backend enables `i-slint-core/image-decoders` and `svg`. Its graph includes `image` with PNG/JPEG and `resvg` with text, compressed SVG, and embedded raster-image support. Omitting `image-default-formats` does not remove the separate SVG route. This is enabled surface, not a newly discovered vulnerability. [Tagged backend manifest](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/internal/backends/winit/Cargo.toml), [workspace decoder configuration](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/Cargo.toml).

Slint's `Image::from_rgba8` accepts decoded pixels. We can reserve path/data/SVG loaders for reviewed application assets and route received media through a sandboxed helper. That is an application boundary to enforce; the parser code still being selected must not be described as compiled out. [Tagged image implementation](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/internal/core/graphics/image.rs).

**Action:** prove that all received images, avatars, and thumbnails reach only the bounded pixel API. Consider upstream discussion about independently disabling native decoders if compile-time removal becomes a requirement. Confidence: 98% in the manifest/graph finding; runtime enforcement is untested.

### 2. iced offers a codec-free path, with remaining text and metadata dependencies

The candidate enables `image-without-codecs`; its resolved `image` crate has no codec features. The `Rgba` handle branch uses supplied pixels rather than encoded-image decoding. `svg`, `markdown`, and syntax highlighting are optional and omitted. An EXIF parser dependency remains selected; the reviewed RGBA branch bypasses the encoded-image/EXIF loading branches. A codec-free feature is not a promise that every parser has disappeared. [Tagged image loading](https://github.com/iced-rs/iced/blob/3997291f318a8bc06fa522f5579836fb3feb94df/graphics/src/image.rs), [graphics dependencies](https://github.com/iced-rs/iced/blob/3997291f318a8bc06fa522f5579836fb3feb94df/graphics/Cargo.toml).

Plain text still exercises shaping, layout, and fonts. The application must cap text sizes and histories, avoid downloaded fonts, and validate links and identity displays. Compiling the UI in Rust does not isolate a compromised renderer from the UI's memory.

**Action:** preserve the pixel-only path if we prototype iced. Verify dimensions, checked output-size arithmetic, allocation limits, and helper output in the receiving process. Confidence: 97% in feature/path selection; containment is untested.

### 3. iced's GPU path has a current dependency warning beyond the old release lockfile

The upstream 0.14.0 lockfile contains `lru 0.16.2`, affected by [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002.html). Fresh resolution selects `0.16.4`, which clears that particular advisory. However, **0.16.4 is covered by a second advisory**, [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html), fixed in 0.18.2.

The additional graph confirms `iced -> iced_renderer -> iced_wgpu -> cryoglyph 0.1.0 -> lru 0.16.4`. Cryoglyph requests `^0.16`, so an ordinary compatible update will not reach 0.18.2. The software candidate has no `lru` in its selected normal graph. [Published cryoglyph dependency metadata](https://crates.io/api/v1/crates/cryoglyph/0.1.0/dependencies).

The newer issue requires particular panic/unwind/key-destruction conditions. I did not establish those conditions in cryoglyph or reproduce an exploit. This is a confirmed dependency-advisory match, not evidence that a Matrix message can exploit iced.

**Action:** keep GPU rendering out of the initial candidate; before enabling it, resolve the dependency issue through an upstream-compatible update or reviewed fix and inspect reachability. Do not suppress it merely because Rust is used. Confidence: 99% in selection/advisory match; application exploitability unresolved.

### 4. Both reduced candidates retain maintenance warnings

The fresh OSV results distinguish maintenance status from memory-corruption advisories:

| Package/advisory | Selected normal graph in these probes | Meaning |
| --- | --- | --- |
| `ttf-parser 0.25.1`, [RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192.html) | iced: all three targets; Slint: Linux | Upstream maintenance warning; not a demonstrated exploit |
| `zvariant 5.15.1`, [RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370.html) | Slint: Linux | Standalone crate merged into zbus 6; dependency migration needed |
| `zbus_names 4.3.5`, [RUSTSEC-2026-0371](https://rustsec.org/advisories/RUSTSEC-2026-0371.html) | Slint: Linux | Same family of migration warning |
| `bincode 2.0.1`, [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141.html) | Slint lock only; absent from the selected graphs | Do not attribute to runtime solely from the lock |
| `zvariant_derive 5.15.1`, [RUSTSEC-2026-0372](https://rustsec.org/advisories/RUSTSEC-2026-0372.html) | Slint lock; proc macros excluded from these graphs | Needs build-dependency review, not a runtime-vulnerability claim |

In iced the font path includes `iced_graphics -> cosmic-text -> fontdb -> ttf-parser`. In Slint's Linux graph it is `Winit -> sctk-adwaita -> ab_glyph -> owned_ttf_parser -> ttf-parser`. Slint's newer text stack therefore does not make the entire Linux dependency graph free of the older parser. Its D-Bus warnings were published only two days before this research; their presence alone does not establish poor vendor response.

**Action:** request/track upstream migration plans, assess affected platform paths, and establish explicit acceptance criteria before release. Preserve accessibility rather than quietly removing it to get fewer warnings. Confidence: 98% in graph matches; future maintenance quality unknown.

### 5. Slint's published vulnerabilities concern development tools

Both advisories cover versions `>=1.17.0,<1.18.0` and list 1.18.0 as fixed:

- [GHSA-p64q-493c-6mmq](https://github.com/slint-ui/slint/security/advisories/GHSA-p64q-493c-6mmq): an attacker-controlled remote viewer could induce developer-host file reads and editor writes through slint-lsp.
- [GHSA-2jqx-548f-7fj8](https://github.com/slint-ui/slint/security/advisories/GHSA-2jqx-548f-7fj8): remote-preview listener authentication failure allowed viewer control. The vendor explicitly distinguishes development tooling from generated applications.

These are important developer/build-system trust failures. They do not establish vulnerabilities in Persephone's proposed release runtime. The current reviewed release is newer than the stated fixes.

Slint also offers optional MCP, live-preview, and system-testing features. Their ability to inspect/control or reload UI is unsuitable for our release profile; compile them out, rather than relying on environment variables being unset. Use build-time `.slint` compilation and exclude runtime interpretation of downloaded definitions. [Tagged Slint feature definitions](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/api/rs/slint/Cargo.toml).

**Action:** maintain patched developer tooling, avoid unnecessary remote preview, isolate release credentials, and verify final release feature closure. Apply the equivalent rule to iced's debug, hot reload, and time-travel facilities. Confidence: 99% in published advisory scope; fixes were not independently exploitation-tested.

### 6. Public security process evidence favours Slint; neither has audit clearance

Slint's [security policy](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/SECURITY.md) commits to security updates for the latest release and directs private reports. It also contains a [compiler fuzz target](https://github.com/slint-ui/slint/blob/372cf0ee5577c3dfec309a45e7b778ba4e81b734/internal/compiler/fuzz/fuzz_targets/compiler_fuzzing.rs). That target covers source parsing/compiler lowering; it is not proof of runtime image/text/IPC fuzz coverage.

iced's [security page](https://github.com/iced-rs/iced/security) showed no detected repository security policy and no published repository advisories when checked. Its tagged [audit workflow](https://github.com/iced-rs/iced/blob/3997291f318a8bc06fa522f5579836fb3feb94df/.github/workflows/audit.yml) declares daily/push/PR `cargo audit` after `cargo update`. This is a positive control, but audits an updated graph rather than necessarily the release's original lockfile. I did not inspect a completed current run.

No independent whole-framework security audit establishing suitability for this threat model was located in the bounded primary-source search. That is a search limit, not proof that no audit exists. Fewer published advisories do not establish fewer vulnerabilities. Confidence: 94% in the documented processes; actual response guarantees and full testing coverage need further evidence.

## Candidate feature sets actually resolved

These are research probes, not approved or build-tested application configurations:

```toml
slint = { version = "=1.18.1", default-features = false, features = [
    "std", "compat-1-18", "backend-winit", "renderer-software", "accessibility"
] }
```

```toml
iced = { version = "=0.14.0", default-features = false, features = [
    "tiny-skia", "thread-pool", "x11", "wayland", "image-without-codecs"
] }
```

Both Linux display paths were retained for research. Target-specific packaging can narrow them later. Renderer performance, text input, selection, screen readers, binary feature closure, and OS sandbox integration remain untested.

## Decision gates before adoption

1. Prototype text-heavy timelines and identity/verification prompts, including Unicode/bidirectional spoofing, input methods, keyboard use, and screen readers on all three OSes.
2. Implement and test pixel-only media transfer with actual OS-enforced worker restrictions. A process boundary without those restrictions does not meet our threat model.
3. Resolve or formally assess the identified dependency warnings, with separate runtime and build-tool graphs; establish a patching process consistent with the supported framework release.
4. Review unsafe/native interfaces and enabled backends in the actual packaged builds; test parser/resource limits and failure handling. Prefer explicit features over upstream defaults.
5. Integrate the Matrix SDK only after UI/core authority boundaries are defined; no UI process should need raw encryption keys. This limits damage but cannot hide displayed plaintext from a compromised UI.

## Verification completed and limits

- Both `cargo generate-lockfile` commands completed with exit 0.
- All six platform-specific `cargo tree` commands completed with exit 0; additional GPU `lru` tracing completed with exit 0.
- OSV requests completed and responses were inspected for advisory type, selected target, and withdrawn status. Top-level-only queries returned no results; the transitive investigation still found warnings, demonstrating why top-level checks are insufficient.
- The advisory queries cover public Rust package/version records; they do not assess installed OS libraries, driver vulnerabilities, or unreported defects.
- No upstream code was built or run. No cross-platform application build, parser fuzzing, exploit reproduction, GUI test, binary inspection, or sandbox test occurred.
- `cargo-audit` was unavailable locally; the advisory check used OSV's version API and direct RustSec verification, not a completed `cargo audit` run.
- The selected targets are representative, not a complete supported-platform matrix. No minimum OS versions or remaining CPU architectures have been approved.
- No new vulnerability is claimed. Current package-level warnings and documented past vulnerabilities must not be presented as proven attacks against Persephone.

**Overall confidence: 94% in the documented source/dependency findings; 82% in the provisional framework recommendation until practical gates are tested.**
