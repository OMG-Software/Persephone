# Encrypted storage and credential probes

- This package is `persephone-test-storage`. Run `cargo test --locked -p persephone-test-storage --test encrypted_stores` from the repository root for storage regressions.
- Use synthetic keys, messages, accounts, and temporary stores. Keep live database and SQLite WAL/SHM scans, closed-store scans, and plaintext positive controls where the test claim requires them.
- Preserve tests for wrong keys, correct-key recovery, and missing keys. The SDK can permit plaintext writes when no encryption key is supplied; an application must reject that path before opening a protected store.
- Pattern scans test the selected artifacts and markers. They do not prove whole-disk encryption, memory erasure, or absence of all sensitive data.
- `vault_roundtrip` is a Windows-only manual credential API probe. It writes a synthetic session credential. Preserve the unique PID/time target, refusal to overwrite an existing entry, read-back comparison, deletion, and post-delete check.
- Running Rust tests does not run the credential probe. Report its execution separately. Never substitute a real application credential or claim other operating-system vault support from the Windows result.
