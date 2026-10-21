//! Sign-in credentials, asked for by the core itself.
//!
//! The password never passes through the UI process or the web view: the core
//! shows the operating system's own credential dialog, naming the homeserver
//! host so the user can see where the password goes even if the page was
//! compromised. Buffers that held the password are zeroed. The SDK copies the
//! password into its login request; that copy is outside this guarantee.

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

/// Asks the user for a username and password for `host`.
pub trait CredentialSource: Send + Sync {
    fn ask(&self, host: &str) -> Result<Credentials, PromptError>;
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
    use super::{CredentialSource, Credentials, PromptError};
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    const MAX_PROFILE_BYTES: u64 = 64 * 1024;

    #[derive(serde::Deserialize)]
    struct Entry {
        base_url: String,
        username: String,
        password: String,
    }

    pub struct HarnessProfile {
        path: PathBuf,
    }

    impl HarnessProfile {
        pub fn new(path: PathBuf) -> Self {
            Self { path }
        }
    }

    impl CredentialSource for HarnessProfile {
        fn ask(&self, host: &str) -> Result<Credentials, PromptError> {
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
            Ok(Credentials {
                username: Zeroizing::new(entry.username),
                password: Zeroizing::new(entry.password),
            })
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{CredentialSource, Credentials, PromptError, from_wide};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::{ERROR_CANCELLED, ERROR_INSUFFICIENT_BUFFER, GetLastError},
        Security::Credentials::{
            CRED_PACK_PROTECTED_CREDENTIALS, CREDUI_INFOW, CREDUIWIN_GENERIC,
            CredUIPromptForWindowsCredentialsW, CredUnPackAuthenticationBufferW,
        },
        System::Com::CoTaskMemFree,
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

    impl CredentialSource for NativePrompt {
        fn ask(&self, host: &str) -> Result<Credentials, PromptError> {
            let caption = wide("Sign in to Persephone");
            let message = wide(&format!(
                "Enter your Matrix username and password for {host}. \
                 Only Persephone's core sees the password."
            ));
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
            // SAFETY: every pointer is valid for the call; the dialog fills
            // `buffer`, which `AuthBuffer` then owns. No save checkbox.
            let result = unsafe {
                CredUIPromptForWindowsCredentialsW(
                    &info,
                    0,
                    &mut package,
                    null(),
                    0,
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
            let packed = unsafe {
                std::slice::from_raw_parts(buffer.data.cast::<u8>(), buffer.size as usize)
            };
            let credentials = unpack(packed, CRED_PACK_PROTECTED_CREDENTIALS);
            drop(buffer);
            credentials
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
    }
}
