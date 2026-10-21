# Workflow guidance

This area inherits `../AGENTS.md`.

- `workflows/pages.yml` deploys the static `site/` directory. Do not assume it runs Rust, frontend, security, or integration checks.
- Keep action references pinned to full commit SHAs with version comments. Verify the upstream source and tag before changing a pin.
- Preserve minimum permissions: read-only contents by default, with Pages and identity-token write permissions limited to the deployment job.
- Keep checkout credentials disabled, the Pages environment, and serialized deployment behavior. Do not add secrets to logs or artifacts.
- Review workflow triggers and uploaded paths with site changes. Validate YAML and action inputs against authoritative documentation when changing workflow behavior. Report local validation separately from a completed GitHub Actions run.
