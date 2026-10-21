# Matrix integration and lifecycle fixtures

- This package is `persephone-test-matrix`. Keep Matrix compatibility general. Exercise both Merovingian and Synapse when a live compatibility claim depends on them.
- Read `docs/architecture/sdk-lifecycle.md` before changing lifecycle ownership. Retain owned tasks and completion handles across cancellation and timeout. A paused client or an aborted wrapper does not prove that SDK work or store writers have stopped.
- Preserve capability selection and explicit error behavior. Missing or disabled Sliding Sync support can select classic sync; authentication, malformed discovery, and native-sync errors must not silently trigger fallback.
- Keep controlled tests synthetic. Run `cargo test --locked -p persephone-test-matrix`; focus lifecycle changes with `--test lifecycle_owner --test lifecycle_sdk`.
- For TLS-policy changes, run the `tls_policy` test target. Keep loopback HTTP test exceptions separate from the product TLS policy.
- Live binaries require explicit disposable profiles and report paths. Use `tools/matrix_capabilities.py` and `tools/matrix_restart.py` for their controlled scenarios. Build `verified_recipients` before using those controllers.
- Verify exact decrypted events, device/key continuity, and peer verification where claimed. Successful API calls alone do not prove encrypted delivery or trust recovery.
- Keep restart keys off disk and command lines. Preserve wrong-key, missing-key, offline, and resume cases. Wait for live executables to finish before Cargo replaces them, especially on Windows.
- Distinguish fixture ownership, completed-operation transitions, and full SDK drain. Do not infer production-safe lock, logout, or store handoff from a bounded fixture result.
