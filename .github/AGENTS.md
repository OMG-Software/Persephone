# Workflow guidance

This area inherits `../AGENTS.md`.

- `workflows/pages.yml` deploys the static `site/` directory. Do not assume it runs Rust, frontend, security, or integration checks.
- `workflows/check.yml` runs `python tools/dev.py check` on Windows for pull requests and pushes to `main`. Keep its toolchain pins aligned with the versions recorded in evidence, and do not add the local `AR_x86_64_pc_windows_msvc` workaround or other machine-specific paths. It does not run live homeservers, the native sandbox probe or the dependency audit.
- Keep action references pinned to full commit SHAs with version comments. Verify the upstream source and tag before changing a pin.
- Preserve minimum permissions: read-only contents by default, with Pages and identity-token write permissions limited to the deployment job.
- Keep checkout credentials disabled, the Pages environment, and serialized deployment behavior. Do not add secrets to logs or artifacts.
- Review workflow triggers and uploaded paths with site changes. Validate YAML and action inputs against authoritative documentation when changing workflow behavior. Report local validation separately from a completed GitHub Actions run.
