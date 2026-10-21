# Test programs and fixtures

- Run commands from the repository root and use the root `Cargo.lock`. Rust packages here are test programs and feasibility fixtures; they do not establish production security controls.
- Use `cargo test --locked -p <package>` for a focused Rust package. `python tools/dev.py check` runs the workspace checks and both Python unit-test directories. Add `--offline` to the development runner when dependencies are cached.
- Run Python tests with `python -m unittest discover -s tests/tools -v` and `python -m unittest discover -s tests/servers -v`.
- Prefer behavior assertions with clear GIVEN, WHEN, and THEN conditions. Exercise negative cases, timeouts, cancellation, malformed inputs, and cleanup when relevant.
- Keep controlled loopback tests separate from live disposable-server runs. Never use real accounts or production stores. Keep live profiles and reports under ignored `.local`.
- Preserve authoritative nonzero results. Report skipped platform coverage, unsupported targets, missing prerequisites, and incomplete runs explicitly.
- Update the relevant `docs/evidence` report when a requested change alters a recorded claim. Keep historical results tied to their original inputs; do not promote them to current verification.
