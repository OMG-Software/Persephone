# Offer to create a default keyring on Linux

* Status: proposed
* Deciders: project owner
* Date: 2026-11-24

Technical Story: on 24 November 2026 the owner's Linux Mint Xfce test machine
logged in automatically and had no keyring at all. `ReadAlias("default")`
returned `/` and `~/.local/share/keyrings` was empty. Persephone stopped with
"Local store unavailable on this system", and no prompt appeared. The owner
asked whether Persephone could create the keyring itself, or ask the user to
let it.

## Context and Problem Statement

ADR-0010 keeps the wrapping secret of the store key in the Secret Service's
default collection. It makes "no default collection" `unsupported`, so the
profile stays locked, and it says that "the app explains why". The page shows
one text for every Linux `unsupported` case. That text does not tell the user
which part is missing or what to do.

A desktop that logs in automatically does not unlock or create the login
keyring, because there is no login password to use. GNOME Keyring then has a
Secret Service but no default collection. Programs that use libsecret create
it with `org.freedesktop.Secret.Service.CreateCollection`. The keyring then
shows its own prompt for a new password. Persephone can do the same thing
without seeing that password.

How can Persephone help the user get a default keyring, and say which part is
missing in every other case, without weakening the fail-closed rule of
ADR-0010?

## Decision Drivers

* Keep the fail-closed rule of ADR-0010: no kernel keyring, no file and no
  passphrase-only fallback.
* The keyring password is the keyring's own secret. Persephone must not see,
  choose or store it.
* Do not change the user's existing keyrings without a separate decision.
* Ask only when the answer can help, and never more than once in each open
  attempt.
* Tell the user what is missing and what to do next, with fixed texts only.

## Considered Options

* Keep ADR-0010 as it is, and only explain each case better.
* Ask in the core's own window, then call `CreateCollection` with the alias
  `default`. The keyring prompts for its password.
* Make an existing, non-default collection the default with `SetAlias`.
* Store the item in any collection that exists when there is no default.

## Decision Outcome

Chosen option: "Ask in the core's own window, then call `CreateCollection`",
with fixed reasons for every other case. The owner must approve this record
before it is accepted.

### The question

When `ReadAlias("default")` returns `/` while Persephone stores a new wrapping
secret, the core asks in its own GTK window (ADR-0010). The window says that
the system has no default keyring, that Persephone can ask the keyring to
create one, and that the keyring will then ask for a new password in its own
window. "No" is the default button, and closing the window answers "No", as
for the other yes-or-no questions.

* **Yes:** the core calls `CreateCollection` with the label "Login" and the
  alias `default`. It completes the provider's `Prompt` object with the same
  handling as the unlock prompt of ADR-0010, dismissed after 5 minutes without
  an answer. It then reads the alias again. Only a collection that the
  provider now names `default` is used, and the item is then created as
  before.
* **No, a closed window, a dismissed or unanswered keyring prompt, a
  provider that refuses `CreateCollection` (some KWallet and KeePassXC
  setups), or a provider that does not make the new collection the
  default:** the profile stays locked with the reason "no default keyring".
* Persephone asks at most once in each open attempt. "Unlock" on the page is
  a new attempt and can ask again.
* Persephone never calls `SetAlias`, so it never changes which of the user's
  existing keyrings is the default.
* **One creator at a time.** Before its first search for the item, each core
  takes the session-bus name
  `io.github.omg_software.persephone.StoreKeyWrapCreation` (`RequestName`
  with `DoNotQueue`, asked again every 100 ms). It holds the name through the
  question, the keyring prompts, `CreateItem` and the read-back. Without
  this, a core that waits for its user's answer could create a second item
  after another core had already wrapped a key with the first, and later
  reads could choose the second. The lock is on the bus because the bus is
  what every core that reaches the provider shares: a file under each
  process's data directory would not join cores with different
  environments. A core waits at most 15 minutes for the name, then fails,
  and Unlock can try again. The bus releases the name when a core's
  connection closes. Cores that reach one provider through different buses,
  and cores from before this change, are not serialised; they keep the
  "smallest object path" rule of ADR-0010.

The question is asked only when a new secret must be stored (a new profile,
or a profile whose wrap is written again). A profile whose secret is already
lost cannot be opened by a new keyring, so the question is not asked then.

### Fixed reasons

The vault's `unsupported` result on Linux is split into four reasons. Each is
a fixed store state that the page shows with a fixed text that says what to
do next:

| Store state | Cause |
| --- | --- |
| `no_session_bus` | No session bus. |
| `no_secret_service` | No service owns `org.freedesktop.secrets`, and none can be started. |
| `no_default_keyring` | No default collection, and none was created (see above). |
| `no_display` | The core cannot show its windows, so it cannot ask. |

These states are retryable: "Unlock" opens the profile again after the user
fixes the cause. The other platforms keep `unsupported` for "no vault on this
system". No text from the bus or the provider reaches the page.

### Positive Consequences

* A desktop that logs in automatically can open a profile after one question
  and the keyring's own password prompt.
* The user is told which part is missing and what to do next.
* The fail-closed rule of ADR-0010 is unchanged: every refusal leaves the
  profile locked.

### Negative Consequences

* The new keyring is created with a password that the user chooses in the
  keyring's prompt. On a desktop that logs in automatically, that keyring is
  not unlocked at login. The keyring asks for its password again after each
  login, when Persephone opens the profile.
* A user can choose an empty password in the keyring's prompt. The keyring
  then keeps its items unencrypted on disk. Persephone cannot see or refuse
  that choice. The passphrase of ADR-0050 is then the only layer that a copy
  of the files does not bypass.
* The question adds one more core window on Linux. It is not shown on the
  other platforms.
* While one core waits for its user's answers, a second core that must store
  a new secret shows "Unlocking" until the first finishes, for at most 15
  minutes.
* A provider that creates the collection but does not set the alias leaves
  the profile locked, although a new collection exists. The user can make it
  the default in the keyring manager.

## Pros and Cons of the Options

### Keep ADR-0010 as it is, and only explain each case better

* Good, because no new code talks to the provider.
* Bad, because the user must find and use a keyring manager before the first
  start, with no help from the app.

### Ask in the core's own window, then call `CreateCollection`

* Good, because it is the same request that libsecret's own clients make, and
  the keyring keeps control of its password.
* Good, because the user decides in a window the page cannot draw or answer.
* Bad, because some providers refuse it, and then the user must still act.

### Make an existing, non-default collection the default with `SetAlias`

* Good, because no new keyring is needed when one exists.
* Bad, because it changes where other programs store their secrets, which is
  outside Persephone's authority without a separate decision.

### Store the item in any collection that exists

* Good, because no question is needed.
* Bad, because the choice of collection is arbitrary, may be a temporary
  collection (`session`), and differs between processes, which breaks the
  "one canonical item" rule of ADR-0010.

## Links

* Refines [ADR-0010](0010-linux-platform-layer.md) (Linux vault and prompts)
* Related: [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md),
  [ADR-0050](0050-make-the-profile-passphrase-optional-and-off-by-default.md)
* [Secret Service API: CreateCollection](https://specifications.freedesktop.org/secret-service-spec/latest/)
