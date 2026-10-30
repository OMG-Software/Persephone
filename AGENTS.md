# Project guidance

## Scope and requirements

- This file applies to the full repository. Read the nearest child `AGENTS.md` for area-specific rules. Child files add to this guidance. Direct user instructions take priority.
- Build a general Matrix desktop client for Windows, Linux, and macOS. Test Merovingian first and retain compatibility with other standard homeservers.
- The intended first release has one account, login, rooms, message history, text and image messages, and end-to-end encrypted chats. Do not expand this scope without a requirement.
- Use accepted records in `docs/adr/index.md` for architecture decisions. Use current source and manifests for implementation facts, `docs/development.md` for commands, and dated evidence for recorded results. Report conflicts between these sources.
- The project has open feasibility and production security gates. A prototype, successful build, or passing test suite does not establish readiness for high-risk users.

## Working method

- Be critical. Identify errors, unsafe assumptions, weak evidence, missing coverage, and better approaches. Separate verified facts from assumptions and inference.
- Use simple, readable solutions. Inspect the current worktree before changes. Preserve unrelated and uncommitted work. Do not reset, overwrite, or clean it away.
- Ask for clarification when ambiguity materially changes the result. Otherwise state a reasonable assumption and proceed.
- Use `rg` for file and text searches. Exclude ignored caches, generated output, and third-party sources unless the task needs them.
- Use UTF-8, LF, and a final newline. Follow `.editorconfig`. Keep generated files separate from their source.
- Use clear, short sentences, active voice, and consistent technical terms. Follow ASD-STE100 principles without changing technical meaning. End user-facing answers with a confidence score from 0 to 100%.

## Architecture and security constraints

- Keep sessions, SDK clients, keys, tokens, stores, and media-broker authority in `crates/core`. Keep `src-tauri` and the web view keyless, as required by ADR-0004.
- Treat the UI, homeserver responses, decrypted messages, names, URLs, and media as untrusted. Validate requests in the receiving process. Bound sizes, queues, concurrent work, and frame bodies before allocation.
- Keep IPC command schemas and permissions explicit. Do not add generic filesystem, shell, network, arbitrary decryption, or key-export capabilities to the UI.
- Render received content as inert text. Do not add raw HTML, automatic URL previews, external asset fetches, or unrestricted image decoding.
- Use SDK-owned Matrix cryptography and explicit dependency features. Preserve Sliding Sync preference with classic sync fallback. Do not implement server-specific shortcuts or custom cryptography.
- Fail closed on unavailable secrets, invalid peer data, unsupported containment, and incomplete lifecycle transitions. Never silently select plaintext storage or plaintext sending after an encrypted operation fails.
- Require explicit stop-and-join ownership before SDK store handoff. A pause, dropped handle, wrapper completion, or outer cancellation is insufficient evidence of a full SDK drain. See `docs/architecture/sdk-lifecycle.md`.
- Keep media decoding in the bounded WebAssembly guest and contained worker design. A separate process or AppContainer token alone does not prove network denial. Review each OS separately.
- Keep secrets, real chat data, synthetic profiles, and unredacted logs out of committed artifacts. Use ignored `.local/` for builds, caches, disposable profiles, and raw run output. Minimize diagnostics and metadata.
- Preserve TLS certificate checks and the TLS 1.3 policy. Loopback HTTP fixtures do not authorize product trust bypasses.

## Layout

| Area | Responsibility |
| --- | --- |
| `crates/` | Core process, bounded protocols, DTOs, and media pipeline |
| `src-tauri/` | Keyless desktop host, command relay, capabilities, window policy |
| `frontend/` | Bundled Svelte UI and typed host bridge |
| `tests/` | Matrix, storage, platform, and Python harness verification |
| `tools/` | Development checks, dependency audit, and disposable-server controllers |
| `docs/` | Requirements, ADRs, plans, and reproducible evidence |
| `site/` | Static public project page |
| `.github/` | CI, release, SonarQube and GitHub Pages workflows |

## Build and verification

Run these commands from the repository root. Use the committed root `Cargo.lock` and `frontend/package-lock.json`. Do not update dependency versions or features incidentally.

| Command | Purpose |
| --- | --- |
| `python tools/dev.py bootstrap` | Install locked frontend dependencies |
| `python tools/dev.py check` | Rust, frontend, contract drift, and Python harness checks |
| `python tools/dev.py build [--release]` | Build the frontend, Wasm guest, and app executables (debug or release); no tests |
| `python tools/dev.py rust` | Build frontend and Wasm guest; run Rust format, tests, and Clippy |
| `python tools/dev.py frontend` | Svelte checks, static build, and Node tests with LCOV coverage |
| `python tools/dev.py contracts` | Generated DTO/fixture drift and TypeScript checks |
| `python tools/dev.py coverage` | Instrumented Rust tests writing LCOV for SonarQube; needs cargo-llvm-cov 0.9.1 |
| `python tools/dev.py audit` | Network-dependent OSV dependency audit |
| `python tools/dev.py sandbox --offline` | Separate Windows native containment gate |

- Add `--offline` only when dependencies are cached. `audit` requires network access. `check` does not run live homeservers, the native containment gate, or the dependency audit.
- Build outputs belong under `.local/build`. The Tauri host embeds the built frontend. Sandbox tests and the media worker need the release Wasm guest. Use the wrapper to preserve build order.
- For focused Rust changes, use `cargo test --locked -p <package>` after its build prerequisites. Run `cargo fmt --all -- --check` for Rust changes. See child guidance for package names and prerequisites.
- Add meaningful behavior tests for changed contracts. Prefer GIVEN/WHEN/THEN cases. Cover rejection and failure paths where relevant. Do not add tests that merely repeat the implementation.
- Run checks relevant to the change. For guidance-only changes, verify paths, package names, commands, inheritance, and diff hygiene. Runtime tests are needed when runtime behavior changes.
- Inspect completed output and exit status. A crash, timeout, interruption, incomplete run, or unrun check is not a pass. State skipped checks and platform limits. Never copy a historical passing result into a new verification claim.

## Delegation and review

- Delegate well-bounded or repetitive reading, tracing, implementation, and checks when useful. Use the least expensive capable worker available. Parallelize independent work only.
- Give each worker a clear scope, current baseline, authoritative requirements, edit ownership, expected output, and required verification. Preserve meaningful progress where supported.
- Keep requirements, architecture, security, privacy, concurrency, compatibility, migrations, and data-integrity decisions with the lead agent.
- Treat worker reports as claims. Inspect material results, verify important assumptions, confirm checks completed, and review high-risk changes directly. Check parallel edits for conflicts.
- Prefer authoritative requirements, direct result inspection, completed command results, reports, targeted tests, then worker summaries. Resolve disagreements between requirements, tests, and implementation.

## Pull requests

Keep descriptions brief. Explain the problem, resulting behavior, reason, and actual validation. Use these headings:

```markdown
## Summary
## What changed
## Why it changed
## Tests
### Updated
### New
```

Use `None` where no tests changed. Distinguish changed tests from checks actually run. Do not claim completion until the requested work and relevant verification are complete.
