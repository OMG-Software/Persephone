//! The macOS prompts: the core's own `NSAlert` windows, shown on the main
//! thread ([`main_thread`]). Secrets are typed into an `NSSecureTextField`;
//! sign-in adds a text field for the username. The reset warning and the
//! identity-change question answer "No" to Return, and "Yes" has no key. The
//! recovery key is shown as selectable, read-only text. The words come from
//! [`crate::prompt_text`]; shared limits and checks are in
//! [`crate::credentials`].
//!
//! A field is read as UTF-16 into zeroed-on-drop memory and then cleared.
//! The text AppKit itself keeps while a field is edited, and the
//! `NSString` copies it hands out, are outside this guarantee, as are the
//! SDK's copies on Windows.

use super::main_loop::main_thread;
use crate::credentials::{
    CredentialSource, Credentials, MAX_PASSWORD_CHARS, MAX_RECOVERY_KEY_CHARS, MAX_USERNAME_CHARS,
    PromptError, SecretPrompt, checked_secret, from_wide,
};
use crate::prompt_text::{self, SecretText};
use objc2::{
    MainThreadMarker, MainThreadOnly,
    rc::{Retained, autoreleasepool},
};
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSFloatingWindowLevel, NSSecureTextField, NSTextField,
    NSView, NSWindow,
};
use objc2_foundation::{NSPoint, NSRange, NSRect, NSSize, NSString};
use std::ptr::NonNull;
use zeroize::Zeroizing;

/// `NSAlertFirstButtonReturn`: the first button added, the default one.
pub const FIRST_BUTTON: isize = 1000;
/// `NSAlertSecondButtonReturn`.
pub const SECOND_BUTTON: isize = 1001;
/// Longest field text read at all, in UTF-16 units; longer text is refused
/// before it is copied.
pub const MAX_FIELD_UNITS: usize = 4096;
// A character is at most two UTF-16 units, so every text within a shared
// limit fits the read.
const _: () = assert!(
    MAX_FIELD_UNITS >= 2 * MAX_PASSWORD_CHARS
        && MAX_FIELD_UNITS >= 2 * MAX_RECOVERY_KEY_CHARS
        && MAX_FIELD_UNITS >= 2 * MAX_USERNAME_CHARS
);
/// The recovery key window's way to copy the key.
pub const COPY_HINT: &str = "Select the key and press Command+C to copy it.";
/// Width of the fields under the message, in points.
const FIELD_WIDTH: f64 = 320.0;
const FIELD_HEIGHT: f64 = 24.0;
const FIELD_GAP: f64 = 8.0;

/// The core's credential prompts on macOS.
pub struct NativePrompt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Informational,
    Warning,
}

/// What an alert shows under its caption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry<'a> {
    /// The message only.
    Message,
    /// The message and one secure field, with fixed placeholder text.
    Secret { placeholder: Option<&'a str> },
    /// The message, a username field and a secure password field.
    SignIn,
    /// The message as selectable, read-only text, so part of it can be
    /// copied.
    Selectable,
}

/// One alert. The first button is the default: Return presses it. A
/// "Cancel" button answers Escape; no other button has a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alert<'a> {
    pub caption: &'a str,
    pub message: &'a str,
    pub style: Style,
    pub buttons: &'static [&'static str],
    pub entry: Entry<'a>,
}

/// How an alert was closed, and its fields' text in order when it was closed
/// with the first button.
pub struct Answer {
    pub response: isize,
    pub fields: Result<Vec<Zeroizing<Vec<u16>>>, PromptError>,
}

pub fn sign_in_alert<'a>(caption: &'a str, message: &'a str) -> Alert<'a> {
    Alert {
        caption,
        message,
        style: Style::Informational,
        buttons: &["Sign In", "Cancel"],
        entry: Entry::SignIn,
    }
}

pub fn secret_alert<'a>(text: &'a SecretText<'a>) -> Alert<'a> {
    Alert {
        caption: text.caption,
        message: &text.message,
        style: Style::Informational,
        buttons: &["OK", "Cancel"],
        entry: Entry::Secret {
            placeholder: text.field,
        },
    }
}

/// A warning whose default answer is "No".
pub fn question_alert<'a>(caption: &'a str, message: &'a str) -> Alert<'a> {
    Alert {
        caption,
        message,
        style: Style::Warning,
        buttons: &["No", "Yes"],
        entry: Entry::Message,
    }
}

pub fn recovery_alert<'a>(caption: &'a str, message: &'a str) -> Alert<'a> {
    Alert {
        caption,
        message,
        style: Style::Informational,
        buttons: &["OK"],
        entry: Entry::Selectable,
    }
}

/// The failed-window question (`UiHost::ask_to_retry`). On macOS the watch
/// reports only a page that never reached the core (`ui.rs`).
pub fn retry_alert() -> Alert<'static> {
    Alert {
        caption: "Persephone's window did not start correctly",
        message: "Retry opens the window again; you stay signed in. \
                  Cancel closes Persephone.",
        style: Style::Warning,
        buttons: &["Retry", "Cancel"],
        entry: Entry::Message,
    }
}

/// The fields of an alert closed with its first button; the second button
/// cancels; anything else (the alert was aborted) fails.
pub fn entered(answer: Answer) -> Result<Vec<Zeroizing<Vec<u16>>>, PromptError> {
    match answer.response {
        FIRST_BUTTON => answer.fields,
        SECOND_BUTTON => Err(PromptError::Cancelled),
        _ => Err(PromptError::Failed),
    }
}

/// The answer to [`question_alert`]: `true` only for "Yes".
pub fn yes(response: isize) -> Result<bool, PromptError> {
    match response {
        FIRST_BUTTON => Ok(false),
        SECOND_BUTTON => Ok(true),
        _ => Err(PromptError::Failed),
    }
}

/// A secret field's text, checked like a password: not empty, at most `max`
/// characters, valid text without control characters.
pub fn secret_from(units: &[u16], max: usize) -> Result<Zeroizing<String>, PromptError> {
    let secret = Zeroizing::new(String::from_utf16(units).map_err(|_| PromptError::Invalid)?);
    checked_secret(secret, max)
}

/// Copies a field's text as UTF-16 into zeroed-on-drop memory, refusing
/// text longer than [`MAX_FIELD_UNITS`] before copying it.
pub fn units(text: &NSString) -> Result<Zeroizing<Vec<u16>>, PromptError> {
    let length = text.length();
    if length > MAX_FIELD_UNITS {
        return Err(PromptError::Invalid);
    }
    let mut units = Zeroizing::new(vec![0u16; length]);
    if length > 0 {
        let buffer = NonNull::new(units.as_mut_ptr()).ok_or(PromptError::Failed)?;
        // SAFETY: `buffer` holds `length` units, and the range is the
        // whole of `text`.
        unsafe { text.getCharacters_range(buffer, NSRange::new(0, length)) };
    }
    Ok(units)
}

/// Shows `alert` on the main thread and waits for it. `Unsupported` when no
/// main loop runs in this process.
fn prompt(alert: &Alert<'_>) -> Result<Answer, PromptError> {
    main_thread(|| {
        let mtm = MainThreadMarker::new().ok_or(PromptError::Failed)?;
        Ok(autoreleasepool(|_| show(mtm, alert)))
    })
    .unwrap_or(Err(PromptError::Unsupported))
}

fn rect(y: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(0.0, y), NSSize::new(FIELD_WIDTH, height))
}

fn secure_field(mtm: MainThreadMarker, placeholder: Option<&str>, y: f64) -> Retained<NSTextField> {
    let field =
        NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), rect(y, FIELD_HEIGHT));
    if let Some(placeholder) = placeholder {
        field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    }
    Retained::into_super(field)
}

/// Like the topmost, foreground windows on Windows: the core is an
/// Accessory application, so its window would otherwise open behind the
/// UI's window.
pub(super) fn bring_to_front(mtm: MainThreadMarker, window: &NSWindow) {
    window.setLevel(NSFloatingWindowLevel);
    // `activate` replaces this from macOS 14 only; this works on all.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// Draws `request` as an `NSAlert`, brings it to the front, runs it and
/// reads its fields.
fn show(mtm: MainThreadMarker, request: &Alert<'_>) -> Answer {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(request.caption));
    alert.setAlertStyle(match request.style {
        Style::Informational => NSAlertStyle::Informational,
        Style::Warning => NSAlertStyle::Warning,
    });
    for (index, title) in request.buttons.iter().enumerate() {
        let button = alert.addButtonWithTitle(&NSString::from_str(title));
        let key = match (index, *title) {
            (0, _) => "\r",
            (_, "Cancel") => "\u{1b}",
            _ => "",
        };
        button.setKeyEquivalent(&NSString::from_str(key));
    }
    let window = alert.window();
    let fields: Vec<Retained<NSTextField>> = match request.entry {
        Entry::Message => {
            alert.setInformativeText(&NSString::from_str(request.message));
            Vec::new()
        }
        Entry::Secret { placeholder } => {
            alert.setInformativeText(&NSString::from_str(request.message));
            let secret = secure_field(mtm, placeholder, 0.0);
            alert.setAccessoryView(Some(&secret));
            window.setInitialFirstResponder(Some(&secret));
            vec![secret]
        }
        Entry::SignIn => {
            alert.setInformativeText(&NSString::from_str(request.message));
            let container = NSView::initWithFrame(
                NSView::alloc(mtm),
                rect(0.0, 2.0 * FIELD_HEIGHT + FIELD_GAP),
            );
            let username = NSTextField::initWithFrame(
                NSTextField::alloc(mtm),
                rect(FIELD_HEIGHT + FIELD_GAP, FIELD_HEIGHT),
            );
            username.setPlaceholderString(Some(&NSString::from_str("Username")));
            let password = secure_field(mtm, Some("Password"), 0.0);
            container.addSubview(&username);
            container.addSubview(&password);
            // SAFETY: the key-view links are not retained; both fields are
            // retained by `container`, and so by the alert, for as long as
            // the alert is shown.
            unsafe {
                username.setNextKeyView(Some(&password));
                password.setNextKeyView(Some(&username));
            }
            alert.setAccessoryView(Some(&container));
            window.setInitialFirstResponder(Some(&username));
            vec![username, password]
        }
        Entry::Selectable => {
            let text =
                NSTextField::wrappingLabelWithString(&NSString::from_str(request.message), mtm);
            text.setSelectable(true);
            text.setEditable(false);
            text.setPreferredMaxLayoutWidth(FIELD_WIDTH);
            let fitted = text.sizeThatFits(NSSize::new(FIELD_WIDTH, 10_000.0));
            text.setFrame(rect(0.0, fitted.height.ceil()));
            alert.setAccessoryView(Some(&text));
            Vec::new()
        }
    };
    bring_to_front(mtm, &window);
    let response = alert.runModal();
    let read = if response == FIRST_BUTTON {
        fields
            .iter()
            .map(|field| units(&field.stringValue()))
            .collect()
    } else {
        Ok(Vec::new())
    };
    let empty = NSString::from_str("");
    for field in &fields {
        field.setStringValue(&empty);
    }
    Answer {
        response,
        fields: read,
    }
}

impl CredentialSource for NativePrompt {
    fn ask(&self, host: &str) -> Result<Credentials, PromptError> {
        let (caption, message) = prompt_text::sign_in(host);
        let fields = entered(prompt(&sign_in_alert(caption, &message))?)?;
        match fields.as_slice() {
            [username, password] => from_wide(username, password),
            _ => Err(PromptError::Failed),
        }
    }

    fn ask_secret(&self, request: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        let text = prompt_text::secret(&request);
        let fields = entered(prompt(&secret_alert(&text))?)?;
        match fields.as_slice() {
            [secret] => secret_from(secret, text.max_chars),
            _ => Err(PromptError::Failed),
        }
    }

    fn show_recovery_key(&self, user_id: &str, key: &str) -> Result<(), PromptError> {
        let (caption, message) = prompt_text::recovery_key(user_id, key, Some(COPY_HINT));
        match prompt(&recovery_alert(caption, &message))?.response {
            FIRST_BUTTON => Ok(()),
            _ => Err(PromptError::Failed),
        }
    }

    fn confirm_reset(&self) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::RESET;
        yes(prompt(&question_alert(caption, message))?.response)
    }

    fn confirm_identity_change(
        &self,
        user_id: &str,
        was_verified: bool,
    ) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::identity_change(user_id, was_verified)?;
        yes(prompt(&question_alert(caption, &message))?.response)
    }

    fn confirm_recovery_key_change(&self) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::RECOVERY_KEY_CHANGE;
        yes(prompt(&question_alert(caption, message))?.response)
    }

    /// Any panel error is `Failed`, so the key is reported as not saved; a
    /// cancel stays `Ok(None)`. The key window just ran, so a missing main
    /// loop is a failure here too, not a platform without the dialog.
    fn choose_recovery_key_file(&self) -> Result<Option<std::path::PathBuf>, PromptError> {
        let (title, suggested) = prompt_text::SAVE_RECOVERY_KEY;
        super::pick::save_panel(title, suggested).map_err(|_| PromptError::Failed)
    }
}

/// Asks whether to start the failed window again; `true` only for Retry.
/// No main loop, or an aborted alert, closes.
pub fn ask_to_retry() -> bool {
    prompt(&retry_alert()).is_ok_and(|answer| answer.response == FIRST_BUTTON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::NewPassphrase;

    fn answer(response: isize, fields: &[&str]) -> Answer {
        Answer {
            response,
            fields: Ok(fields
                .iter()
                .map(|text| Zeroizing::new(text.encode_utf16().collect()))
                .collect()),
        }
    }

    /// GIVEN the reset warning and the identity-change question THEN each is
    /// a warning whose first (default, Return) button is "No", and only
    /// "Yes" accepts.
    #[test]
    fn given_the_yes_or_no_questions_then_no_is_the_default() {
        let (caption, message) = prompt_text::RESET;
        let reset = question_alert(caption, message);
        let (caption, message) = prompt_text::identity_change("@alice:example.org", true).unwrap();
        let identity = question_alert(caption, &message);
        for alert in [reset, identity] {
            assert_eq!(alert.style, Style::Warning);
            assert_eq!(alert.buttons, ["No", "Yes"]);
            assert_eq!(alert.entry, Entry::Message);
        }
        assert_eq!(yes(FIRST_BUTTON), Ok(false));
        assert_eq!(yes(SECOND_BUTTON), Ok(true));
        assert_eq!(yes(-1000), Err(PromptError::Failed), "aborted");
    }

    /// GIVEN each secret prompt THEN it has one secure field with the fixed
    /// placeholder, OK as default and Cancel.
    #[test]
    fn given_secret_prompts_then_each_has_one_secure_field() {
        for request in [
            SecretPrompt::Password {
                user_id: "@a:b",
                host: "b",
            },
            SecretPrompt::RecoveryKey { user_id: "@a:b" },
            SecretPrompt::Passphrase { retry: false },
            SecretPrompt::NewPassphrase(NewPassphrase::Confirm),
            SecretPrompt::NewAccountPassword {
                username: "al",
                host: "b",
                step: crate::credentials::NewPassword::Mismatch,
            },
            SecretPrompt::RegistrationToken { host: "b" },
        ] {
            let text = prompt_text::secret(&request);
            let alert = secret_alert(&text);
            assert_eq!(
                alert.entry,
                Entry::Secret {
                    placeholder: text.field
                }
            );
            assert_eq!(alert.buttons, ["OK", "Cancel"]);
            assert_eq!(alert.message, text.message);
        }
        let (caption, message) = prompt_text::sign_in("example.org");
        let sign_in = sign_in_alert(caption, &message);
        assert_eq!(sign_in.entry, Entry::SignIn);
        assert!(sign_in.message.contains("example.org"));
    }

    /// GIVEN how an alert closed THEN the first button gives the fields,
    /// the second cancels, and an aborted alert fails.
    #[test]
    fn given_alert_responses_then_only_the_first_button_gives_fields() {
        let fields = entered(answer(FIRST_BUTTON, &["user", "secret"])).unwrap();
        assert_eq!(fields.len(), 2);
        assert!(matches!(
            entered(answer(SECOND_BUTTON, &[])),
            Err(PromptError::Cancelled)
        ));
        assert!(matches!(
            entered(answer(-1001, &[])),
            Err(PromptError::Failed)
        ));
        let refused = Answer {
            response: FIRST_BUTTON,
            fields: Err(PromptError::Invalid),
        };
        assert!(matches!(entered(refused), Err(PromptError::Invalid)));
    }

    /// GIVEN secret field text THEN it is accepted only when not empty,
    /// within the limit in characters (not UTF-16 units), valid UTF-16 and
    /// without control characters.
    #[test]
    fn given_secret_text_then_the_limits_and_checks_apply() {
        let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(
            secret_from(&wide("pässword"), 8).unwrap().as_str(),
            "pässword"
        );
        assert_eq!(secret_from(&wide("😀😀"), 2).unwrap().as_str(), "😀😀");
        assert!(matches!(
            secret_from(&wide(""), 8),
            Err(PromptError::Invalid)
        ));
        assert!(matches!(
            secret_from(&wide("123456789"), 8),
            Err(PromptError::Invalid)
        ));
        assert!(matches!(
            secret_from(&wide("a\nb"), 8),
            Err(PromptError::Invalid)
        ));
        assert!(matches!(
            secret_from(&[0xd800], 8),
            Err(PromptError::Invalid)
        ));
        let longest = "k".repeat(MAX_RECOVERY_KEY_CHARS);
        assert!(secret_from(&wide(&longest), MAX_RECOVERY_KEY_CHARS).is_ok());
        assert!(secret_from(&wide(&format!("{longest}k")), MAX_RECOVERY_KEY_CHARS).is_err());
        // Sign-in goes through the shared check of both fields.
        assert!(from_wide(&wide("alice"), &wide("secret")).is_ok());
        assert!(from_wide(&wide(&"u".repeat(MAX_USERNAME_CHARS + 1)), &wide("s")).is_err());
    }

    /// GIVEN field text in Foundation WHEN it is read THEN it is copied
    /// exactly, and text over the read limit is refused before any copy.
    #[test]
    fn given_field_text_then_it_is_read_within_the_limit() {
        let read = units(&NSString::from_str("pä😀")).unwrap();
        assert_eq!(read.as_slice(), "pä😀".encode_utf16().collect::<Vec<_>>());
        assert!(units(&NSString::from_str("")).unwrap().is_empty());
        let longest = "x".repeat(MAX_FIELD_UNITS);
        assert_eq!(
            units(&NSString::from_str(&longest)).unwrap().len(),
            MAX_FIELD_UNITS
        );
        assert!(matches!(
            units(&NSString::from_str(&format!("{longest}x"))),
            Err(PromptError::Invalid)
        ));
    }

    /// GIVEN a new recovery key THEN it is shown as selectable text that
    /// ends with the macOS copy hint, with one OK button.
    #[test]
    fn given_a_recovery_key_then_it_is_selectable_with_the_copy_hint() {
        let (caption, message) = prompt_text::recovery_key("@a:b", "EsTj 3yST", Some(COPY_HINT));
        let alert = recovery_alert(caption, &message);
        assert_eq!(alert.entry, Entry::Selectable);
        assert_eq!(alert.buttons, ["OK"]);
        assert!(alert.message.contains("EsTj 3yST"));
        assert!(
            alert
                .message
                .ends_with("\n\nSelect the key and press Command+C to copy it.")
        );
        assert_eq!(retry_alert().buttons, ["Retry", "Cancel"]);
    }

    /// GIVEN no main loop (this test process) WHEN any prompt is asked THEN
    /// it fails closed at once instead of waiting, and an unprintable user
    /// ID is refused before anything is shown.
    #[test]
    fn given_no_main_loop_then_prompts_fail_closed() {
        let prompt = NativePrompt;
        assert!(matches!(
            prompt.ask("example.org"),
            Err(PromptError::Unsupported)
        ));
        assert_eq!(
            prompt
                .ask_secret(SecretPrompt::Passphrase { retry: false })
                .err(),
            Some(PromptError::Unsupported)
        );
        assert_eq!(prompt.confirm_reset(), Err(PromptError::Unsupported));
        assert_eq!(
            prompt.show_recovery_key("@a:b", "key"),
            Err(PromptError::Unsupported)
        );
        assert_eq!(
            prompt.confirm_identity_change("@a:b\u{202e}c", false),
            Err(PromptError::Invalid)
        );
        assert!(!ask_to_retry());
    }
}
