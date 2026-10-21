# Development tools

- Run commands from the repository root. Read `docs/development.md` and the affected tool's argument parser before changing a command.
- Keep `dev.py` as the shared check entry point. Preserve locked dependency resolution, `.local` cache/build paths, subprocess exit codes, and failure propagation.
- `python tools/dev.py check` includes Rust, frontend, contracts, and Python unit tests. Live homeservers, the native sandbox probe, and OSV audit are separate operations. `audit --offline` is invalid.
- Keep live profiles, stores, credentials, and fresh run directories under ignored `.local`. Validate resolved paths and loopback origins before starting workers. Do not print raw profiles or secrets on failure.
- In `matrix_restart.py`, pass store keys through child stdin. Do not put keys in arguments, logs, reports, or files. Preserve separate worker processes and strict phase-report validation.
- In `matrix_capabilities.py`, preserve bounded gateway bodies, no redirects/proxies, controlled capability cases, and errors that do not silently select another sync mode.
- For controller changes, run `python -m unittest discover -s tests/tools -v`. For server changes, also run `python -m unittest discover -s tests/servers -v`.
- Inspect `audit_dependencies.py` output destinations before running it. An advisory query needs network access and is separate from a local build result.
