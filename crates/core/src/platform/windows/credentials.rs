//! The Windows credential dialogs: CredUI for the sign-in password and
//! secrets, and message boxes for the recovery key, the reset warning and
//! identity changes. Shared limits and checks are in [`crate::credentials`],
//! and the words in [`crate::prompt_text`].

use crate::credentials::{
    CredentialSource, Credentials, PromptError, SecretPrompt, checked_secret, from_wide,
};
use crate::prompt_text;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, ERROR_INSUFFICIENT_BUFFER, GetLastError},
    Security::Credentials::{
        CRED_PACK_GENERIC_CREDENTIALS, CRED_PACK_PROTECTED_CREDENTIALS, CREDUI_INFOW,
        CREDUIWIN_GENERIC, CredPackAuthenticationBufferW, CredUIPromptForWindowsCredentialsW,
        CredUnPackAuthenticationBufferW,
    },
    System::Com::CoTaskMemFree,
    UI::Shell::ShellExecuteW,
    UI::WindowsAndMessaging::{
        IDNO, IDOK, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
        MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW, SW_SHOWNORMAL,
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
fn prompt(caption: &str, message: &str, user: Option<&str>) -> Result<Credentials, PromptError> {
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
        let (caption, message) = prompt_text::sign_in(host);
        prompt(caption, &message, None)
    }

    fn ask_secret(&self, request: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        // Only the password field is used; the username shown is fixed
        // text, and the secret is used only for the signed-in account.
        let text = prompt_text::secret(&request);
        let secret = prompt(text.caption, &text.message, text.field)?.password;
        checked_secret(secret, text.max_chars)
    }

    fn confirm_reset(&self) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::RESET;
        yes_or_no(caption, message)
    }

    fn confirm_identity_change(
        &self,
        user_id: &str,
        was_verified: bool,
    ) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::identity_change(user_id, was_verified)?;
        yes_or_no(caption, &message)
    }

    fn confirm_recovery_key_change(&self) -> Result<bool, PromptError> {
        let (caption, message) = prompt_text::RECOVERY_KEY_CHANGE;
        yes_or_no(caption, message)
    }

    fn confirm_open_link(&self, caption: &str, message: &str) -> Result<bool, PromptError> {
        yes_or_no(caption, message)
    }

    fn open_link(&self, url: &str) -> Result<(), PromptError> {
        let operation = wide("open");
        let file = wide(url);
        // SAFETY: both strings are NUL-terminated and live for the call; no
        // owner window, parameters or directory. The URL was checked to be an
        // http or https link (`links::checked`), so the shell hands it to the
        // default browser.
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecuteW reports success as a value above 32.
        if result as usize > 32 {
            Ok(())
        } else {
            Err(PromptError::Failed)
        }
    }

    fn choose_recovery_key_file(&self) -> Result<Option<PathBuf>, PromptError> {
        let (title, suggested) = prompt_text::SAVE_RECOVERY_KEY;
        super::pick::save_dialog(title, suggested).map_err(|_| PromptError::Failed)
    }

    fn show_recovery_key(&self, user_id: &str, key: &str) -> Result<(), PromptError> {
        let (caption, message) =
            prompt_text::recovery_key(user_id, key, Some("Press Ctrl+C to copy this message."));
        let caption = wide(caption);
        let text = Zeroizing::new(wide(&message));
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

/// A warning with Yes and No, where "No" is the default button.
fn yes_or_no(caption: &str, message: &str) -> Result<bool, PromptError> {
    let caption = wide(caption);
    let text = wide(message);
    // SAFETY: both strings are NUL-terminated and live for the call.
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
