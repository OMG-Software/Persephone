# Native window hand checks

Prepared 24 October 2026. **Status: not run.** This is the P4 checklist for the
core's native windows on Windows. A person runs it by hand. Record the date,
Windows build, display scale, Persephone commit and build type, and a result
for each row. A row that was not run stays "not run".

The windows come from `crates/core/src/credentials.rs` (`NativePrompt`). They
belong to the core process, not to the page.

| Window | Caption | When it appears |
| --- | --- | --- |
| Sign-in | Sign in to Persephone | Sign in from the page |
| Encryption password | Set up encryption in Persephone | First encryption setup |
| Recovery key entry | Verify this device with your recovery key | Verify with the recovery key |
| Recovery key display | Your Persephone recovery key | Recovery is set up |
| Unlock | Unlock Persephone | Start, or unlock after a lock |
| New passphrase | Protect Persephone on this computer | A new or upgraded profile |
| Reset | Reset Persephone on this computer? | Reset from the unlock screen |
| Identity change | A contact's identity changed | A contact's identity changed (ADR-0007) |

Use synthetic accounts on the disposable homeservers only
([development guide](../development.md)).

## Checks for each window

Run every row for every window in the table, unless the row says otherwise.

| # | Check | Result |
| --- | --- | --- |
| 1 | The window opens in front of Persephone and takes the keyboard focus. | not run |
| 2 | The caption and text match the table and name the right account and server. | not run |
| 3 | Using only the keyboard (Tab, Shift+Tab, Space, Enter), you can complete it and cancel it. | not run |
| 4 | Cancel, Esc or the close button (where it is enabled) changes nothing, and the page shows a cancelled or failed state, not a success. | not run |
| 5 | Secret fields show dots, not the typed text. | not run |
| 6 | Narrator reads the caption, the text and the focused control. | not run |
| 7 | At 150% and 200% display scale, the text is not cut off and the window fits on the screen. | not run |
| 8 | While the window is open, the page cannot type into it, close it or read what you typed. | not run |

## Window-specific checks

| # | Window | Check | Result |
| --- | --- | --- | --- |
| 9 | Identity change, Reset | "No" is selected when the window opens, so Enter does not accept. | not run |
| 10 | Identity change | After "No", nothing is sent in that room, and the page still shows the pause. | not run |
| 11 | Recovery key display | Record whether Ctrl+C copies the window text, which includes the key, to the clipboard. | not run |
| 12 | Unlock | A wrong passphrase shows the retry text. The profile does not open. | not run |
| 13 | New passphrase | A passphrase that is too short or does not match shows the matching text, and asks again. | not run |
| 14 | Sign-in | A wrong password shows an error on the page. No password text appears on the page. | not run |

## Results

Not run. Add the run details above this line when the checks are done.
