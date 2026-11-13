//! The Linux vault: the wrapping secret is a Secret Service item (ADR-0010),
//! used through [`unix::Wrapped`](super::super::unix::Wrapped).
//!
//! The item is in the user's default collection (the login keyring for
//! GNOME Keyring, the default wallet for KWallet). Its attributes name it
//! ([`attributes`]); its value is the secret as 64 lowercase hexadecimal
//! characters, so a provider that keeps values as text keeps it unchanged.
//!
//! The secret travels in a `plain` session: unencrypted on the session bus.
//! The session bus is a Unix socket that only the same user can reach, and
//! that user can already read the item from an unlocked collection, so the
//! Diffie-Hellman session would add no protection against anyone who can
//! read the bus (ADR-0010).
//!
//! Without a session bus, a Secret Service provider or a default
//! collection, the vault is [`VaultError::Unsupported`]; a locked collection
//! whose unlock prompt the user dismisses, or does not answer within
//! [`PROMPT_TIMEOUT`], and every other refusal is [`VaultError::Failed`].
//! Either way the profile stays locked: there is no kernel keyring, file or
//! passphrase-only fallback.
//!
//! The secret is copied into zbus's message buffers on its way to and from
//! the provider; those copies are outside the zeroing guarantee.

use super::super::unix::{SECRET_BYTES, SecretStore};
use super::bus::{self, BusError};
use crate::vault::VaultError;
use std::{collections::HashMap, sync::mpsc, thread, time::Duration};
use zbus::{
    blocking::{Connection, Proxy, proxy::Builder as ProxyBuilder},
    message::Message,
    proxy::CacheProperties,
    zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value},
};
use zeroize::Zeroizing;

/// The item's `service` attribute: the application's reverse-DNS
/// identifier, as on macOS.
pub const SERVICE: &str = "io.github.omg-software.persephone";
/// The item's `account` attribute: what it holds, with a version.
pub const ACCOUNT: &str = "store-key-wrap-v1";
/// The `xdg:schema` attribute that libsecret-based tools show.
pub const SCHEMA: &str = "io.github.omg-software.persephone.StoreKeyWrap";
/// The label a keyring manager shows for the item.
pub const LABEL: &str = "Persephone local store key wrap";
/// The item's content type: the value is ASCII text.
pub const CONTENT_TYPE: &str = "text/plain";
/// Bytes of the item's value: two hexadecimal digits for each secret byte.
pub const ENCODED_BYTES: usize = 2 * SECRET_BYTES;
/// Longest wait for the user to answer the provider's unlock prompt. The
/// prompt is then dismissed and the profile stays locked.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Most matching items accepted in one search; more is refused.
const MAX_ITEMS: usize = 64;

const SECRETS: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const SESSION_INTERFACE: &str = "org.freedesktop.Secret.Session";
const PROMPT_INTERFACE: &str = "org.freedesktop.Secret.Prompt";
/// The object path that means "none" (no prompt, no collection).
const NONE: &str = "/";

/// The attributes that name the item, for both search and creation.
pub fn attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("xdg:schema", SCHEMA),
        ("service", SERVICE),
        ("account", ACCOUNT),
    ])
}

/// The item's value for `secret`: lowercase hexadecimal, in zeroed-on-drop
/// memory.
pub fn encode(secret: &[u8; SECRET_BYTES]) -> Zeroizing<Vec<u8>> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = Zeroizing::new(Vec::with_capacity(ENCODED_BYTES));
    for byte in secret {
        text.push(DIGITS[usize::from(byte >> 4)]);
        text.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    text
}

/// The secret in an item's value: exactly [`ENCODED_BYTES`] lowercase
/// hexadecimal digits, nothing else (no whitespace, no upper case).
pub fn decode(text: &[u8]) -> Result<Zeroizing<[u8; SECRET_BYTES]>, VaultError> {
    if text.len() != ENCODED_BYTES {
        return Err(VaultError::Failed);
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(VaultError::Failed),
    };
    let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
    let (pairs, _) = text.as_chunks::<2>();
    for (slot, [high, low]) in secret.iter_mut().zip(pairs) {
        *slot = (digit(*high)? << 4) | digit(*low)?;
    }
    Ok(secret)
}

/// The vault error for a bus error: no bus or no provider is
/// `Unsupported`, anything else `Failed`.
pub fn vault_error(error: BusError) -> VaultError {
    match error {
        BusError::Absent => VaultError::Unsupported,
        BusError::Failed => VaultError::Failed,
    }
}

/// The item every process uses when more than one matches (two cores that
/// stored at the same moment): the smallest object path. Items keep their
/// paths, so every reader picks the same one.
pub fn canonical(items: &[OwnedObjectPath]) -> Option<&OwnedObjectPath> {
    items.iter().min_by(|a, b| a.as_str().cmp(b.as_str()))
}

/// The wrapping secret's Secret Service item, reached over the session
/// bus on each use.
pub struct SecretService {
    /// `None`: the user's session bus. Tests name another address.
    address: Option<String>,
}

impl SecretService {
    /// The item on the user's session bus.
    pub fn session_bus() -> Self {
        Self { address: None }
    }

    /// The item behind the bus at `address` (tests).
    pub fn at(address: String) -> Self {
        Self {
            address: Some(address),
        }
    }

    fn connect(&self) -> Result<Connection, VaultError> {
        match &self.address {
            None => bus::session(),
            Some(address) => bus::at(address),
        }
        .map_err(vault_error)
    }
}

impl SecretStore for SecretService {
    fn load(&self) -> Result<Option<Zeroizing<[u8; SECRET_BYTES]>>, VaultError> {
        let connection = self.connect()?;
        let session = Session::open(&connection)?;
        load(&connection, &session)
    }

    fn store_new(
        &self,
        secret: &[u8; SECRET_BYTES],
    ) -> Result<Zeroizing<[u8; SECRET_BYTES]>, VaultError> {
        let connection = self.connect()?;
        let session = Session::open(&connection)?;
        // Another process may have stored one since our caller looked.
        if let Some(stored) = load(&connection, &session)? {
            return Ok(stored);
        }
        let collection = default_collection(&connection)?;
        unlock(&connection, std::slice::from_ref(&collection))?;
        let value = encode(secret);
        let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
        properties.insert("org.freedesktop.Secret.Item.Label", Value::from(LABEL));
        properties.insert(
            "org.freedesktop.Secret.Item.Attributes",
            Value::from(attributes()),
        );
        let reply = call(
            &connection,
            collection.as_str(),
            COLLECTION_INTERFACE,
            "CreateItem",
            &(
                properties,
                (&session.path, &[] as &[u8], value.as_slice(), CONTENT_TYPE),
                // Never replace: a secret another process stored first may
                // already wrap a key.
                false,
            ),
        )?;
        drop(value);
        let (_item, prompt): (OwnedObjectPath, OwnedObjectPath) =
            reply.body().deserialize().map_err(|_| VaultError::Failed)?;
        complete(&connection, &prompt)?;
        // Read back whichever item is canonical now, so racing processes
        // agree on one secret.
        load(&connection, &session)?.ok_or(VaultError::Failed)
    }
}

/// An open `plain` session, closed when dropped.
struct Session<'c> {
    connection: &'c Connection,
    path: OwnedObjectPath,
}

impl<'c> Session<'c> {
    fn open(connection: &'c Connection) -> Result<Self, VaultError> {
        let reply = call(
            connection,
            SERVICE_PATH,
            SERVICE_INTERFACE,
            "OpenSession",
            &("plain", Value::from("")),
        )?;
        let (_output, path): (OwnedValue, OwnedObjectPath) =
            reply.body().deserialize().map_err(|_| VaultError::Failed)?;
        Ok(Self { connection, path })
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // Closing the connection ends the session too; this is tidiness.
        let _ = self.connection.call_method(
            Some(SECRETS),
            self.path.as_str(),
            Some(SESSION_INTERFACE),
            "Close",
            &(),
        );
    }
}

/// One method call to the provider.
fn call<B>(
    connection: &Connection,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<Message, VaultError>
where
    B: serde::Serialize + DynamicType,
{
    connection
        .call_method(Some(SECRETS), path, Some(interface), method, body)
        .map_err(|error| vault_error(bus::classify(&error)))
}

/// The matching items: (unlocked, locked).
fn search(
    connection: &Connection,
) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>), VaultError> {
    let reply = call(
        connection,
        SERVICE_PATH,
        SERVICE_INTERFACE,
        "SearchItems",
        &(attributes(),),
    )?;
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
        reply.body().deserialize().map_err(|_| VaultError::Failed)?;
    if unlocked.len() + locked.len() > MAX_ITEMS {
        return Err(VaultError::Failed);
    }
    Ok((unlocked, locked))
}

/// The stored secret, unlocking its collection first when it is locked (the
/// provider may then prompt). `None` when no item matches.
fn load(
    connection: &Connection,
    session: &Session<'_>,
) -> Result<Option<Zeroizing<[u8; SECRET_BYTES]>>, VaultError> {
    let (_, locked) = search(connection)?;
    if !locked.is_empty() {
        unlock(connection, &locked)?;
    }
    let (unlocked, locked) = search(connection)?;
    let mut all = unlocked.clone();
    all.extend(locked.iter().cloned());
    let Some(item) = canonical(&all) else {
        return Ok(None);
    };
    if locked.contains(item) {
        return Err(VaultError::Failed);
    }
    let reply = call(
        connection,
        item.as_str(),
        ITEM_INTERFACE,
        "GetSecret",
        &(&session.path,),
    )?;
    let (_session, _parameters, value, _content_type): (OwnedObjectPath, Vec<u8>, Vec<u8>, String) =
        reply.body().deserialize().map_err(|_| VaultError::Failed)?;
    let value = Zeroizing::new(value);
    decode(&value).map(Some)
}

/// The default collection's path. No default collection is `Unsupported`:
/// there is no keyring to keep the item in.
fn default_collection(connection: &Connection) -> Result<OwnedObjectPath, VaultError> {
    let reply = call(
        connection,
        SERVICE_PATH,
        SERVICE_INTERFACE,
        "ReadAlias",
        &("default",),
    )?;
    let collection: OwnedObjectPath = reply.body().deserialize().map_err(|_| VaultError::Failed)?;
    if collection.as_str() == NONE {
        return Err(VaultError::Unsupported);
    }
    Ok(collection)
}

/// Unlocks `objects`; the provider may prompt the user.
fn unlock(connection: &Connection, objects: &[OwnedObjectPath]) -> Result<(), VaultError> {
    let reply = call(
        connection,
        SERVICE_PATH,
        SERVICE_INTERFACE,
        "Unlock",
        &(objects,),
    )?;
    let (_unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
        reply.body().deserialize().map_err(|_| VaultError::Failed)?;
    complete(connection, &prompt)
}

/// Runs the provider's prompt at `prompt`, if there is one, and waits for
/// it to complete. A dismissed prompt, or one not answered within
/// [`PROMPT_TIMEOUT`] (then dismissed here), fails.
fn complete(connection: &Connection, prompt: &OwnedObjectPath) -> Result<(), VaultError> {
    if prompt.as_str() == NONE {
        return Ok(());
    }
    let failed = |_| VaultError::Failed;
    let proxy: Proxy<'static> = ProxyBuilder::new(connection)
        .destination(SECRETS)
        .and_then(|builder| builder.path(prompt.clone()))
        .and_then(|builder| builder.interface(PROMPT_INTERFACE))
        .map_err(failed)?
        .cache_properties(CacheProperties::No)
        .build()
        .map_err(failed)?;
    // Subscribed before the prompt starts, so the answer cannot be missed.
    let mut completed = proxy.receive_signal("Completed").map_err(failed)?;
    // No parent window: the core has none that the provider could use.
    proxy.call_method("Prompt", &("",)).map_err(failed)?;
    // The blocking iterator has no timeout, so it waits on its own thread.
    // After a timeout the dismissal ends that wait.
    let (sender, receiver) = mpsc::sync_channel::<Option<Message>>(1);
    thread::Builder::new()
        .name("core-secret-prompt".into())
        .spawn(move || {
            let _ = sender.send(completed.next());
        })
        .map_err(|_| VaultError::Failed)?;
    let message = match receiver.recv_timeout(PROMPT_TIMEOUT) {
        Ok(Some(message)) => message,
        Ok(None) => return Err(VaultError::Failed),
        Err(_) => {
            let _ = proxy.call_method("Dismiss", &());
            return Err(VaultError::Failed);
        }
    };
    let (dismissed, _result): (bool, OwnedValue) = message.body().deserialize().map_err(failed)?;
    if dismissed {
        Err(VaultError::Failed)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::unix::Wrapped;
    use crate::vault::KeyProtector;

    /// GIVEN the item's attributes THEN they name this application, the
    /// versioned purpose and the schema, as on macOS, and nothing else.
    #[test]
    fn given_the_item_then_its_attributes_name_the_app_and_purpose() {
        let attributes = attributes();
        assert_eq!(attributes.len(), 3);
        assert_eq!(attributes["service"], "io.github.omg-software.persephone");
        assert_eq!(attributes["account"], "store-key-wrap-v1");
        assert_eq!(
            attributes["xdg:schema"],
            "io.github.omg-software.persephone.StoreKeyWrap"
        );
        assert_eq!(
            Value::from(attributes).value_signature().to_string(),
            "a{ss}"
        );
    }

    /// GIVEN secrets WHEN encoded and decoded THEN they round-trip through
    /// exactly 64 lowercase hexadecimal characters.
    #[test]
    fn given_a_secret_then_it_round_trips_as_lowercase_hex() {
        let mut secret = [0u8; SECRET_BYTES];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37) ^ 0xa5;
        }
        let text = encode(&secret);
        assert_eq!(text.len(), ENCODED_BYTES);
        assert!(text.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(*decode(&text).unwrap(), secret);
        assert_eq!(
            encode(&[0xff; SECRET_BYTES]).as_slice(),
            "ff".repeat(SECRET_BYTES).as_bytes()
        );
    }

    /// GIVEN item values of the wrong length or with other characters
    /// (upper case, whitespace, raw bytes) THEN they are refused, never
    /// read as a shorter or different secret.
    #[test]
    fn given_malformed_values_then_they_are_refused() {
        let good = encode(&[0x5a; SECRET_BYTES]);
        assert!(decode(&good).is_ok());
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            good[..ENCODED_BYTES - 1].to_vec(),
            [good.as_slice(), b"0"].concat(),
            [good.as_slice(), b"\n"].concat(),
            vec![0u8; SECRET_BYTES],
            vec![0x5a; ENCODED_BYTES],
        ];
        for (index, bad) in [b'A', b'g', b' ', b'\0', 0xc3].into_iter().enumerate() {
            let mut changed = good.to_vec();
            changed[index * 7] = bad;
            cases.push(changed);
        }
        for case in cases {
            assert_eq!(decode(&case), Err(VaultError::Failed), "{case:?}");
        }
    }

    /// GIVEN bus errors THEN a missing bus or provider is unsupported and
    /// any other refusal fails; GIVEN several matching items THEN every
    /// reader picks the same one.
    #[test]
    fn given_errors_and_duplicates_then_the_choice_is_fixed() {
        assert_eq!(vault_error(BusError::Absent), VaultError::Unsupported);
        assert_eq!(vault_error(BusError::Failed), VaultError::Failed);
        let path = |text: &str| OwnedObjectPath::try_from(text).unwrap();
        let items = [
            path("/org/freedesktop/secrets/collection/login/7"),
            path("/org/freedesktop/secrets/collection/login/12"),
            path("/org/freedesktop/secrets/collection/login/3"),
        ];
        assert_eq!(
            canonical(&items).map(|item| item.as_str()),
            Some("/org/freedesktop/secrets/collection/login/12")
        );
        let mut reversed = items.clone();
        reversed.reverse();
        assert_eq!(canonical(&reversed), canonical(&items));
        assert_eq!(canonical(&[]), None);
    }

    /// GIVEN no reachable session bus (an address with nothing behind it)
    /// WHEN the vault protects or opens THEN it is unsupported and no key
    /// comes back: the profile stays locked.
    #[test]
    fn given_no_session_bus_then_the_vault_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", dir.path().join("no-bus").display());
        let store = SecretService::at(address.clone());
        assert_eq!(store.load(), Err(VaultError::Unsupported));
        assert_eq!(
            store.store_new(&[1u8; SECRET_BYTES]).err(),
            Some(VaultError::Unsupported)
        );
        let vault = Wrapped(SecretService::at(address));
        assert_eq!(
            vault.protect(b"sealed").err(),
            Some(VaultError::Unsupported)
        );
        assert_eq!(
            vault.unprotect(&[1u8; 64]).err(),
            Some(VaultError::Unsupported)
        );
    }

    /// GIVEN a Secret Service provider on the session bus (a disposable
    /// GNOME Keyring under `dbus-run-session`, never the user's own) WHEN
    /// the secret is stored twice and read THEN the first stays, and the
    /// wrap opens with it.
    #[test]
    #[ignore = "needs a disposable Secret Service provider on the session bus"]
    fn given_a_disposable_keyring_then_the_secret_is_stored_once() {
        let store = SecretService::session_bus();
        let first = [7u8; SECRET_BYTES];
        let stored = store.store_new(&first).unwrap();
        let second = [9u8; SECRET_BYTES];
        assert_eq!(*store.store_new(&second).unwrap(), *stored, "kept");
        assert_eq!(store.load().unwrap().as_deref(), Some(&*stored));
        let vault = Wrapped(SecretService::session_bus());
        let blob = vault.protect(b"sealed store key").unwrap();
        assert_eq!(
            vault.unprotect(&blob).unwrap().as_slice(),
            b"sealed store key"
        );
    }
}
