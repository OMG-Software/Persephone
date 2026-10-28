//! The words of the core's own windows, shared by every platform so that the
//! Windows, Linux and macOS prompts say the same thing. Each platform module
//! only draws them (`src/platform/<os>/credentials.rs`).

use crate::credentials::{
    MAX_PASSWORD_CHARS, MAX_PHONE_CODE_CHARS, MAX_RECOVERY_KEY_CHARS, MAX_REGISTRATION_TOKEN_CHARS,
    NewPassphrase, NewPassword, PasswordPurpose, PromptError, SecretPrompt,
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
        SecretPrompt::NewAccountPassword {
            username,
            host,
            step,
        } => {
            let choose = format!(
                "Choose a password for the new account {username} on {host} and \
                 type it in the password field. Only Persephone's core sees it."
            );
            SecretText {
                caption: "Create a Matrix account",
                message: match step {
                    NewPassword::Choose => choose,
                    NewPassword::Confirm => {
                        "Type the same password again to confirm it.".to_owned()
                    }
                    NewPassword::Mismatch => {
                        format!("The two passwords were different. {choose}")
                    }
                },
                field: Some(username),
                max_chars: MAX_PASSWORD_CHARS,
            }
        }
        SecretPrompt::RegistrationToken { host } => SecretText {
            caption: "Create a Matrix account",
            message: format!(
                "{host} asks for a registration token to create an account. \
                 Type the token you were given in the password field. Only \
                 Persephone's core sees it."
            ),
            field: Some("Registration token"),
            max_chars: MAX_REGISTRATION_TOKEN_CHARS,
        },
        SecretPrompt::CurrentPassword {
            user_id,
            host,
            purpose,
        } => SecretText {
            caption: match purpose {
                PasswordPurpose::ChangePassword => "Change your Matrix password",
                PasswordPurpose::AddThreePid => "Add an email address or phone number",
            },
            message: format!(
                "Enter the current password for {user_id} on {host} to approve {}. \
                 Only Persephone's core sees it.",
                match purpose {
                    PasswordPurpose::ChangePassword => "the new password",
                    PasswordPurpose::AddThreePid => "adding it to your account",
                }
            ),
            field: Some(user_id),
            max_chars: MAX_PASSWORD_CHARS,
        },
        SecretPrompt::ChangedPassword {
            user_id,
            host,
            step,
        } => {
            let choose = format!(
                "Choose a new password for {user_id} on {host} and type it in the \
                 password field. Only Persephone's core sees it."
            );
            SecretText {
                caption: "Change your Matrix password",
                message: match step {
                    NewPassword::Choose => choose,
                    NewPassword::Confirm => {
                        "Type the same new password again to confirm it.".to_owned()
                    }
                    NewPassword::Mismatch => {
                        format!("The two new passwords were different. {choose}")
                    }
                },
                field: Some("New password"),
                max_chars: MAX_PASSWORD_CHARS,
            }
        }
        SecretPrompt::PhoneCode { host } => SecretText {
            caption: "Confirm your phone number",
            message: format!(
                "{host} sent a code to the phone number you are adding. Type the \
                 code in the password field. Only Persephone's core sees it."
            ),
            field: Some("Code from the text message"),
            max_chars: MAX_PHONE_CODE_CHARS,
        },
    }
}

/// The recovery-key change warning (ADR-0021): caption and message. "No"
/// must be the default answer.
pub const RECOVERY_KEY_CHANGE: (&str, &str) = (
    "Change your recovery key?",
    "Persephone makes a new recovery key and shows it once. The old key \
     stops working at once: delete or destroy every copy of it, and keep \
     the new one somewhere safe.\n\n\
     Your encrypted messages and this device's verification are not \
     affected.\n\n\
     Make a new recovery key?",
);

/// The Save dialog's title for a new recovery key, and the file name it
/// suggests.
pub const SAVE_RECOVERY_KEY: (&str, &str) = (
    "Save your recovery key to a file (Cancel to skip)",
    "persephone-recovery-key.txt",
);

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
         device. After this window, Persephone offers to save it to a file \
         as well.\n\n{key}"
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
        let token = secret(&SecretPrompt::RegistrationToken { host: "b" });
        assert_eq!(
            (token.field, token.max_chars),
            (Some("Registration token"), MAX_REGISTRATION_TOKEN_CHARS)
        );
        let account = secret(&SecretPrompt::NewAccountPassword {
            username: "al",
            host: "b",
            step: NewPassword::Mismatch,
        });
        assert_eq!(account.field, Some("al"));
        assert!(
            account
                .message
                .starts_with("The two passwords were different.")
        );
    }

    /// GIVEN every secret prompt WHEN its text is built THEN the message has
    /// no run of spaces or stray line breaks from the source layout.
    #[test]
    fn given_secret_prompts_then_messages_have_no_layout_gaps() {
        let mut prompts = vec![
            SecretPrompt::Password {
                user_id: "@a:b",
                host: "b",
            },
            SecretPrompt::RecoveryKey { user_id: "@a:b" },
            SecretPrompt::Passphrase { retry: false },
            SecretPrompt::Passphrase { retry: true },
            SecretPrompt::RegistrationToken { host: "b" },
            SecretPrompt::PhoneCode { host: "b" },
            SecretPrompt::CurrentPassword {
                user_id: "@a:b",
                host: "b",
                purpose: PasswordPurpose::ChangePassword,
            },
            SecretPrompt::CurrentPassword {
                user_id: "@a:b",
                host: "b",
                purpose: PasswordPurpose::AddThreePid,
            },
        ];
        for step in [
            NewPassphrase::Choose,
            NewPassphrase::Upgrade,
            NewPassphrase::Confirm,
            NewPassphrase::TooShort,
            NewPassphrase::Mismatch,
        ] {
            prompts.push(SecretPrompt::NewPassphrase(step));
        }
        for step in [
            NewPassword::Choose,
            NewPassword::Confirm,
            NewPassword::Mismatch,
        ] {
            prompts.push(SecretPrompt::NewAccountPassword {
                username: "al",
                host: "b",
                step,
            });
            prompts.push(SecretPrompt::ChangedPassword {
                user_id: "@a:b",
                host: "b",
                step,
            });
        }
        for prompt in prompts {
            let message = secret(&prompt).message;
            assert!(
                !message.contains("  ") && !message.contains('\n'),
                "{prompt:?}: {message:?}"
            );
        }
    }

    /// GIVEN the account-change prompts WHEN their text is built THEN each
    /// has its own limit and field, and the current password names the
    /// account it approves for.
    #[test]
    fn given_account_change_prompts_then_each_has_its_limit_and_field() {
        let current = secret(&SecretPrompt::CurrentPassword {
            user_id: "@a:b",
            host: "b",
            purpose: PasswordPurpose::AddThreePid,
        });
        assert_eq!(
            (current.field, current.max_chars),
            (Some("@a:b"), MAX_PASSWORD_CHARS)
        );
        assert!(current.message.contains("adding it to your account"));
        let changed = secret(&SecretPrompt::ChangedPassword {
            user_id: "@a:b",
            host: "b",
            step: NewPassword::Mismatch,
        });
        assert_eq!(changed.field, Some("New password"));
        assert!(
            changed
                .message
                .starts_with("The two new passwords were different.")
        );
        let code = secret(&SecretPrompt::PhoneCode { host: "b" });
        assert_eq!(code.max_chars, MAX_PHONE_CODE_CHARS);
        // The warning says the old key stops working before it asks.
        let (_, warning) = RECOVERY_KEY_CHANGE;
        assert!(warning.contains("old key stops working"));
        assert!(warning.ends_with("Make a new recovery key?"));
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
