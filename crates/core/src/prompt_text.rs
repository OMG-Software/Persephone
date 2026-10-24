//! The words of the core's own windows, shared by every platform so that the
//! Windows, Linux and macOS prompts say the same thing. Each platform module
//! only draws them (`src/platform/<os>/credentials.rs`).

use crate::credentials::{
    MAX_PASSWORD_CHARS, MAX_RECOVERY_KEY_CHARS, NewPassphrase, PromptError, SecretPrompt,
};
use crate::passphrase::MIN_PASSPHRASE_CHARS;
use zeroize::Zeroizing;

/// What a secret prompt shows, and the longest answer it accepts.
pub struct SecretText<'a> {
    pub caption: &'static str,
    pub message: String,
    /// Fixed text for the username field, where the dialog has one.
    pub field: Option<&'a str>,
    pub max_chars: usize,
}

/// The sign-in window: caption and message.
pub fn sign_in(host: &str) -> (&'static str, String) {
    (
        "Sign in to Persephone",
        format!(
            "Enter your Matrix username and password for {host}. \
             Only Persephone's core sees the password."
        ),
    )
}

/// The window for one of the core's secrets.
pub fn secret<'a>(request: &SecretPrompt<'a>) -> SecretText<'a> {
    match *request {
        SecretPrompt::Password { user_id, host } => SecretText {
            caption: "Set up encryption in Persephone",
            message: format!(
                "Enter the password for {user_id} on {host} to set up \
                 encryption for this account. Only Persephone's core sees it."
            ),
            field: Some(user_id),
            max_chars: MAX_PASSWORD_CHARS,
        },
        SecretPrompt::RecoveryKey { user_id } => SecretText {
            caption: "Verify this device with your recovery key",
            message: format!(
                "Type the recovery key for {user_id} in the password field. \
                 Only Persephone's core sees it."
            ),
            field: Some("Recovery key"),
            max_chars: MAX_RECOVERY_KEY_CHARS,
        },
        SecretPrompt::Passphrase { retry } => SecretText {
            caption: "Unlock Persephone",
            message: if retry {
                "That passphrase was not correct. Type the passphrase for \
                 Persephone on this computer in the password field. Only \
                 Persephone's core sees it."
            } else {
                "Type the passphrase for Persephone on this computer in the \
                 password field. Only Persephone's core sees it."
            }
            .to_owned(),
            field: Some("Profile passphrase"),
            max_chars: MAX_PASSWORD_CHARS,
        },
        SecretPrompt::NewPassphrase(step) => {
            let choose = format!(
                "Choose a passphrase of at least {MIN_PASSPHRASE_CHARS} characters \
                 and type it in the password field. Persephone asks for it each \
                 time it opens. If you forget it, you must reset this computer's \
                 profile and sign in again; encrypted history comes back only from \
                 key backup."
            );
            let message = match step {
                NewPassphrase::Choose => choose,
                NewPassphrase::Upgrade => format!(
                    "Persephone now protects this computer's profile with a \
                     passphrase. {choose}"
                ),
                NewPassphrase::Confirm => {
                    "Type the same passphrase again to confirm it.".to_owned()
                }
                NewPassphrase::TooShort => format!("That passphrase was too short. {choose}"),
                NewPassphrase::Mismatch => format!("The two passphrases were different. {choose}"),
            };
            SecretText {
                caption: "Protect Persephone on this computer",
                message,
                field: Some("New profile passphrase"),
                max_chars: MAX_PASSWORD_CHARS,
            }
        }
    }
}

/// The reset warning: caption and message. "No" must be the default answer.
pub const RESET: (&str, &str) = (
    "Reset Persephone on this computer?",
    "This deletes Persephone's profile on this computer: its messages, \
     keys and sign-in. It cannot be undone.\n\n\
     Your account stays on the server. This device stays signed in there \
     until you remove it from another client. Encrypted history comes back \
     only from key backup, with your recovery key.\n\n\
     Delete the profile and choose a new passphrase?",
);

/// The identity-change question (ADR-0007): caption and message. "No" must
/// be the default answer. Refuses a user ID that is not printable ASCII, so
/// it cannot reorder or hide the surrounding text.
pub fn identity_change(
    user_id: &str,
    was_verified: bool,
) -> Result<(&'static str, String), PromptError> {
    if user_id.len() > 255 || !user_id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(PromptError::Invalid);
    }
    let before = if was_verified {
        "You verified this person before. Their encryption identity is now a \
         different one, so it is no longer verified."
    } else {
        "This person's encryption identity is not the one Persephone saw \
         before."
    };
    let after = if was_verified {
        "Accepting withdraws your verification of the old identity. You \
         can then verify the new one by comparing emoji."
    } else {
        "You can also verify them by comparing emoji instead."
    };
    Ok((
        "A contact's identity changed",
        format!(
            "{user_id}\n\n{before}\n\n\
             This happens when they reset their encryption, for example after \
             losing their devices. It also happens when someone who controls \
             the server tries to read your messages.\n\n\
             Persephone sends nothing in rooms with this person until you \
             decide. Ask them in person or on a call you trust whether they \
             reset their encryption.\n\n{after}\n\n\
             Accept the new identity and send to it?"
        ),
    ))
}

/// The recovery key window: caption and message. `copy_hint` is the
/// platform's own way to copy the text, if it has one.
pub fn recovery_key(
    user_id: &str,
    key: &str,
    copy_hint: Option<&str>,
) -> (&'static str, Zeroizing<String>) {
    let mut message = Zeroizing::new(format!(
        "This is the recovery key for {user_id}. Write it down or save it \
         somewhere safe now: Persephone does not keep it, and it is shown \
         only once. You need it to read your encrypted messages on a new \
         device.\n\n{key}"
    ));
    if let Some(hint) = copy_hint {
        message.push_str("\n\n");
        message.push_str(hint);
    }
    ("Your Persephone recovery key", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN a user ID with a control or non-ASCII character WHEN the
    /// identity-change text is built THEN it is refused.
    #[test]
    fn given_an_unprintable_user_id_then_the_identity_question_is_refused() {
        for user_id in ["@a:b\nc", "@a:b\u{202e}c", "@a b:c"] {
            assert!(matches!(
                identity_change(user_id, false),
                Err(PromptError::Invalid)
            ));
        }
        assert!(identity_change(&"a".repeat(256), true).is_err());
        let (_, message) = identity_change("@alice:example.org", true).unwrap();
        assert!(message.starts_with("@alice:example.org\n\n"));
        assert!(message.contains("no longer verified"));
    }

    /// GIVEN each secret prompt WHEN its text is built THEN it has the
    /// matching limit and field label.
    #[test]
    fn given_secret_prompts_then_each_has_its_limit_and_field() {
        let password = secret(&SecretPrompt::Password {
            user_id: "@a:b",
            host: "b",
        });
        assert_eq!(
            (password.field, password.max_chars),
            (Some("@a:b"), MAX_PASSWORD_CHARS)
        );
        let recovery = secret(&SecretPrompt::RecoveryKey { user_id: "@a:b" });
        assert_eq!(recovery.max_chars, MAX_RECOVERY_KEY_CHARS);
        let retry = secret(&SecretPrompt::Passphrase { retry: true });
        assert!(
            retry
                .message
                .starts_with("That passphrase was not correct.")
        );
        let short = secret(&SecretPrompt::NewPassphrase(NewPassphrase::TooShort));
        assert!(short.message.contains(&MIN_PASSPHRASE_CHARS.to_string()));
    }

    /// GIVEN a copy hint WHEN the recovery key text is built THEN the hint
    /// follows the key, and without one the key ends the message.
    #[test]
    fn given_a_copy_hint_then_it_follows_the_recovery_key() {
        let (_, with) = recovery_key("@a:b", "KEY", Some("Press Ctrl+C to copy this message."));
        assert!(with.ends_with("KEY\n\nPress Ctrl+C to copy this message."));
        let (_, without) = recovery_key("@a:b", "KEY", None);
        assert!(without.ends_with("KEY"));
    }
}
