# Keep accounts in separate profiles and switch by restart

* Status: accepted
* Deciders: project owner
* Date: 2026-10-27

Technical Story: on 26 October 2026 the owner asked for a way to add more
Matrix accounts. On 27 October 2026 the owner chose how accounts switch and
what the switcher may show, and then that every setting Persephone adds is
kept per account, not per computer.

## Context and Problem Statement

The core opens one encrypted profile per process: one account, one set of
stores, one key and one passphrase (ADR-0006, ADR-0013). matrix-sdk 0.19.1
cannot prove that its background tasks stopped, so after a client ran the
core never reopens or hands over stores in the same process (ADR-0008,
`docs/architecture/sdk-lifecycle.md`). How can one computer hold several
accounts without weakening that rule, and what may the UI learn about the
accounts that are not open?

## Decision Drivers

* No SDK store handoff in one process without stop-and-join evidence.
* The keyless UI never names a path or chooses where the core stores data
  (ADR-0004).
* Minimize metadata outside the encrypted stores.
* Each account keeps its own device keys and passphrase.

## Considered Options

* Several SDK sessions at once in one core, with an account selector.
* One profile per core process; switching locks this one and starts a new
  core for the other one.
* Defer multiple accounts.

For the switcher's labels:

* Keep each profile's Matrix ID in a plaintext label beside it.
* Store nothing extra and show generic labels.

## Decision Outcome

Chosen options: "one profile per core process, switching by restart" and
"generic labels", the owner's decisions of 27 October 2026.

* Slot 1 is the base profile: the default one (ADR-0013) or the directory
  given with `--profile`. Slot `n`, from 2 to 8, is the sibling directory with
  `-n` added to the base name, such as `profile-2`. `--account <n>` opens it.
* The UI sees slot numbers only (`profile_list`), never a path. It may ask to
  add a profile in the lowest free slot (`add_profile`) or to switch to an
  existing one (`switch_profile`). The core checks the slot.
* A switch closes the window, then locks the stores through the exit path,
  then starts a new core for the chosen slot as the last step before this
  core exits. The new core asks for that profile's passphrase, or for a new
  one, in its own window. Only one switch can wait. A switch is refused
  while an image or homeserver call is still running, and no such call
  starts once a switch waits, so the old core never waits on a request
  before it exits.
* Nothing names an account outside its encrypted store. The UI shows
  "Account 1", "Account 2", and so on.
* Settings belong to the account (owner decision, 27 October 2026). The core
  keeps them as one record in the profile's encrypted crypto store
  (`account_settings`, `set_account_settings`); the web view keeps no setting
  and uses no web storage. A new account starts with the defaults, and
  signing out deletes its settings with the rest of the profile. Every
  setting is a required field of `set_account_settings`; the stored record is
  read leniently, so a missing setting takes its default.

### Positive Consequences

* Several accounts on one computer without any in-process store handoff and
  without a claim about SDK-internal work.
* A profile that is not open reveals nothing about its account.

### Negative Consequences

* One account is open at a time. Messages for the others arrive only when
  they are opened.
* A switch costs a restart and that profile's passphrase.
* The user must remember which number is which account.
* Settings stay on this computer: another device signed in to the same
  account has its own. Matrix account data would carry them, but the
  homeserver could read them.
* Settings can be read and changed only while the account is signed in and
  unlocked; before that, the defaults apply.
* A switch to a profile whose old core is still exiting can start in
  `profile_in_use`; Unlock then tries again.
* Linux starts no UI yet (ADR-0010), so it cannot switch.
* Only checked with unit and IPC tests. A switch between two real profiles
  on each OS has not been run.

## Links

* Builds on [ADR-0008](0008-restart-the-core-process-to-finish-a-lock-after-the-client-ran.md)
* Builds on [ADR-0013](0013-open-a-default-profile-when-none-is-given.md)
* Builds on [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
