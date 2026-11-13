//! The Linux prompts: the core's own GTK3 windows, shown on the `core-gtk`
//! thread ([`gtk_thread`](super::gtk_thread)), with the words from
//! [`crate::prompt_text`] and the limits and checks of
//! [`crate::credentials`] (ADR-0010).
//!
//! Secrets are typed into hidden entries; sign-in adds an entry for the
//! username. Return presses the first (default) button. The reset warning,
//! the identity-change question and the link question have "No" as that
//! button and "Yes" second; closing them (Escape or the window's close
//! button) answers "No". The recovery key is shown as selectable,
//! read-only text. Received text is shown as plain text, never as markup.
//!
//! An entry's text is copied once from GTK's own buffer straight into
//! zeroed-on-drop memory, and the entry is then cleared; GTK 3 overwrites
//! deleted entry text with zeros. Copies that an input method or GTK keeps
//! while the field is edited are outside this guarantee, as are the SDK's.
//!
//! Links (ADR-0024, ADR-0030) are confirmed in a core window and opened
//! through the desktop's OpenURI portal ([`super::portal`]); no shell or
//! program is started with the address.

use super::gtk_thread::{GtkError, gtk_thread, settle};
use crate::credentials::{
    CredentialSource, Credentials, MAX_PASSWORD_CHARS, MAX_USERNAME_CHARS, PromptError,
    SecretPrompt, checked_secret,
};
use crate::prompt_text;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use std::{ffi::CStr, path::PathBuf};
use zeroize::Zeroizing;

/// Longest entry text read at all, in characters; longer text is refused
/// before it is copied. Each entry also stops input one character past its
/// own limit, so an over-long paste is refused, not cut to a shorter secret.
pub const MAX_FIELD_CHARS: usize = 4096;
/// The recovery key window's way to copy the key.
pub const COPY_HINT: &str = "Select the key and press Ctrl+C to copy it.";
/// The window title of every core window; the caption says what it asks.
const TITLE: &str = "Persephone";
/// Width of the message, in characters.
const MESSAGE_WIDTH_CHARS: i32 = 60;

/// The core's prompts on Linux.
pub struct NativePrompt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Informational,
    Warning,
}

/// What a window shows under its caption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The message only.
    Message,
    /// The message and one hidden entry with fixed placeholder text,
    /// accepting at most `max_chars`.
    Secret {
        placeholder: Option<String>,
        max_chars: usize,
    },
    /// The message, a username entry and a hidden password entry.
    SignIn,
    /// The message as selectable, read-only text.
    Selectable,
}

/// One window. Owned, so it can move to the `core-gtk` thread; the
/// message may hold a recovery key and is zeroed when dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub caption: String,
    pub message: Zeroizing<String>,
    pub style: Style,
    pub buttons: &'static [&'static str],
    pub entry: Entry,
}

/// How a window closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// The button at this index (the first is the default).
    Button(usize),
    /// Closed without a button: Escape or the window's close button.
    Closed,
    /// Anything else: the window did not run.
    Other,
}

/// How a window closed, and its entries' text in order when it closed
/// with the first button.
pub struct Answer {
    pub response: Response,
    pub fields: Result<Vec<Zeroizing<String>>, PromptError>,
}

/// `text` without NUL characters, which GTK's C strings cannot carry: a
/// NUL would end the shown text early.
pub fn without_nul(text: &str) -> String {
    text.replace('\0', "\u{FFFD}")
}

fn alert(
    caption: &str,
    message: &str,
    style: Style,
    buttons: &'static [&'static str],
    entry: Entry,
) -> Alert {
    Alert {
        caption: without_nul(caption),
        message: Zeroizing::new(without_nul(message)),
        style,
        buttons,
        entry,
    }
}

pub fn sign_in_alert(caption: &str, message: &str) -> Alert {
    alert(
        caption,
        message,
        Style::Informational,
        &["Sign In", "Cancel"],
        Entry::SignIn,
    )
}

pub fn secret_alert(text: &prompt_text::SecretText<'_>) -> Alert {
    alert(
        text.caption,
        &text.message,
        Style::Informational,
        &["OK", "Cancel"],
        Entry::Secret {
            placeholder: text.field.map(str::to_owned),
            max_chars: text.max_chars,
        },
    )
}

/// A warning whose default answer is "No".
pub fn question_alert(caption: &str, message: &str) -> Alert {
    alert(
        caption,
        message,
        Style::Warning,
        &["No", "Yes"],
        Entry::Message,
    )
}

pub fn recovery_alert(caption: &str, message: &str) -> Alert {
    alert(
        caption,
        message,
        Style::Informational,
        &["OK"],
        Entry::Selectable,
    )
}

/// The failed-window question (`UiHost::ask_to_retry`).
pub fn retry_alert() -> Alert {
    alert(
        "Persephone's window did not start correctly",
        "Retry opens the window again; you stay signed in. \
         Cancel closes Persephone.",
        Style::Warning,
        &["Retry", "Cancel"],
        Entry::Message,
    )
}

/// A notice with one OK button (the UI host's memory warning).
pub fn notice_alert(caption: &str, message: &str) -> Alert {
    alert(caption, message, Style::Warning, &["OK"], Entry::Message)
}

/// The entries of a window closed with its first button; the second
/// button or closing the window cancels; anything else fails.
pub fn entered(answer: Answer) -> Result<Vec<Zeroizing<String>>, PromptError> {
    match answer.response {
        Response::Button(0) => answer.fields,
        Response::Button(1) | Response::Closed => Err(PromptError::Cancelled),
        _ => Err(PromptError::Failed),
    }
}

/// The answer to [`question_alert`]: `true` only for "Yes". Closing the
/// window is "No"; a window that did not run fails.
pub fn yes(response: Response) -> Result<bool, PromptError> {
    match response {
        Response::Button(1) => Ok(true),
        Response::Button(0) | Response::Closed => Ok(false),
        _ => Err(PromptError::Failed),
    }
}

/// Sign-in entries, each checked like a password: not empty, within its
/// limit, no control characters.
pub fn sign_in_from(fields: Vec<Zeroizing<String>>) -> Result<Credentials, PromptError> {
    let mut fields = fields.into_iter();
    match (fields.next(), fields.next(), fields.next()) {
        (Some(username), Some(password), None) => Ok(Credentials {
            username: checked_secret(username, MAX_USERNAME_CHARS)?,
            password: checked_secret(password, MAX_PASSWORD_CHARS)?,
        }),
        _ => Err(PromptError::Failed),
    }
}

/// The one entry of a secret window, checked against `max` characters.
pub fn secret_from(
    fields: Vec<Zeroizing<String>>,
    max: usize,
) -> Result<Zeroizing<String>, PromptError> {
    let mut fields = fields.into_iter();
    match (fields.next(), fields.next()) {
        (Some(secret), None) => checked_secret(secret, max),
        _ => Err(PromptError::Failed),
    }
}

/// Copies `text`, at most [`MAX_FIELD_CHARS`] characters, into
/// zeroed-on-drop memory.
pub fn field_text(text: &[u8], chars: usize) -> Result<Zeroizing<String>, PromptError> {
    if chars > MAX_FIELD_CHARS || text.len() > 4 * MAX_FIELD_CHARS {
        return Err(PromptError::Invalid);
    }
    let text = std::str::from_utf8(text).map_err(|_| PromptError::Invalid)?;
    let mut copy = Zeroizing::new(String::with_capacity(text.len()));
    copy.push_str(text);
    Ok(copy)
}

/// Reads an entry's text from GTK's own buffer, without a copy outside
/// zeroed memory.
fn read_entry(entry: &gtk::Entry) -> Result<Zeroizing<String>, PromptError> {
    let chars = usize::from(entry.text_length());
    if chars > MAX_FIELD_CHARS {
        return Err(PromptError::Invalid);
    }
    let raw: *mut gtk::ffi::GtkEntry = entry.to_glib_none().0;
    // SAFETY: `gtk_entry_get_text` returns the entry's own NUL-terminated
    // UTF-8 buffer, valid until the entry changes; it is copied before the
    // entry is touched again.
    let text = unsafe { CStr::from_ptr(gtk::ffi::gtk_entry_get_text(raw)) };
    field_text(text.to_bytes(), chars)
}

/// Shows `alert` on the `core-gtk` thread and waits for it. No display is
/// `Unsupported`; a stopped GTK thread is `Failed`.
pub fn prompt(alert: Alert) -> Result<Answer, PromptError> {
    gtk_thread()
        .run(move || show(&alert))
        .map_err(|error| match error {
            GtkError::NoDisplay => PromptError::Unsupported,
            GtkError::Stopped => PromptError::Failed,
        })
}

/// Shows `alert` without waiting for it (a notice); `false` when no window
/// can be shown.
pub fn post(alert: Alert) -> bool {
    gtk_thread()
        .post(move || {
            show(&alert);
        })
        .is_ok()
}

fn hidden_entry(placeholder: Option<&str>, max_chars: usize) -> gtk::Entry {
    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_input_purpose(gtk::InputPurpose::Password);
    entry.set_placeholder_text(placeholder);
    entry.set_max_length(i32::try_from(max_chars.saturating_add(1)).unwrap_or(i32::MAX));
    entry.set_activates_default(true);
    entry
}

/// Builds `request` as a GTK dialog, runs it modally, reads its entries
/// and destroys it. Runs on the `core-gtk` thread only.
fn show(request: &Alert) -> Answer {
    let failed = Answer {
        response: Response::Other,
        fields: Err(PromptError::Failed),
    };
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        match request.style {
            Style::Informational => gtk::MessageType::Info,
            Style::Warning => gtk::MessageType::Warning,
        },
        gtk::ButtonsType::None,
        &request.caption,
    );
    dialog.set_title(TITLE);
    for (index, label) in request.buttons.iter().enumerate() {
        let Ok(id) = u16::try_from(index) else {
            return failed;
        };
        dialog.add_button(label, gtk::ResponseType::Other(id));
    }
    dialog.set_default_response(gtk::ResponseType::Other(0));
    let Ok(area) = dialog.message_area().downcast::<gtk::Box>() else {
        // SAFETY: the dialog was created here and is not used afterwards.
        unsafe { dialog.destroy() };
        return failed;
    };
    let fields: Vec<gtk::Entry> = match &request.entry {
        Entry::Message => {
            dialog.set_secondary_text(Some(&request.message));
            Vec::new()
        }
        Entry::Secret {
            placeholder,
            max_chars,
        } => {
            dialog.set_secondary_text(Some(&request.message));
            let secret = hidden_entry(placeholder.as_deref(), *max_chars);
            area.pack_start(&secret, false, false, 0);
            vec![secret]
        }
        Entry::SignIn => {
            dialog.set_secondary_text(Some(&request.message));
            let username = gtk::Entry::new();
            username.set_placeholder_text(Some("Username"));
            username.set_max_length(i32::try_from(MAX_USERNAME_CHARS + 1).unwrap_or(i32::MAX));
            username.set_activates_default(true);
            let password = hidden_entry(Some("Password"), MAX_PASSWORD_CHARS);
            area.pack_start(&username, false, false, 0);
            area.pack_start(&password, false, false, 0);
            vec![username, password]
        }
        Entry::Selectable => {
            let text = gtk::Label::new(Some(&request.message));
            text.set_selectable(true);
            text.set_line_wrap(true);
            text.set_max_width_chars(MESSAGE_WIDTH_CHARS);
            text.set_xalign(0.0);
            area.pack_start(&text, false, false, 0);
            Vec::new()
        }
    };
    // Like the topmost, foreground windows on Windows. A Wayland compositor
    // may ignore "keep above"; the window is still modal and focused.
    dialog.set_keep_above(true);
    dialog.set_position(gtk::WindowPosition::Center);
    dialog.show_all();
    match fields.first() {
        Some(first) => first.grab_focus(),
        None => {
            if let Some(default) = dialog.widget_for_response(gtk::ResponseType::Other(0)) {
                default.grab_focus();
            }
        }
    }
    dialog.present();
    let response = match dialog.run() {
        gtk::ResponseType::Other(id) => Response::Button(usize::from(id)),
        gtk::ResponseType::DeleteEvent => Response::Closed,
        _ => Response::Other,
    };
    let read = if response == Response::Button(0) {
        fields.iter().map(read_entry).collect()
    } else {
        Ok(Vec::new())
    };
    for field in &fields {
        field.set_text("");
    }
    // SAFETY: the dialog was created here; nothing uses it afterwards.
    unsafe { dialog.destroy() };
    settle();
    Answer {
        response,
        fields: read,
    }
}

impl CredentialSource for NativePrompt {
    fn ask(&self, host: &str) -> Result<Credentials, PromptError> {
        let (caption, message) = prompt_text::sign_in(host);
        sign_in_from(entered(prompt(sign_in_alert(caption, &message))?)?)
    }

    fn ask_secret(&self, request: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        let text = prompt_text::secret(&request);
        secret_from(entered(prompt(secret_alert(&text))?)?, text.max_chars)
    }

    fn show_recovery_key(&self, user_id: &str, key: &str) -> Result<(), PromptError> {
        let (caption, message) = prompt_text::recovery_key(user_id, key, Some(COPY_HINT));
        match prompt(recovery_alert(caption, &message))?.response {
            Response::Button(0) => Ok(()),
            _ => Err(PromptError::Failed),
        }
    }

    fn confirm_reset(&self) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::RESET;
        yes(prompt(question_alert(caption, message))?.response)
    }

    fn confirm_identity_change(
        &self,
        user_id: &str,
        was_verified: bool,
    ) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::identity_change(user_id, was_verified)?;
        yes(prompt(question_alert(caption, &message))?.response)
    }

    fn confirm_recovery_key_change(&self) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::RECOVERY_KEY_CHANGE;
        yes(prompt(question_alert(caption, message))?.response)
    }

    /// Any chooser error is `Failed`, so the key is reported as not saved;
    /// a cancel stays `Ok(None)`.
    fn choose_recovery_key_file(&self) -> Result<Option<PathBuf>, PromptError> {
        let (title, suggested) = prompt_text::SAVE_RECOVERY_KEY;
        super::pick::save_chooser(title, suggested).map_err(|_| PromptError::Failed)
    }

    fn confirm_open_link(&self, caption: &str, message: &str) -> Result<bool, PromptError> {
        yes(prompt(question_alert(caption, message))?.response)
    }

    /// Hands the checked link to the OpenURI portal. A link that does not
    /// pass the core's link check again is refused here too.
    fn open_link(&self, url: &str) -> Result<(), PromptError> {
        let checked = crate::links::checked(url).ok_or(PromptError::Failed)?;
        super::portal::open_uri(checked.as_str()).map_err(|_| PromptError::Failed)
    }
}

/// Asks whether to start the failed window again; `true` only for Retry.
/// No display, or a window that did not run, closes.
pub fn ask_to_retry() -> bool {
    prompt(retry_alert()).is_ok_and(|answer| answer.response == Response::Button(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{NewPassphrase, NewPassword};

    fn answer(response: Response, fields: &[&str]) -> Answer {
        Answer {
            response,
            fields: Ok(fields
                .iter()
                .map(|text| Zeroizing::new((*text).to_owned()))
                .collect()),
        }
    }

    fn texts(fields: &[&str]) -> Vec<Zeroizing<String>> {
        fields
            .iter()
            .map(|text| Zeroizing::new((*text).to_owned()))
            .collect()
    }

    /// GIVEN the reset, identity-change and link questions THEN each is a
    /// warning whose first (default) button is "No"; only "Yes" accepts,
    /// closing the window is "No", and a window that did not run fails.
    #[test]
    fn given_the_yes_or_no_questions_then_no_is_the_default() {
        let (caption, message) = prompt_text::RESET;
        let reset = question_alert(caption, message);
        let (caption, message) = prompt_text::identity_change("@alice:example.org", true).unwrap();
        let identity = question_alert(caption, &message);
        let url = crate::links::checked("https://example.org/a").unwrap();
        let (caption, message) = crate::links::question(&url).unwrap();
        let link = question_alert(caption, &message);
        for alert in [reset, identity, link] {
            assert_eq!(alert.style, Style::Warning);
            assert_eq!(alert.buttons, ["No", "Yes"]);
            assert_eq!(alert.entry, Entry::Message);
        }
        assert_eq!(yes(Response::Button(0)), Ok(false));
        assert_eq!(yes(Response::Button(1)), Ok(true));
        assert_eq!(yes(Response::Closed), Ok(false));
        assert_eq!(yes(Response::Other), Err(PromptError::Failed));
        assert_eq!(yes(Response::Button(2)), Err(PromptError::Failed));
    }

    /// GIVEN each secret prompt THEN it has one hidden entry with the fixed
    /// placeholder and the prompt's own limit, OK as default and Cancel;
    /// GIVEN sign-in THEN it names the host and has both entries.
    #[test]
    fn given_secret_prompts_then_each_has_one_hidden_entry() {
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
                step: NewPassword::Mismatch,
            },
            SecretPrompt::RegistrationToken { host: "b" },
            SecretPrompt::PhoneCode { host: "b" },
        ] {
            let text = prompt_text::secret(&request);
            let alert = secret_alert(&text);
            assert_eq!(
                alert.entry,
                Entry::Secret {
                    placeholder: text.field.map(str::to_owned),
                    max_chars: text.max_chars,
                }
            );
            assert!(text.max_chars <= MAX_FIELD_CHARS);
            assert_eq!(alert.buttons, ["OK", "Cancel"]);
            assert_eq!(alert.message.as_str(), text.message);
        }
        let (caption, message) = prompt_text::sign_in("example.org");
        let sign_in = sign_in_alert(caption, &message);
        assert_eq!(sign_in.entry, Entry::SignIn);
        assert_eq!(sign_in.buttons, ["Sign In", "Cancel"]);
        assert!(sign_in.message.contains("example.org"));
    }

    /// GIVEN how a window closed THEN the first button gives the entries,
    /// the second button or closing cancels, and anything else fails.
    #[test]
    fn given_window_responses_then_only_the_first_button_gives_entries() {
        let fields = entered(answer(Response::Button(0), &["user", "secret"])).unwrap();
        assert_eq!(fields.len(), 2);
        for cancel in [Response::Button(1), Response::Closed] {
            assert_eq!(
                entered(answer(cancel, &[])).err(),
                Some(PromptError::Cancelled)
            );
        }
        assert_eq!(
            entered(answer(Response::Other, &[])).err(),
            Some(PromptError::Failed)
        );
        let refused = Answer {
            response: Response::Button(0),
            fields: Err(PromptError::Invalid),
        };
        assert_eq!(entered(refused).err(), Some(PromptError::Invalid));
    }

    /// GIVEN entry text THEN it is accepted only when not empty, within the
    /// limit in characters (not bytes), valid UTF-8 and without control
    /// characters; text over the read limit is refused before any copy.
    #[test]
    fn given_entry_text_then_the_limits_and_checks_apply() {
        let read = |text: &str| field_text(text.as_bytes(), text.chars().count());
        assert_eq!(
            secret_from(vec![read("pässword").unwrap()], 8)
                .unwrap()
                .as_str(),
            "pässword"
        );
        assert_eq!(
            secret_from(vec![read("😀😀").unwrap()], 2)
                .unwrap()
                .as_str(),
            "😀😀"
        );
        for bad in ["", "123456789", "a\nb", "a\u{7f}b"] {
            assert_eq!(
                secret_from(vec![read(bad).unwrap()], 8).err(),
                Some(PromptError::Invalid),
                "{bad:?}"
            );
        }
        assert_eq!(
            field_text(b"\xff\xfe", 2).err(),
            Some(PromptError::Invalid),
            "not UTF-8"
        );
        let longest = "x".repeat(MAX_FIELD_CHARS);
        assert_eq!(read(&longest).unwrap().len(), MAX_FIELD_CHARS);
        assert_eq!(
            read(&format!("{longest}x")).err(),
            Some(PromptError::Invalid)
        );
        assert_eq!(
            secret_from(texts(&["a", "b"]), 8).err(),
            Some(PromptError::Failed)
        );
        assert_eq!(secret_from(Vec::new(), 8).err(), Some(PromptError::Failed));
    }

    /// GIVEN sign-in entries THEN both must pass their own limits, and any
    /// other number of entries fails.
    #[test]
    fn given_sign_in_entries_then_both_are_checked() {
        let credentials = sign_in_from(texts(&["alice", "secret"])).unwrap();
        assert_eq!(credentials.username.as_str(), "alice");
        assert_eq!(credentials.password.as_str(), "secret");
        let long_user = "u".repeat(MAX_USERNAME_CHARS + 1);
        let long_password = "p".repeat(MAX_PASSWORD_CHARS + 1);
        for (username, password) in [
            ("", "secret"),
            ("alice", ""),
            (long_user.as_str(), "secret"),
            ("alice", long_password.as_str()),
            ("al\tice", "secret"),
        ] {
            assert_eq!(
                sign_in_from(texts(&[username, password])).err(),
                Some(PromptError::Invalid)
            );
        }
        assert_eq!(
            sign_in_from(texts(&["alice"])).err(),
            Some(PromptError::Failed)
        );
        assert_eq!(
            sign_in_from(texts(&["a", "b", "c"])).err(),
            Some(PromptError::Failed)
        );
    }

    /// GIVEN a new recovery key THEN it is shown as selectable text that
    /// ends with the Linux copy hint, with one OK button; the retry and
    /// notice windows have their fixed buttons.
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
                .ends_with("\n\nSelect the key and press Ctrl+C to copy it.")
        );
        assert_eq!(retry_alert().buttons, ["Retry", "Cancel"]);
        assert_eq!(notice_alert("c", "m").buttons, ["OK"]);
    }

    /// GIVEN text with a NUL THEN the window shows it whole, with the NUL
    /// replaced, instead of ending at it.
    #[test]
    fn given_text_with_a_nul_then_it_is_not_cut_short() {
        let alert = question_alert("open\0 this", "https://a.example/\0https://b.example/");
        assert_eq!(alert.caption, "open\u{FFFD} this");
        assert_eq!(
            alert.message.as_str(),
            "https://a.example/\u{FFFD}https://b.example/"
        );
    }

    /// GIVEN an address that fails the core's link check WHEN it is opened
    /// THEN it is refused before the portal is asked; GIVEN an unprintable
    /// user ID THEN the identity question is refused before any window.
    #[test]
    fn given_unchecked_input_then_it_is_refused_before_any_window() {
        for url in ["file:///etc/passwd", "javascript:alert(1)", "https://a b"] {
            assert_eq!(
                NativePrompt.open_link(url),
                Err(PromptError::Failed),
                "{url}"
            );
        }
        assert_eq!(
            NativePrompt.confirm_identity_change("@a:b\u{202e}c", false),
            Err(PromptError::Invalid)
        );
    }
}
