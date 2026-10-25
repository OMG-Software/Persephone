# Workflow guidance

This area inherits `../AGENTS.md`.

- `workflows/pages.yml` deploys the static `site/` directory. Do not assume it runs Rust, frontend, security, or integration checks.
- `workflows/check.yml` runs `python tools/dev.py check` on Windows, macOS and Ubuntu runners and in Debian, Arch, Fedora and openSUSE containers, for pull requests and pushes to `main`. Keep its toolchain pins aligned with the versions recorded in evidence, and do not add the local `AR_x86_64_pc_windows_msvc` workaround or other machine-specific paths. It does not run live homeservers, the native sandbox probe or the dependency audit.
- `workflows/release.yml` signs and drafts a release only for pushed `v*` tags. A push to `main` builds unsigned files as a dry run does and replaces the `latest` pre-release and tag. Keep main runs out of the `release` environment and its secrets. Test a tag run with `github.event_name == 'push' && github.ref_type == 'tag'`, not with the event name alone. See `docs/release.md`.
- A job `if` without a status function adds `success()`, which is false when any earlier job in the `needs` chain was skipped. A job that must run after a skipped signing job needs `!cancelled()` and explicit `needs.<job>.result` checks, as `checksums` and `latest` have.
- Pin container images by digest, with the tag in a comment. Install each distribution's prerequisites from its own package manager. A passing non-Windows job shows that the code builds and its tests pass. It does not show a vault, prompt, UI containment or worker containment on that OS; these fail closed there.
- Keep action references pinned to full commit SHAs with version comments. Verify the upstream source and tag before changing a pin.
- Preserve minimum permissions: read-only contents by default, with Pages and identity-token write permissions limited to the deployment job.
- Keep checkout credentials disabled, the Pages environment, and serialized deployment behavior. Do not add secrets to logs or artifacts.
- Review workflow triggers and uploaded paths with site changes. Validate YAML and action inputs against authoritative documentation when changing workflow behavior. Report local validation separately from a completed GitHub Actions run.
