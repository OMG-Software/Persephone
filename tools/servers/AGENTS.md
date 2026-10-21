# Disposable server harness

- This harness is for synthetic Merovingian and Synapse instances in Linux/WSL. Never accept production configuration, databases, accounts, or remote origins.
- Keep server listeners on loopback. Keep output under the repository's ignored `.local` directory, and require an empty output directory before writing profiles.
- Preserve the Merovingian prerequisites: non-root UID, empty effective and bounding capability sets, and `no_new_privs`. Do not bypass these checks to make a run pass.
- `run.py --help` lists required binary, Python, and output paths. `--server` selects the instances. `--synapse-classic-only` changes the disposable Synapse configuration; distinguish this from gateway-injected capability responses.
- Give each registered account its own authentication flow. Redact passwords and registration tokens from retained errors. Preserve bounded readiness waits and process termination with a final wait.
- `build_merovingian.py` requires Linux/WSL, a build path under `.local`, and 1 to 8 jobs. Preserve stable source identity, hardening checks, stage exit codes, and binary identity reports.
- Run `python -m unittest discover -s tests/servers -v` from the repository root after harness changes. These unit tests do not establish live interoperability. Run live checks only with explicitly configured disposable inputs, and report their scope.
- Temporary-directory cleanup is not secure erasure. A hard kill can leave synthetic state.
