# Windows containment probe

- This package is `persephone-test-windows`. Keep Windows APIs behind platform gates. The executable returns 2 on unsupported operating systems.
- Run `cargo test --locked -p persephone-test-windows` for parser and classifier tests. Run `python tools/dev.py sandbox --offline` separately for the native probe, with cached dependencies. It creates a fresh `.local/runs` output directory.
- A successful build or unit-test run does not prove containment. Inspect the native probe's exit status and individual filesystem, process, Job, mitigation, and network observations.
- Preserve the endpoint-observed network differential. A failed connection alone is not evidence of a denied network path. Keep unrestricted controls, TCP/UDP and IPv4/IPv6 distinctions, and listener shutdown checks.
- Preserve the AppContainer and LPAC distinction. A comparative LPAC observation must not override a failed AppContainer gate.
- Keep canaries and process state synthetic. Retain handle ownership, process/thread waits, and cleanup on failed launch or timeout. Do not change firewall policy or other host security settings to make the probe pass.
- Consult `docs/evidence/windows-sandbox.md` and the feasibility workbench before changing acceptance claims. Preserve nonzero gate failures; record unresolved results explicitly.
