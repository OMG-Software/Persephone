//! Sign-in credentials, asked for by the core itself.
//!
//! The password never passes through the UI process or the web view: the core
//! shows the operating system's own credential dialog, naming the homeserver
//! host so the user can see where the password goes even if the page was
//! compromised. Buffers that held the password are zeroed. The SDK copies the
//! password into its login request; that copy is outside this guarantee.
//!
//! The same holds for encryption secrets (ADR-0005): the account password
//! asked again to set up cross-signing and the recovery key are entered in the
//! core's own dialog, and a new recovery key is shown in a native window,
//! never in the page. So is the profile passphrase (ADR-0006), and the
//! warning that confirms a profile reset.

use zeroize::Zeroizing;

/// Longest username accepted (a Matrix user ID is at most 255 bytes).
pub const MAX_USERNAME_CHARS: usize = 255;
/// Longest password accepted.
pub const MAX_PASSWORD_CHARS: usize = 1024;

pub struct Credentials {
    pub username: Zeroizing<String>,
    pub password: Zeroizing<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptError {
    /// The user closed or cancelled the dialog.
    Cancelled,
    /// The entry was empty, too long or not valid text.
    Invalid,
    /// This platform has no native prompt yet.
    Unsupported,
    /// The dialog could not be shown or read.
    Failed,
}

/// Longest recovery key or passphrase accepted.
pub const MAX_RECOVERY_KEY_CHARS: usize = 1024;

/// A secret the core asks for after sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretPrompt<'a> {
    /// The account password again, to approve setting up encryption.
    Password { user_id: &'a str, host: &'a str },
    /// The recovery key (or its passphrase), to verify this device.
    RecoveryKey { user_id: &'a str },
    /// The passphrase that opens this computer's profile; `retry` after a
    /// wrong entry.
    Passphrase { retry: bool },
    /// A new profile passphrase.
    NewPassphrase(NewPassphrase),
}

/// Which step of choosing a new profile passphrase a prompt is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewPassphrase {
    /// A new profile.
    Choose,
    /// A profile from before passphrases (ADR-0006).
    Upgrade,
    /// The same passphrase again.
    Confirm,
    /// The last entry was shorter than the minimum.
    TooShort,
    /// The two entries differed.
    Mismatch,
}

/// Asks the user for credentials and secrets in the core's own windows.
pub trait CredentialSource: Send + Sync {
    /// A username and password for `host`.
    fn ask(&self, host: &str) -> Result<Credentials, PromptError>;

    /// One secret. Unsupported unless a source provides it.
    fn ask_secret(&self, _prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        Err(PromptError::Unsupported)
    }

    /// Shows a new recovery key so the user can save it. Unsupported unless a
    /// source provides it; the key is never kept.
    fn show_recovery_key(&self, _user_id: &str, _key: &str) -> Result<(), PromptError> {
        Err(PromptError::Unsupported)
    }

    /// Asks whether to delete this computer's profile. `Ok(true)` only for an
    /// explicit yes. Unsupported unless a source provides it.
    fn confirm_reset(&self) -> Result<bool, PromptError> {
        Err(PromptError::Unsupported)
    }

    /// Asks whether to accept the new identity of `user_id` (ADR-0007);
    /// `was_verified` when this account had verified the old one. `Ok(true)`
    /// only for an explicit yes. Unsupported unless a source provides it.
    fn confirm_identity_change(
        &self,
        _user_id: &str,
        _was_verified: bool,
    ) -> Result<bool, PromptError> {
        Err(PromptError::Unsupported)
    }
}

/// Checks a secret the way a password is checked: not empty, within `max`
/// characters, no control characters.
pub fn checked_secret(
    secret: Zeroizing<String>,
    max: usize,
) -> Result<Zeroizing<String>, PromptError> {
    if secret.is_empty() || secret.chars().count() > max || secret.chars().any(char::is_control) {
        return Err(PromptError::Invalid);
    }
    Ok(secret)
}

/// The platform's native prompt.
pub fn platform() -> Box<dyn CredentialSource> {
    #[cfg(windows)]
    return Box::new(windows::NativePrompt);
    #[cfg(not(windows))]
    return Box::new(Unsupported);
}

pub struct Unsupported;

impl CredentialSource for Unsupported {
    fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
        Err(PromptError::Unsupported)
    }
}

/// Builds credentials from UTF-16 text without its terminator. The decoded
/// strings go straight into zeroed-on-drop storage.
pub fn from_wide(username: &[u16], password: &[u16]) -> Result<Credentials, PromptError> {
    let decode = |text: &[u16], max: usize| -> Result<Zeroizing<String>, PromptError> {
        let text = Zeroizing::new(String::from_utf16(text).map_err(|_| PromptError::Invalid)?);
        if text.is_empty() || text.chars().count() > max || text.chars().any(char::is_control) {
            return Err(PromptError::Invalid);
        }
        Ok(text)
    };
    Ok(Credentials {
        username: decode(username, MAX_USERNAME_CHARS)?,
        password: decode(password, MAX_PASSWORD_CHARS)?,
    })
}

/// Development only: answers the prompt from the disposable-server harness
/// profile (`tools/servers/run.py` writes it under the ignored `.local`), for
/// the account whose homeserver has the prompted host. Never in default, CI or
/// release builds.
#[cfg(feature = "dev-disposable-servers")]
pub mod dev {
    use super::{
        CredentialSource, Credentials, MAX_PASSWORD_CHARS, MAX_RECOVERY_KEY_CHARS, PromptError,
        SecretPrompt, checked_secret,
    };
    use std::path::{Component, Path, PathBuf};
    use zeroize::Zeroizing;

    const MAX_PROFILE_BYTES: u64 = 64 * 1024;
    const MAX_RECOVERY_FILE_BYTES: u64 = 4 * 1024;

    #[derive(serde::Deserialize)]
    struct Entry {
        base_url: String,
        username: String,
        password: String,
    }

    pub struct HarnessProfile {
        path: PathBuf,
        recovery_file: Option<PathBuf>,
        passphrase_file: Option<PathBuf>,
        identity_answer_file: Option<PathBuf>,
    }

    impl HarnessProfile {
        pub fn new(path: PathBuf) -> Self {
            Self {
                path,
                recovery_file: None,
                passphrase_file: None,
                identity_answer_file: None,
            }
        }

        /// Answers "accept this new identity?" from `path` instead of the
        /// native window: `yes` accepts, anything else declines. Read each
        /// time it is asked, so a run can test both answers. Under the same
        /// `.local` rule as the recovery file.
        pub fn with_identity_answer_file(mut self, path: PathBuf) -> Self {
            self.identity_answer_file = Some(path);
            self
        }

        /// Answers the profile passphrase prompts from `path` (its trimmed
        /// contents), under the same `.local` rule as the recovery file. Without
        /// it, those prompts and the reset warning use the native windows.
        pub fn with_passphrase_file(mut self, path: PathBuf) -> Self {
            self.passphrase_file = Some(path);
            self
        }

        /// Shows and reads recovery keys through `path` instead of a window,
        /// so automated runs can recover a second device. Refused unless the
        /// path is inside a `.local` directory (ignored by git).
        pub fn with_recovery_file(mut self, path: PathBuf) -> Self {
            self.recovery_file = Some(path);
            self
        }

        /// The recovery-key file, resolved by [`local_file`].
        fn recovery_file(&self) -> Result<PathBuf, PromptError> {
            local_file(
                self.recovery_file
                    .as_ref()
                    .ok_or(PromptError::Unsupported)?,
            )
        }

        fn read_secret(path: &Path, max: usize) -> Result<Zeroizing<String>, PromptError> {
            use std::io::Read;
            let mut bytes = Zeroizing::new(Vec::new());
            std::fs::File::open(path)
                .and_then(|file| {
                    file.take(MAX_RECOVERY_FILE_BYTES + 1)
                        .read_to_end(&mut bytes)
                })
                .map_err(|_| PromptError::Failed)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| PromptError::Invalid)?;
            checked_secret(Zeroizing::new(text.trim().to_owned()), max)
        }
    }

    /// `path`, resolved: it must stay inside the `.local` directory it names
    /// once `..` and links are followed, and an existing one must be a plain
    /// file, not a link.
    pub(crate) fn local_file(path: &Path) -> Result<PathBuf, PromptError> {
        let parts: Vec<Component> = path.components().collect();
        let local_at = parts
            .iter()
            .position(|part| *part == Component::Normal(".local".as_ref()))
            .ok_or(PromptError::Unsupported)?;
        let local: PathBuf = parts[..=local_at].iter().collect();
        let local = std::fs::canonicalize(local).map_err(|_| PromptError::Unsupported)?;
        let name = path.file_name().ok_or(PromptError::Unsupported)?;
        let parent = path.parent().ok_or(PromptError::Unsupported)?;
        let parent = std::fs::canonicalize(parent).map_err(|_| PromptError::Unsupported)?;
        if !parent.starts_with(&local) {
            return Err(PromptError::Unsupported);
        }
        let resolved = parent.join(name);
        match std::fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.file_type().is_file() => Ok(resolved),
            Ok(_) => Err(PromptError::Unsupported),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
            Err(_) => Err(PromptError::Unsupported),
        }
    }

    impl HarnessProfile {
        fn entry(&self, host: &str) -> Result<Entry, PromptError> {
            use std::io::Read;
            let mut bytes = Zeroizing::new(Vec::new());
            std::fs::File::open(&self.path)
                .and_then(|file| file.take(MAX_PROFILE_BYTES + 1).read_to_end(&mut bytes))
                .map_err(|_| PromptError::Failed)?;
            if bytes.len() as u64 > MAX_PROFILE_BYTES {
                return Err(PromptError::Failed);
            }
            let entries: Vec<Entry> =
                serde_json::from_slice(&bytes).map_err(|_| PromptError::Failed)?;
            let entry = entries
                .into_iter()
                .find(|entry| {
                    url::Url::parse(&entry.base_url).is_ok_and(|url| {
                        let shown = match url.port() {
                            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
                            None => url.host_str().unwrap_or_default().to_owned(),
                        };
                        shown == host
                    })
                })
                .ok_or(PromptError::Cancelled)?;
            Ok(entry)
        }
    }

    impl CredentialSource for HarnessProfile {
        fn ask(&self, host: &str) -> Result<Credentials, PromptError> {
            let entry = self.entry(host)?;
            Ok(Credentials {
                username: Zeroizing::new(entry.username),
                // The same checks as the native prompt's.
                password: checked_secret(Zeroizing::new(entry.password), MAX_PASSWORD_CHARS)?,
            })
        }

        fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
            match prompt {
                SecretPrompt::Password { host, .. } => checked_secret(
                    Zeroizing::new(self.entry(host)?.password),
                    MAX_PASSWORD_CHARS,
                ),
                SecretPrompt::RecoveryKey { .. } => {
                    Self::read_secret(&self.recovery_file()?, MAX_RECOVERY_KEY_CHARS)
                }
                SecretPrompt::Passphrase { .. } | SecretPrompt::NewPassphrase(_) => {
                    match &self.passphrase_file {
                        Some(path) => Self::read_secret(&local_file(path)?, MAX_PASSWORD_CHARS),
                        None => super::platform().ask_secret(prompt),
                    }
                }
            }
        }

        fn confirm_reset(&self) -> Result<bool, PromptError> {
            super::platform().confirm_reset()
        }

        fn confirm_identity_change(
            &self,
            user_id: &str,
            was_verified: bool,
        ) -> Result<bool, PromptError> {
            match &self.identity_answer_file {
                Some(path) => Ok(Self::read_secret(&local_file(path)?, 16)?.as_str() == "yes"),
                None => super::platform().confirm_identity_change(user_id, was_verified),
            }
        }

        /// Writes the key to the recovery file, readable by its owner only on
        /// Unix (a new file is created so; an existing one is narrowed
        /// first). On Windows the file takes its folder's ACL, which under
        /// the user's profile is already the owner's.
        fn show_recovery_key(&self, _user_id: &str, key: &str) -> Result<(), PromptError> {
            use std::io::Write;
            let path = self.recovery_file()?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                options.mode(0o600);
                if path.exists() {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|_| PromptError::Failed)?;
                }
            }
            options
                .open(&path)
                .and_then(|mut file| file.write_all(key.as_bytes()))
                .map_err(|_| PromptError::Failed)
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{
        CredentialSource, Credentials, MAX_PASSWORD_CHARS, MAX_RECOVERY_KEY_CHARS, NewPassphrase,
        PromptError, SecretPrompt, checked_secret, from_wide,
    };
    use crate::passphrase::MIN_PASSPHRASE_CHARS;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::{ERROR_CANCELLED, ERROR_INSUFFICIENT_BUFFER, GetLastError},
        Security::Credentials::{
            CRED_PACK_GENERIC_CREDENTIALS, CRED_PACK_PROTECTED_CREDENTIALS, CREDUI_INFOW,
            CREDUIWIN_GENERIC, CredPackAuthenticationBufferW, CredUIPromptForWindowsCredentialsW,
            CredUnPackAuthenticationBufferW,
        },
        System::Com::CoTaskMemFree,
        UI::WindowsAndMessaging::{
            IDNO, IDOK, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
            MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
        },
    };
    use zeroize::Zeroizing;

    pub struct NativePrompt;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// The dialog's packed credential buffer; zeroed before it is freed.
    struct AuthBuffer {
        data: *mut core::ffi::c_void,
        size: u32,
    }

    impl Drop for AuthBuffer {
        fn drop(&mut self) {
            if !self.data.is_null() {
                // SAFETY: `data` is the dialog's CoTaskMemAlloc allocation of
                // `size` bytes, owned only here.
                unsafe {
                    std::ptr::write_bytes(self.data.cast::<u8>(), 0, self.size as usize);
                    CoTaskMemFree(self.data);
                }
            }
        }
    }

    /// The text before the first NUL. The lengths the unpack call reports are
    /// buffer sizes and can exceed the text plus one terminator.
    fn text(buffer: &[u16], length: u32) -> &[u16] {
        let length = (length as usize).min(buffer.len());
        let text = &buffer[..length];
        text.iter()
            .position(|c| *c == 0)
            .map_or(text, |end| &text[..end])
    }

    /// Packs `user` with an empty password, to fill in the dialog's
    /// username.
    fn prefill(user: &str) -> Result<Vec<u8>, PromptError> {
        let user = wide(user);
        let empty = wide("");
        let mut size = 0u32;
        // SAFETY: a null output buffer asks only for the size.
        unsafe {
            CredPackAuthenticationBufferW(
                CRED_PACK_GENERIC_CREDENTIALS,
                user.as_ptr(),
                empty.as_ptr(),
                null_mut(),
                &mut size,
            )
        };
        let mut packed = vec![0u8; size as usize];
        // SAFETY: `packed` holds `size` bytes.
        let ok = unsafe {
            CredPackAuthenticationBufferW(
                CRED_PACK_GENERIC_CREDENTIALS,
                user.as_ptr(),
                empty.as_ptr(),
                packed.as_mut_ptr(),
                &mut size,
            )
        };
        if ok == 0 {
            return Err(PromptError::Failed);
        }
        packed.truncate(size as usize);
        Ok(packed)
    }

    /// Shows the credential dialog and unpacks what the user entered.
    fn prompt(
        caption: &str,
        message: &str,
        user: Option<&str>,
    ) -> Result<Credentials, PromptError> {
        let caption = wide(caption);
        let message = wide(message);
        let input = user.map(prefill).transpose()?;
        let info = CREDUI_INFOW {
            cbSize: size_of::<CREDUI_INFOW>() as u32,
            hwndParent: null_mut(),
            pszMessageText: message.as_ptr(),
            pszCaptionText: caption.as_ptr(),
            hbmBanner: null_mut(),
        };
        let mut package = 0u32;
        let mut buffer = AuthBuffer {
            data: null_mut(),
            size: 0,
        };
        let mut save = 0;
        let (input_data, input_size) = match &input {
            Some(packed) => (packed.as_ptr().cast(), packed.len() as u32),
            None => (null(), 0),
        };
        // SAFETY: every pointer is valid for the call; the dialog fills
        // `buffer`, which `AuthBuffer` then owns. No save checkbox.
        let result = unsafe {
            CredUIPromptForWindowsCredentialsW(
                &info,
                0,
                &mut package,
                input_data,
                input_size,
                &mut buffer.data,
                &mut buffer.size,
                &mut save,
                CREDUIWIN_GENERIC,
            )
        };
        match result {
            0 => {}
            ERROR_CANCELLED => return Err(PromptError::Cancelled),
            _ => return Err(PromptError::Failed),
        }

        // SAFETY: the dialog returned `size` bytes at `data`, owned by
        // `buffer` until it drops below.
        let packed =
            unsafe { std::slice::from_raw_parts(buffer.data.cast::<u8>(), buffer.size as usize) };
        let credentials = unpack(packed, CRED_PACK_PROTECTED_CREDENTIALS);
        drop(buffer);
        credentials
    }

    impl CredentialSource for NativePrompt {
        fn ask(&self, host: &str) -> Result<Credentials, PromptError> {
            prompt(
                "Sign in to Persephone",
                &format!(
                    "Enter your Matrix username and password for {host}. \
                     Only Persephone's core sees the password."
                ),
                None,
            )
        }

        fn ask_secret(&self, request: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
            // Only the password field is used; the username shown is fixed
            // text, and the secret is used only for the signed-in account.
            let (secret, max) = match request {
                SecretPrompt::Password { user_id, host } => (
                    prompt(
                        "Set up encryption in Persephone",
                        &format!(
                            "Enter the password for {user_id} on {host} to set up \
                             encryption for this account. Only Persephone's core sees it."
                        ),
                        Some(user_id),
                    )?
                    .password,
                    MAX_PASSWORD_CHARS,
                ),
                SecretPrompt::RecoveryKey { user_id } => (
                    prompt(
                        "Verify this device with your recovery key",
                        &format!(
                            "Type the recovery key for {user_id} in the password field. \
                             Only Persephone's core sees it."
                        ),
                        Some("Recovery key"),
                    )?
                    .password,
                    MAX_RECOVERY_KEY_CHARS,
                ),
                SecretPrompt::Passphrase { retry } => (
                    prompt(
                        "Unlock Persephone",
                        if retry {
                            "That passphrase was not correct. Type the passphrase for \
                             Persephone on this computer in the password field. Only \
                             Persephone's core sees it."
                        } else {
                            "Type the passphrase for Persephone on this computer in the \
                             password field. Only Persephone's core sees it."
                        },
                        Some("Profile passphrase"),
                    )?
                    .password,
                    MAX_PASSWORD_CHARS,
                ),
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
                        NewPassphrase::TooShort => {
                            format!("That passphrase was too short. {choose}")
                        }
                        NewPassphrase::Mismatch => {
                            format!("The two passphrases were different. {choose}")
                        }
                    };
                    (
                        prompt(
                            "Protect Persephone on this computer",
                            &message,
                            Some("New profile passphrase"),
                        )?
                        .password,
                        MAX_PASSWORD_CHARS,
                    )
                }
            };
            checked_secret(secret, max)
        }

        fn confirm_reset(&self) -> Result<bool, PromptError> {
            let caption = wide("Reset Persephone on this computer?");
            let text = wide(
                "This deletes Persephone's profile on this computer: its messages, \
                 keys and sign-in. It cannot be undone.\n\n\
                 Your account stays on the server. This device stays signed in there \
                 until you remove it from another client. Encrypted history comes back \
                 only from key backup, with your recovery key.\n\n\
                 Delete the profile and choose a new passphrase?",
            );
            // SAFETY: both strings are NUL-terminated and live for the call.
            // "No" is the default button.
            let answer = unsafe {
                MessageBoxW(
                    null_mut(),
                    text.as_ptr(),
                    caption.as_ptr(),
                    MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
                )
            };
            match answer {
                IDYES => Ok(true),
                IDNO => Ok(false),
                _ => Err(PromptError::Failed),
            }
        }

        fn confirm_identity_change(
            &self,
            user_id: &str,
            was_verified: bool,
        ) -> Result<bool, PromptError> {
            // The caller passes a valid Matrix user ID: printable ASCII only,
            // so it cannot reorder or hide the surrounding text.
            if user_id.len() > 255 || !user_id.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(PromptError::Invalid);
            }
            let caption = wide("A contact's identity changed");
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
            let text = wide(&format!(
                "{user_id}\n\n{before}\n\n\
                 This happens when they reset their encryption, for example after \
                 losing their devices. It also happens when someone who controls \
                 the server tries to read your messages.\n\n\
                 Persephone sends nothing in rooms with this person until you \
                 decide. Ask them in person or on a call you trust whether they \
                 reset their encryption.\n\n{after}\n\n\
                 Accept the new identity and send to it?"
            ));
            // SAFETY: both strings are NUL-terminated and live for the call.
            // "No" is the default button.
            let answer = unsafe {
                MessageBoxW(
                    null_mut(),
                    text.as_ptr(),
                    caption.as_ptr(),
                    MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
                )
            };
            match answer {
                IDYES => Ok(true),
                IDNO => Ok(false),
                _ => Err(PromptError::Failed),
            }
        }

        fn show_recovery_key(&self, user_id: &str, key: &str) -> Result<(), PromptError> {
            let caption = wide("Your Persephone recovery key");
            let text = Zeroizing::new(wide(&format!(
                "This is the recovery key for {user_id}. Write it down or save it \
                 somewhere safe now: Persephone does not keep it, and it is shown \
                 only once. You need it to read your encrypted messages on a new \
                 device.\n\n{key}\n\nPress Ctrl+C to copy this message."
            )));
            // SAFETY: both strings are NUL-terminated and live for the call;
            // no owner window, as with the credential dialog.
            let answer = unsafe {
                MessageBoxW(
                    null_mut(),
                    text.as_ptr(),
                    caption.as_ptr(),
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
                )
            };
            if answer == IDOK {
                Ok(())
            } else {
                Err(PromptError::Failed)
            }
        }
    }

    /// Unpacks a credential buffer. With `CRED_PACK_PROTECTED_CREDENTIALS`
    /// the password is decrypted when the dialog returned it protected;
    /// without it a protected password comes back as its encrypted form.
    pub fn unpack(packed: &[u8], flags: u32) -> Result<Credentials, PromptError> {
        let size = u32::try_from(packed.len()).map_err(|_| PromptError::Invalid)?;
        // First call reports the sizes; the second fills zeroed buffers.
        let (mut user_len, mut domain_len, mut password_len) = (0u32, 0u32, 0u32);
        // SAFETY: null output buffers with zero lengths ask only for sizes.
        let sized = unsafe {
            CredUnPackAuthenticationBufferW(
                flags,
                packed.as_ptr().cast(),
                size,
                null_mut(),
                &mut user_len,
                null_mut(),
                &mut domain_len,
                null_mut(),
                &mut password_len,
            )
        };
        // SAFETY: reads this thread's last error.
        if sized != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(PromptError::Failed);
        }
        let limit = 4096;
        if user_len > limit || domain_len > limit || password_len > limit {
            return Err(PromptError::Invalid);
        }
        let mut user = Zeroizing::new(vec![0u16; user_len.max(1) as usize]);
        let mut domain = Zeroizing::new(vec![0u16; domain_len.max(1) as usize]);
        let mut password = Zeroizing::new(vec![0u16; password_len.max(1) as usize]);
        // SAFETY: each buffer holds the length passed with it.
        let unpacked = unsafe {
            CredUnPackAuthenticationBufferW(
                flags,
                packed.as_ptr().cast(),
                size,
                user.as_mut_ptr(),
                &mut user_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                password.as_mut_ptr(),
                &mut password_len,
            )
        };
        if unpacked == 0 {
            return Err(PromptError::Failed);
        }
        from_wide(text(&user, user_len), text(&password, password_len))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::Security::Credentials::{
            CRED_PACK_GENERIC_CREDENTIALS, CredPackAuthenticationBufferW,
        };

        fn pack(user: &str, password: &str, flags: u32) -> Vec<u8> {
            let (user, password) = (wide(user), wide(password));
            let mut size = 0u32;
            // SAFETY: a null output buffer asks for the size.
            unsafe {
                CredPackAuthenticationBufferW(
                    flags,
                    user.as_ptr(),
                    password.as_ptr(),
                    null_mut(),
                    &mut size,
                )
            };
            let mut packed = vec![0u8; size as usize];
            // SAFETY: `packed` holds `size` bytes.
            let ok = unsafe {
                CredPackAuthenticationBufferW(
                    flags,
                    user.as_ptr(),
                    password.as_ptr(),
                    packed.as_mut_ptr(),
                    &mut size,
                )
            };
            assert_ne!(ok, 0, "pack failed");
            packed.truncate(size as usize);
            packed
        }

        const PASSWORD: &str = "Aa1!synthetic-Pässword-0123456789";

        /// The dialog can return the password protected. Unpacking without the
        /// protected flag yields the encrypted form (which a homeserver would
        /// refuse); with it, the password itself.
        #[test]
        fn given_a_protected_password_then_only_the_protected_flag_recovers_it() {
            let packed = pack("p0_alice", PASSWORD, CRED_PACK_PROTECTED_CREDENTIALS);
            let raw = unpack(&packed, 0);
            assert!(
                raw.is_err() || raw.as_ref().is_ok_and(|c| c.password.as_str() != PASSWORD),
                "flags 0 should not yield the plain password"
            );
            let credentials = unpack(&packed, CRED_PACK_PROTECTED_CREDENTIALS).unwrap();
            assert_eq!(credentials.username.as_str(), "p0_alice");
            assert_eq!(credentials.password.as_str(), PASSWORD);
        }

        #[test]
        fn given_an_unprotected_password_then_the_protected_flag_still_returns_it() {
            for flags in [0, CRED_PACK_GENERIC_CREDENTIALS] {
                let packed = pack("p0_alice", PASSWORD, flags);
                let credentials = unpack(&packed, CRED_PACK_PROTECTED_CREDENTIALS).unwrap();
                assert_eq!(
                    credentials.password.as_str(),
                    PASSWORD,
                    "pack flags {flags}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn given_valid_entries_then_they_decode() {
        let credentials = from_wide(&wide("@alice:example.org"), &wide("pässwörd 🔑")).unwrap();
        assert_eq!(credentials.username.as_str(), "@alice:example.org");
        assert_eq!(credentials.password.as_str(), "pässwörd 🔑");
    }

    #[test]
    fn given_empty_oversized_control_or_broken_text_then_it_is_refused() {
        let long_user = "u".repeat(MAX_USERNAME_CHARS + 1);
        let long_password = "p".repeat(MAX_PASSWORD_CHARS + 1);
        for (user, password) in [
            (wide(""), wide("x")),
            (wide("alice"), wide("")),
            (wide(&long_user), wide("x")),
            (wide("alice"), wide(&long_password)),
            (wide("ali\nce"), wide("x")),
            (wide("alice"), wide("pass\u{0}word")),
            // A lone surrogate is not valid UTF-16.
            (vec![0xD800], wide("x")),
        ] {
            assert_eq!(
                from_wide(&user, &password).err(),
                Some(PromptError::Invalid)
            );
        }
        // The limits themselves are accepted.
        let user = "u".repeat(MAX_USERNAME_CHARS);
        let password = "p".repeat(MAX_PASSWORD_CHARS);
        assert!(from_wide(&wide(&user), &wide(&password)).is_ok());
    }

    #[test]
    fn given_no_native_prompt_then_asking_is_unsupported() {
        assert_eq!(
            Unsupported.ask("matrix.example.org").err(),
            Some(PromptError::Unsupported)
        );
        // Secrets and the recovery-key window fail closed too.
        assert_eq!(
            Unsupported
                .ask_secret(SecretPrompt::RecoveryKey { user_id: "@a:s" })
                .err(),
            Some(PromptError::Unsupported)
        );
        assert_eq!(
            Unsupported.show_recovery_key("@a:s", "key"),
            Err(PromptError::Unsupported)
        );
    }

    /// GIVEN a secret WHEN checked THEN an empty, oversized or control-bearing
    /// one is refused; spaces (a recovery key's groups) are kept.
    #[test]
    fn given_secrets_then_only_bounded_printable_text_is_accepted() {
        let check = |text: &str| checked_secret(Zeroizing::new(text.to_owned()), 8);
        assert_eq!(check("EsT1 abc").unwrap().as_str(), "EsT1 abc");
        for bad in ["", "123456789", "ab\ncd", "ab\u{0}c"] {
            assert_eq!(check(bad).err(), Some(PromptError::Invalid), "{bad:?}");
        }
    }

    /// GIVEN the development source WHEN its recovery-key file is outside a
    /// `.local` directory, or unset THEN the key is neither written nor read.
    #[cfg(feature = "dev-disposable-servers")]
    #[test]
    fn given_a_recovery_file_outside_local_then_the_dev_source_refuses_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let outside = dev::HarnessProfile::new(dir.path().join("profile.json"))
            .with_recovery_file(dir.path().join("key.txt"));
        assert_eq!(
            outside.show_recovery_key("@a:s", "key"),
            Err(PromptError::Unsupported)
        );
        assert!(!dir.path().join("key.txt").exists());
        let unset = dev::HarnessProfile::new(dir.path().join("profile.json"));
        assert_eq!(
            unset
                .ask_secret(SecretPrompt::RecoveryKey { user_id: "@a:s" })
                .err(),
            Some(PromptError::Unsupported)
        );
        let local = dir.path().join(".local");
        std::fs::create_dir(&local).unwrap();
        let inside = dev::HarnessProfile::new(dir.path().join("profile.json"))
            .with_recovery_file(local.join("key.txt"));
        // On Unix an existing, wider file is narrowed to its owner first.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(local.join("key.txt"), "old").unwrap();
            std::fs::set_permissions(
                local.join("key.txt"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        inside.show_recovery_key("@a:s", "EsTs abcd efgh").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(local.join("key.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "owner only");
        }
        assert_eq!(
            inside
                .ask_secret(SecretPrompt::RecoveryKey { user_id: "@a:s" })
                .unwrap()
                .as_str(),
            "EsTs abcd efgh"
        );
        // `..` out of `.local` is followed before the check, so it is refused.
        let escaped = dev::HarnessProfile::new(dir.path().join("profile.json"))
            .with_recovery_file(local.join("..").join("escaped.txt"));
        assert_eq!(
            escaped.show_recovery_key("@a:s", "key"),
            Err(PromptError::Unsupported)
        );
        assert!(!dir.path().join("escaped.txt").exists());
        // A directory where the file should be is not a key file.
        std::fs::create_dir(local.join("dir")).unwrap();
        let directory = dev::HarnessProfile::new(dir.path().join("profile.json"))
            .with_recovery_file(local.join("dir"));
        assert_eq!(
            directory
                .ask_secret(SecretPrompt::RecoveryKey { user_id: "@a:s" })
                .err(),
            Some(PromptError::Unsupported)
        );
    }

    /// GIVEN a development answer file WHEN a contact's new identity is to be
    /// accepted THEN only `yes` accepts, the file is read each time, and a
    /// file outside `.local` or missing answers nothing.
    #[cfg(feature = "dev-disposable-servers")]
    #[test]
    fn given_an_identity_answer_file_then_only_yes_accepts() {
        let dir = tempfile::TempDir::new().unwrap();
        let local = dir.path().join(".local");
        std::fs::create_dir(&local).unwrap();
        let answer = local.join("answer.txt");
        let source = dev::HarnessProfile::new(dir.path().join("profile.json"))
            .with_identity_answer_file(answer.clone());
        assert_eq!(
            source.confirm_identity_change("@b:s", false),
            Err(PromptError::Failed),
            "missing file"
        );
        for (text, accepted) in [
            ("yes\n", true),
            ("no", false),
            ("YES", false),
            ("yes!", false),
        ] {
            std::fs::write(&answer, text).unwrap();
            assert_eq!(
                source.confirm_identity_change("@b:s", true),
                Ok(accepted),
                "{text}"
            );
        }
        let outside = dev::HarnessProfile::new(dir.path().join("profile.json"))
            .with_identity_answer_file(dir.path().join("answer.txt"));
        std::fs::write(dir.path().join("answer.txt"), "yes").unwrap();
        assert_eq!(
            outside.confirm_identity_change("@b:s", false),
            Err(PromptError::Unsupported)
        );
    }

    /// GIVEN a development profile WHEN its password is empty, oversized or
    /// holds a control character THEN it is refused as the native prompt
    /// refuses it, before anything is sent.
    #[cfg(feature = "dev-disposable-servers")]
    #[test]
    fn given_a_bad_profile_password_then_the_dev_source_refuses_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let profile = dir.path().join("profile.json");
        let long = "p".repeat(MAX_PASSWORD_CHARS + 1);
        for bad in ["", "a\u{1}b", long.as_str()] {
            let entries = serde_json::json!([
                {"base_url": "http://h.test:8008", "username": "u", "password": bad}
            ]);
            std::fs::write(&profile, entries.to_string()).unwrap();
            let source = dev::HarnessProfile::new(profile.clone());
            assert_eq!(
                source
                    .ask_secret(SecretPrompt::Password {
                        user_id: "@u:h.test",
                        host: "h.test:8008"
                    })
                    .err(),
                Some(PromptError::Invalid),
                "{:?}",
                bad.len()
            );
            assert_eq!(source.ask("h.test:8008").err(), Some(PromptError::Invalid));
        }
    }
}
