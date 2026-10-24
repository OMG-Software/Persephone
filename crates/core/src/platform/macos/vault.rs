//! The macOS vault: the wrapping secret is a Keychain generic password
//! (ADR-0011), used through [`unix::Wrapped`](super::super::unix::Wrapped).
//!
//! The item is in the user's default keychain (the login keychain), which
//! is unlocked while the user is logged in. It is accessible only while the
//! device is unlocked and only on this device, and it is not synchronized.
//! The Keychain's access list trusts the program that created the item;
//! another program, or a build with a different signature, gets a Keychain
//! prompt, and a refusal leaves the profile locked.
//!
//! Only the file-based keychain is used. The data protection keychain needs
//! a signed keychain-access-groups entitlement, which unsigned and
//! development builds lack; using it in some builds only would hide the item
//! from the others.
//!
//! The secret passes through Core Foundation buffers on its way to and from
//! the Keychain; those copies are outside the zeroing guarantee.

use super::super::unix::{SECRET_BYTES, SecretStore};
use crate::vault::VaultError;
use core_foundation::{
    array::CFArray,
    base::{CFGetTypeID, CFType, CFTypeRef, OSStatus, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::CFDictionary,
    string::{CFString, CFStringRef},
};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework_sys::{
    access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
    base::{errSecDuplicateItem, errSecItemNotFound, errSecSuccess},
    item::{
        kSecAttrAccount, kSecAttrService, kSecAttrSynchronizable, kSecClass,
        kSecClassGenericPassword, kSecMatchLimit, kSecMatchSearchList, kSecReturnData,
        kSecUseKeychain, kSecValueData,
    },
    keychain_item::{SecItemAdd, SecItemCopyMatching},
};
use std::ptr;
use zeroize::Zeroizing;

/// The item's service: the application's reverse-DNS identifier.
pub const SERVICE: &str = "io.github.omg-software.persephone";
/// The item's account: what it holds, with a version.
pub const ACCOUNT: &str = "store-key-wrap-v1";

/// Keychain errors that mean there is no keychain to use.
const NOT_AVAILABLE: OSStatus = -25291; // errSecNotAvailable
const NO_SUCH_KEYCHAIN: OSStatus = -25294; // errSecNoSuchKeychain
const NO_DEFAULT_KEYCHAIN: OSStatus = -25307; // errSecNoDefaultKeychain

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrAccessible: CFStringRef;
    static kSecMatchLimitOne: CFStringRef;
}

/// The vault error for a Keychain status other than success, "not found"
/// and "duplicate": no keychain is `Unsupported`; everything else, including
/// a locked keychain where interaction is not allowed, a failed
/// authentication and a cancelled prompt, is `Failed`.
pub fn vault_error(status: OSStatus) -> VaultError {
    match status {
        NOT_AVAILABLE | NO_SUCH_KEYCHAIN | NO_DEFAULT_KEYCHAIN => VaultError::Unsupported,
        _ => VaultError::Failed,
    }
}

/// The wrapping secret's Keychain item.
pub struct KeychainStore {
    /// `None`: the user's default keychain, looked up on each use.
    keychain: Option<SecKeychain>,
}

impl KeychainStore {
    /// The item in the user's default keychain.
    pub fn default_keychain() -> Self {
        Self { keychain: None }
    }

    /// The item in `keychain` (tests use a temporary one).
    pub fn in_keychain(keychain: SecKeychain) -> Self {
        Self {
            keychain: Some(keychain),
        }
    }

    fn keychain(&self) -> Result<SecKeychain, VaultError> {
        match &self.keychain {
            Some(keychain) => Ok(keychain.clone()),
            None => SecKeychain::default().map_err(|error| vault_error(error.code())),
        }
    }
}

fn key(constant: CFStringRef) -> CFString {
    // SAFETY: `constant` is one of Security's static key strings.
    unsafe { CFString::wrap_under_get_rule(constant) }
}

/// The attributes that name the item: a generic password with this
/// service and account, never synchronized.
fn item() -> Vec<(CFString, CFType)> {
    // SAFETY: each is one of Security's static strings.
    unsafe {
        vec![
            (key(kSecClass), key(kSecClassGenericPassword).into_CFType()),
            (key(kSecAttrService), CFString::new(SERVICE).into_CFType()),
            (key(kSecAttrAccount), CFString::new(ACCOUNT).into_CFType()),
            (
                key(kSecAttrSynchronizable),
                CFBoolean::false_value().into_CFType(),
            ),
        ]
    }
}

impl SecretStore for KeychainStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; SECRET_BYTES]>>, VaultError> {
        let keychain = self.keychain()?;
        let mut query = item();
        // SAFETY: Security's static strings.
        unsafe {
            // Only this keychain, not the whole search list.
            query.push((
                key(kSecMatchSearchList),
                CFArray::from_CFTypes(&[keychain]).into_CFType(),
            ));
            query.push((key(kSecMatchLimit), key(kSecMatchLimitOne).into_CFType()));
            query.push((key(kSecReturnData), CFBoolean::true_value().into_CFType()));
        }
        let query = CFDictionary::from_CFType_pairs(&query);
        let mut found: CFTypeRef = ptr::null();
        // SAFETY: a valid query; `found` receives an owned reference.
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut found) };
        match status {
            errSecSuccess => {}
            errSecItemNotFound => return Ok(None),
            other => return Err(vault_error(other)),
        }
        // SAFETY: `found` is null or an owned reference from the call above.
        if found.is_null() || unsafe { CFGetTypeID(found) } != CFData::type_id() {
            if !found.is_null() {
                // SAFETY: drops the owned reference of the wrong type.
                drop(unsafe { CFType::wrap_under_create_rule(found) });
            }
            return Err(VaultError::Failed);
        }
        // SAFETY: an owned CFData reference.
        let data = unsafe { CFData::wrap_under_create_rule(found.cast()) };
        let bytes = data.bytes();
        if bytes.len() != SECRET_BYTES {
            return Err(VaultError::Failed);
        }
        let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
        secret.copy_from_slice(bytes);
        Ok(Some(secret))
    }

    fn store_new(
        &self,
        secret: &[u8; SECRET_BYTES],
    ) -> Result<Zeroizing<[u8; SECRET_BYTES]>, VaultError> {
        let keychain = self.keychain()?;
        let mut attributes = item();
        // SAFETY: Security's static strings.
        unsafe {
            attributes.push((
                key(kSecAttrAccessible),
                key(kSecAttrAccessibleWhenUnlockedThisDeviceOnly).into_CFType(),
            ));
            attributes.push((key(kSecUseKeychain), keychain.as_CFType()));
            attributes.push((
                key(kSecValueData),
                CFData::from_buffer(secret).into_CFType(),
            ));
        }
        let attributes = CFDictionary::from_CFType_pairs(&attributes);
        // SAFETY: valid attributes; no result is asked for.
        let status = unsafe { SecItemAdd(attributes.as_concrete_TypeRef(), ptr::null_mut()) };
        match status {
            errSecSuccess => Ok(Zeroizing::new(*secret)),
            // Another process stored one first: use that one.
            errSecDuplicateItem => self.load()?.ok_or(VaultError::Failed),
            other => Err(vault_error(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::unix::Wrapped;
    use crate::vault::KeyProtector;
    use security_framework::os::macos::keychain::CreateOptions;
    use security_framework_sys::base::SecKeychainRef;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainLock(keychain: SecKeychainRef) -> OSStatus;
        fn SecKeychainDelete(keychain: SecKeychainRef) -> OSStatus;
    }

    /// A keychain file in a temporary folder, deleted (and removed from the
    /// search list) on drop. Tests never touch the runner's login keychain.
    struct TemporaryKeychain {
        keychain: SecKeychain,
        password: String,
        _dir: tempfile::TempDir,
    }

    impl TemporaryKeychain {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).unwrap();
            let password: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let keychain = CreateOptions::new()
                .password(&password)
                .create(dir.path().join("persephone-test.keychain"))
                .expect("create a temporary keychain");
            Self {
                keychain,
                password,
                _dir: dir,
            }
        }
    }

    impl Drop for TemporaryKeychain {
        fn drop(&mut self) {
            // SAFETY: a keychain this test created.
            unsafe { SecKeychainDelete(self.keychain.as_concrete_TypeRef()) };
        }
    }

    /// GIVEN Keychain statuses THEN "no keychain" is unsupported and every
    /// refusal (locked without interaction, failed authentication, a
    /// cancelled prompt, anything unknown) fails.
    #[test]
    fn given_keychain_statuses_then_each_maps_to_its_vault_error() {
        for status in [NOT_AVAILABLE, NO_SUCH_KEYCHAIN, NO_DEFAULT_KEYCHAIN] {
            assert_eq!(vault_error(status), VaultError::Unsupported, "{status}");
        }
        // errSecInteractionNotAllowed, errSecAuthFailed, errSecUserCanceled,
        // errSecMissingEntitlement, errSecParam.
        for status in [-25308, -25293, -128, -34018, -50] {
            assert_eq!(vault_error(status), VaultError::Failed, "{status}");
        }
    }

    /// One test, so the process-wide "no user interaction" setting and the
    /// keychain lock do not race other Keychain tests.
    ///
    /// GIVEN an empty temporary keychain WHEN the secret is stored, stored
    /// again and read THEN the first secret stays; the wrap opens with it;
    /// and once the keychain is locked, with no prompt allowed, both reading
    /// and opening fail instead of making a new secret.
    #[test]
    fn given_a_temporary_keychain_then_the_secret_is_stored_once_and_locks_fail_closed() {
        let mut temporary = TemporaryKeychain::new();
        let store = KeychainStore::in_keychain(temporary.keychain.clone());
        assert_eq!(store.load(), Ok(None), "empty keychain");

        let first = [7u8; SECRET_BYTES];
        assert_eq!(*store.store_new(&first).unwrap(), first);
        assert_eq!(store.load().unwrap().as_deref(), Some(&first));
        let second = [9u8; SECRET_BYTES];
        assert_eq!(
            *store.store_new(&second).unwrap(),
            first,
            "an existing secret is kept"
        );

        let vault = Wrapped(KeychainStore::in_keychain(temporary.keychain.clone()));
        let blob = vault.protect(b"sealed store key").unwrap();
        assert_eq!(
            vault.unprotect(&blob).unwrap().as_slice(),
            b"sealed store key"
        );

        let other = TemporaryKeychain::new();
        let elsewhere = Wrapped(KeychainStore::in_keychain(other.keychain.clone()));
        assert!(
            matches!(elsewhere.unprotect(&blob), Err(VaultError::Failed)),
            "another keychain has no secret, and none is made"
        );
        assert_eq!(
            KeychainStore::in_keychain(other.keychain.clone()).load(),
            Ok(None)
        );

        let quiet = SecKeychain::disable_user_interaction().unwrap();
        // SAFETY: a keychain this test created.
        assert_eq!(
            unsafe { SecKeychainLock(temporary.keychain.as_concrete_TypeRef()) },
            errSecSuccess
        );
        assert_eq!(store.load(), Err(VaultError::Failed), "locked");
        assert!(matches!(vault.unprotect(&blob), Err(VaultError::Failed)));
        assert!(matches!(vault.protect(b"k"), Err(VaultError::Failed)));
        drop(quiet);

        temporary
            .keychain
            .unlock(Some(&temporary.password))
            .unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&first), "unlocked");
    }
}
