//! Code that Linux and macOS share: reading a picked image file, saving a
//! file without widening who can read it, and the wrapping layer that the
//! Secret Service and Keychain vaults put around the store key.

use crate::media::{Encoded, MAX_ENCODED_BYTES};
use crate::vault::{KeyProtector, VaultError};
use chacha20poly1305::{AeadInOut, Key, KeyInit, XChaCha20Poly1305, XNonce, aead::Tag};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::ResourceBudgets;
use std::{
    ffi::OsString,
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

/// The path in an environment value, only when it is absolute. A missing,
/// empty or relative value gives `None`, so a default profile never resolves
/// against the working directory.
pub fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// Reads a picked image file once, without writing it. The last path
/// component must not be a symbolic link (`O_NOFOLLOW`), the open file must
/// be a regular file (`fstat`), and its size must be 1 byte to
/// [`MAX_ENCODED_BYTES`]. `O_NONBLOCK` keeps a FIFO from blocking the open; it
/// is then refused as not a regular file. Unix has no share mode, so another
/// process may write the file meanwhile; a read that does not end at exactly
/// the size checked first is refused.
pub fn read_image_file(path: &Path, budgets: &ResourceBudgets) -> Result<Encoded, ErrorCode> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            _ => ErrorCode::Rejected,
        })?;
    // From the open descriptor, so it describes what is read.
    let metadata = file.metadata().map_err(|_| ErrorCode::Rejected)?;
    if !metadata.file_type().is_file() {
        return Err(ErrorCode::Rejected);
    }
    let size = metadata.len();
    if size == 0 || size > MAX_ENCODED_BYTES {
        return Err(ErrorCode::Rejected);
    }
    let mut encoded = Encoded::reserve(budgets, size)?;
    encoded.fill_exactly(&mut file)?;
    Ok(encoded)
}

/// Creates `path`, which must not exist, for writing, readable and writable
/// by its owner only (ADR-0029), so the folder's defaults never expose the
/// bytes while they are written.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Puts the written `partial` file at `path`. A regular file already there
/// passes its permissions on; with none there (or a link, which is replaced
/// itself), the file keeps its owner-only mode.
pub fn replace_file(partial: &Path, path: &Path) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::symlink_metadata(path)
        && existing.is_file()
    {
        std::fs::set_permissions(partial, existing.permissions())?;
    }
    std::fs::rename(partial, path)
}

/// Bytes of a wrapping secret.
pub const SECRET_BYTES: usize = 32;

/// Where a platform keeps the wrapping secret: one per OS user, like DPAPI's
/// per-user key on Windows.
pub trait SecretStore: Send + Sync {
    /// The stored secret, or `None` if there is none yet. An unreachable or
    /// locked store is an error, never `None`.
    fn load(&self) -> Result<Option<Zeroizing<[u8; SECRET_BYTES]>>, VaultError>;
    /// Stores `secret` if no secret exists yet, and returns the secret that
    /// is stored afterwards (another process may have stored one first).
    fn store_new(
        &self,
        secret: &[u8; SECRET_BYTES],
    ) -> Result<Zeroizing<[u8; SECRET_BYTES]>, VaultError>;
}

const WRAP_VERSION: u8 = 1;
const WRAP_CONTEXT: &[u8] = b"Persephone local store key wrap v1";
const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;
/// Longest data the wrap accepts: the passphrase-sealed key is far shorter.
const MAX_WRAPPED_DATA: usize = 1024;

/// A [`KeyProtector`] that encrypts with XChaCha20-Poly1305 under a secret
/// kept in the platform's credential store. Layout: version byte, 24-byte
/// nonce, ciphertext, 16-byte tag; the version byte and a fixed context are
/// the associated data.
pub struct Wrapped<S>(pub S);

impl<S: SecretStore> Wrapped<S> {
    fn cipher(secret: &[u8; SECRET_BYTES]) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::cast_from_core(secret))
    }

    fn associated(version: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(1 + WRAP_CONTEXT.len());
        data.push(version);
        data.extend_from_slice(WRAP_CONTEXT);
        data
    }
}

impl<S: SecretStore> KeyProtector for Wrapped<S> {
    fn protect(&self, data: &[u8]) -> Result<Vec<u8>, VaultError> {
        if data.len() > MAX_WRAPPED_DATA {
            return Err(VaultError::Failed);
        }
        let secret = match self.0.load()? {
            Some(secret) => secret,
            None => {
                let mut fresh = Zeroizing::new([0u8; SECRET_BYTES]);
                getrandom::fill(fresh.as_mut()).map_err(|_| VaultError::Failed)?;
                self.0.store_new(&fresh)?
            }
        };
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|_| VaultError::Failed)?;
        let mut body = data.to_vec();
        let tag = Self::cipher(&secret)
            .encrypt_inout_detached(
                &XNonce::from(nonce),
                &Self::associated(WRAP_VERSION),
                body.as_mut_slice().into(),
            )
            .map_err(|_| VaultError::Failed)?;
        let mut blob = Vec::with_capacity(1 + NONCE_BYTES + body.len() + TAG_BYTES);
        blob.push(WRAP_VERSION);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&body);
        blob.extend_from_slice(&tag);
        Ok(blob)
    }

    fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let minimum = 1 + NONCE_BYTES + TAG_BYTES;
        if blob.len() < minimum
            || blob.len() > minimum + MAX_WRAPPED_DATA
            || blob[0] != WRAP_VERSION
        {
            return Err(VaultError::Failed);
        }
        // A missing secret cannot open the blob: the key is unreadable, and
        // no new secret is made here.
        let secret = self.0.load()?.ok_or(VaultError::Failed)?;
        let nonce: [u8; NONCE_BYTES] = blob[1..1 + NONCE_BYTES]
            .try_into()
            .map_err(|_| VaultError::Failed)?;
        let tag_start = blob.len() - TAG_BYTES;
        let tag = Tag::<XChaCha20Poly1305>::try_from(&blob[tag_start..])
            .map_err(|_| VaultError::Failed)?;
        let mut body = Zeroizing::new(blob[1 + NONCE_BYTES..tag_start].to_vec());
        Self::cipher(&secret)
            .decrypt_inout_detached(
                &XNonce::from(nonce),
                &Self::associated(blob[0]),
                body.as_mut_slice().into(),
                &tag,
            )
            .map_err(|_| VaultError::Failed)?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// GIVEN environment values WHEN read as a base directory THEN only an
    /// absolute path is used.
    #[test]
    fn given_an_environment_value_then_only_an_absolute_path_is_used() {
        assert_eq!(
            absolute(Some("/home/a".into())),
            Some(PathBuf::from("/home/a"))
        );
        for value in [None, Some(""), Some("home/a"), Some("./a"), Some("~/a")] {
            assert_eq!(absolute(value.map(OsString::from)), None, "{value:?}");
        }
    }

    /// A store in memory; `fail` makes every call fail, like a locked or
    /// unreachable keyring.
    #[derive(Default)]
    struct Memory {
        secret: Mutex<Option<[u8; SECRET_BYTES]>>,
        fail: bool,
    }

    impl SecretStore for Memory {
        fn load(&self) -> Result<Option<Zeroizing<[u8; SECRET_BYTES]>>, VaultError> {
            if self.fail {
                return Err(VaultError::Unsupported);
            }
            Ok(self.secret.lock().unwrap().map(Zeroizing::new))
        }

        fn store_new(
            &self,
            secret: &[u8; SECRET_BYTES],
        ) -> Result<Zeroizing<[u8; SECRET_BYTES]>, VaultError> {
            let mut stored = self.secret.lock().unwrap();
            Ok(Zeroizing::new(*stored.get_or_insert(*secret)))
        }
    }

    /// GIVEN an empty store WHEN data is protected THEN a secret is created
    /// and the blob opens again, but not after any byte changes.
    #[test]
    fn given_an_empty_store_then_protect_creates_a_secret_and_the_blob_opens() {
        let vault = Wrapped(Memory::default());
        let blob = vault.protect(b"sealed key").unwrap();
        assert!(vault.0.secret.lock().unwrap().is_some());
        assert_eq!(vault.unprotect(&blob).unwrap().as_slice(), b"sealed key");
        for index in 0..blob.len() {
            let mut changed = blob.clone();
            changed[index] ^= 1;
            assert!(vault.unprotect(&changed).is_err(), "byte {index}");
        }
    }

    /// GIVEN a blob WHEN the secret is gone or different THEN it does not
    /// open, and no new secret is made by opening.
    #[test]
    fn given_a_lost_or_other_secret_then_the_blob_does_not_open() {
        let vault = Wrapped(Memory::default());
        let blob = vault.protect(b"sealed key").unwrap();
        let lost = Wrapped(Memory::default());
        assert!(matches!(lost.unprotect(&blob), Err(VaultError::Failed)));
        assert!(lost.0.secret.lock().unwrap().is_none());
        let other = Wrapped(Memory::default());
        other.protect(b"x").unwrap();
        assert!(other.unprotect(&blob).is_err());
    }

    /// GIVEN an unreachable store WHEN protecting or opening THEN its error
    /// is returned, never a key.
    #[test]
    fn given_an_unreachable_store_then_both_directions_fail() {
        let vault = Wrapped(Memory {
            fail: true,
            ..Memory::default()
        });
        assert!(matches!(vault.protect(b"k"), Err(VaultError::Unsupported)));
        let blob = Wrapped(Memory::default()).protect(b"k").unwrap();
        assert!(matches!(
            vault.unprotect(&blob),
            Err(VaultError::Unsupported)
        ));
    }

    /// GIVEN truncated, oversized or wrong-version blobs THEN they are
    /// refused before any decryption.
    #[test]
    fn given_malformed_blobs_then_they_are_refused() {
        let vault = Wrapped(Memory::default());
        let blob = vault.protect(b"k").unwrap();
        assert!(vault.unprotect(&blob[..blob.len() - 1]).is_err());
        assert!(vault.unprotect(&[]).is_err());
        let mut version = blob.clone();
        version[0] = 2;
        assert!(vault.unprotect(&version).is_err());
        assert!(vault.protect(&[0u8; MAX_WRAPPED_DATA + 1]).is_err());
        assert!(
            vault
                .unprotect(&vec![
                    1u8;
                    1 + NONCE_BYTES + TAG_BYTES + MAX_WRAPPED_DATA + 1
                ])
                .is_err()
        );
    }

    /// GIVEN files of each kind WHEN read as a picked image THEN only a
    /// non-empty regular file within the cap is read, unchanged.
    #[test]
    fn given_picked_paths_then_only_plain_files_within_the_cap_are_read() {
        let budgets = ResourceBudgets::default();
        let dir = std::env::temp_dir().join(format!("persephone-unix-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.png");
        std::fs::write(&plain, b"\x89PNG data").unwrap();
        let read = read_image_file(&plain, &budgets).unwrap();
        assert_eq!(read.bytes(), b"\x89PNG data");
        assert_eq!(std::fs::read(&plain).unwrap(), b"\x89PNG data");

        let link = dir.join("link.png");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&plain, &link).unwrap();
        assert!(matches!(
            read_image_file(&link, &budgets),
            Err(ErrorCode::Rejected)
        ));

        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(matches!(
            read_image_file(&empty, &budgets),
            Err(ErrorCode::Rejected)
        ));
        assert!(matches!(
            read_image_file(&dir, &budgets),
            Err(ErrorCode::Rejected)
        ));
        assert!(matches!(
            read_image_file(&dir.join("missing.png"), &budgets),
            Err(ErrorCode::NotFound)
        ));

        let large = dir.join("large.png");
        let file = std::fs::File::create(&large).unwrap();
        file.set_len(MAX_ENCODED_BYTES + 1).unwrap();
        assert!(matches!(
            read_image_file(&large, &budgets),
            Err(ErrorCode::Rejected)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// GIVEN a new file WHEN created private THEN only its owner may read or
    /// write it; GIVEN a file already there WHEN replaced THEN it keeps its
    /// mode, and a new file keeps the owner-only one.
    #[test]
    fn given_a_saved_file_then_its_mode_is_private_or_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let partial = dir.path().join("partial");
        std::io::Write::write_all(&mut create_private_file(&partial).unwrap(), b"new").unwrap();
        assert_eq!(mode(&partial), 0o600);
        assert!(create_private_file(&partial).is_err(), "never over a file");

        let existing = dir.path().join("existing.pdf");
        std::fs::write(&existing, b"old").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o640)).unwrap();
        replace_file(&partial, &existing).unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"new");
        assert_eq!(mode(&existing), 0o640);

        let fresh = dir.path().join("fresh.pdf");
        std::io::Write::write_all(&mut create_private_file(&partial).unwrap(), b"fresh").unwrap();
        replace_file(&partial, &fresh).unwrap();
        assert_eq!(mode(&fresh), 0o600);
    }
}
