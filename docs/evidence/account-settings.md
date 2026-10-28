# Account settings: password, addresses, sessions, recovery key, notifications

Checked 27 October 2026 on Windows 11 Pro x64 (build 26300), debug build,
Rust 1.99.1, Matrix Rust SDK 0.19.1, against a disposable Merovingian 0.12.59
in WSL Ubuntu 24.04. Source: branch `claude/account-settings-9f90af` on base
`1c42e39`, with the uncommitted changes of this slice; the run used the
committed root `Cargo.lock` plus the additions of
[ADR-0022](../adr/0022-notify-through-push-rules-and-content-free-desktop-notifications.md).
Synapse was not run: no disposable Synapse was installed on this host.
**Result: on Merovingian, every account-settings path below behaved as
designed through the core's calls. The core's native Windows and macOS
windows, the Save dialog and the OS notification itself were not exercised;
the prompts were answered by a test source.**

## What ran

The live program [`live_account_settings.rs`](../../crates/core/tests/live_account_settings.rs)
drives a real core (`Core::handle`, the calls the UI process relays) signed in
as the runner's first account. A plain SDK client signs in as the runner's
second account to write a message, and another SDK client signs in as the
first account to be its second device. A test credential source answers the
core's prompts: the profile's password, a new password, "yes" to the
recovery-key change, and a file under ignored `.local/` for the Save dialog.

```powershell
wsl -d Ubuntu-24.04 -u root -- /usr/bin/setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all --reuid=1000 --regid=1000 --init-groups --reset-env --no-new-privs /usr/bin/python3 /mnt/c/dev/Persephone/.claude/worktrees/account-settings-9f90af/tools/servers/run.py --merovingian-binary /home/retro/m-deb/build-deb/src/merovingian-server --synapse-python /nonexistent --server merovingian --output /mnt/c/dev/Persephone/.claude/worktrees/account-settings-9f90af/.local/runs/account-settings
$env:CARGO_HOME = (Resolve-Path .local/cache/cargo).Path
$env:CARGO_TARGET_DIR = (Resolve-Path .local/build/rust).Path
$env:PERSEPHONE_LIVE_PROFILE = (Resolve-Path .local/runs/account-settings/profile.json).Path
cargo test --locked --offline -p persephone-core --features dev-disposable-servers --test live_account_settings -- --ignored --nocapture
```

The test passed (`1 passed`, exit 0, 68 s). The runner was stopped with
`SIGTERM` and exited 0, removing its temporary databases.

## Results

| Feature | Observed on Merovingian 0.12.59 |
| --- | --- |
| Notification level | Read `all`; set `mentions`, `none` and `all` in turn; each read back after the next sync. |
| Email addresses and phone numbers | Listed none, `m.3pid_changes` on. Asking for an email link and for a phone code each gave `medium_not_supported`; adding with nothing waiting gave `nothing_pending`; removing an absent email address gave `done`. |
| Desktop notifications | With the setting on, a message from the second account waited as a notification with the room name (`Live notifications`) and the sender's user ID only; neither contained the message text. |
| Recovery key change | After setting up encryption, the change gave no error, the new key differed from the first, and the file the core wrote held the new key and a line break only. |
| Verifying another session | The core asked the second device by its ID; both sides showed the same seven emoji; after both confirmed, the flow ended `done` and the session list showed the device verified. |
| Password change | Keeping other sessions: `done`, and the second device still answered `whoami`. A wrong current password: `invalid_password`. Signing out other sessions: `done`, and the second device's token was refused. |

## Not covered by this run

* Adding an email address or phone number successfully: Merovingian 0.12.59
  answers both token requests as unsupported, so `add_three_pid` with a
  confirmed address, and a phone's `submit_url`, ran against the mock
  homeserver only (`crates/core/tests/account_settings.rs`).
* The native windows (CredUI, message boxes, `GetSaveFileNameW`, macOS
  `NSAlert` and `NSSavePanel`) and the Windows toast itself. These are
  covered by the [native window hand checks](native-windows-hand-checks.md),
  not run for this slice.
* Synapse.
* Linux, where the prompts are not implemented and these changes fail as
  `unsupported`.

## Observed outside this slice

The second account's room invitation was not listed by `invite_list` within
20 seconds on Merovingian 0.12.59 (`invitation_listed false`); the test's
second device joined the room instead. Invitations are not part of this
slice; the cause was not investigated.
