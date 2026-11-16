# Report code loaded into Persephone and ask the user to review it

* Status: accepted
* Deciders: project owner
* Date: 2026-11-16

Technical Story: RivaTuner Statistics Server's overlay hook crashed the
WebView2 browser process and was also loaded in the core, the process that
holds the keys. The core and the UI now refuse hook-based injection and images
not signed through Microsoft ([injected DLL hardening](../evidence/injected-dll-hardening.md)).
Anti-malware DLLs signed through Microsoft, and every DLL in the WebView2
processes, still load.

## Context and Problem Statement

Code loaded into a Persephone process can read all of its memory, including
keys, tokens and decrypted messages. Persephone must run beside anti-malware
software, so it cannot refuse every third-party DLL. How does the user learn
what other code is loaded into Persephone?

## Decision Drivers

* The owner decided on 16 November 2026: block hook-based injection for every
  signer, keep refusing DLLs not signed through Microsoft, allow the rest, and
  tell the user about the rest.
* The owner chose an in-app list in Settings > Security and a one-line notice
  for new code.
* The core holds the authority (ADR-0004). The page is keyless and untrusted.
* Module paths can contain the user name. Diagnostics must stay minimal.
* No network access. A signature check must not fetch revocation data.

## Considered Options

* Report the other modules in the app, with a notice until the user reviews them
* Refuse every module that is not from Windows
* Write the modules to a log only

## Decision Outcome

Chosen option: "Report the other modules in the app", because it is the
owner's decision and the only option that keeps anti-malware working and the
user informed.

* On Windows, a core thread lists the modules of the core, the UI process and
  every process in the UI's Job (the WebView2 processes). It starts when the
  page is ready, or after 10 s, and repeats every 30 s.
* Each module's signature is checked with WinVerifyTrust: the embedded
  signature, then the system catalogs. The check uses no network, no UI and no
  revocation fetch.
* These modules are expected and not reported:
  * modules signed by "Microsoft Windows" or "Microsoft Windows Publisher";
  * modules signed by "Microsoft Corporation" inside the Windows folder or the
    WebView2 runtime folder;
  * Persephone's own executables in the core's folder.
* Every other module is reported. This includes a third-party DLL in
  System32.
* The page asks for the report with `third_party_modules` and marks rows as
  reviewed with `acknowledge_modules`. Both limits are checked in the UI host
  before relay and again in the core.
* `ModulesChanged` is content-free and coalesced, like `SecurityChanged`.
* A reviewed row is keyed by its lowercase file name and its signer (or
  "unsigned" or "invalid"). The same vendor's DLL in a new folder stays
  reviewed. A changed signer, or a file that is no longer signed, shows as new
  again.
* Reviewed keys are kept in the open profile's encrypted store (at most 200
  keys, 64 KiB). An unreadable record means nothing is reviewed. Without an
  open profile, reviews last only for the core process. Signing out deletes
  the profile store.
* Settings > Security lists the modules as inert text. The main window shows a
  notice while any module is unreviewed and the user is signed in.

### Positive Consequences

* The user sees what is loaded into the processes that hold or show their
  messages, including the WebView2 processes the policies do not cover.
* Anti-malware keeps working.

### Negative Consequences

* This is transparency, not a boundary. Code that is already loaded can hide
  itself or change what the core reads. Same-user malware is out of scope.
* The trust decision for "expected" uses the leaf signer's name after
  WinVerifyTrust accepts the chain. A machine that trusts an attacker's root
  certificate can forge it.
* The core opens its own child processes with `PROCESS_VM_READ` to list their
  modules.
* A review in a demo or signed-out run is not saved.
* Only Windows is implemented. Other platforms report "unsupported".

## Pros and Cons of the Options

### Report the other modules in the app

* Good, because it meets the owner's decision.
* Bad, because the user must act on a list of technical names.

### Refuse every module that is not from Windows

* Good, because no third-party code runs in the core or the UI.
* Bad, because Microsoft's signature policy cannot express it, and the
  WebView2 processes are not covered.
* Bad, because it breaks anti-malware integration, which the owner rejected.

### Write the modules to a log only

* Good, because it is simple.
* Bad, because the user does not see it, and paths in logs leak the user name.

## Links

* Refines [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Evidence: [third-party module report](../evidence/third-party-module-report.md)
