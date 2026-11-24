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
//! Without a session bus or a Secret Service provider, the vault is
//! [`VaultError::Missing`] with that part as the reason. Without a default
//! collection, the core asks in its own window whether the provider should
//! create one ([`default_collection`], ADR-0061); the provider then asks for
//! the new keyring's password in its own prompt, which Persephone never
//! sees. A "no", a closed window, a dismissed or unanswered keyring prompt,
//! a provider that refuses, or no display to ask on, is again
//! [`VaultError::Missing`]. The vault never calls `SetAlias`, so it never
//! changes which existing keyring is the default. A locked collection whose
//! unlock prompt the user dismisses, or does not answer within
//! [`PROMPT_TIMEOUT`], and every other refusal is [`VaultError::Failed`].
//! Either way the profile stays locked: there is no kernel keyring, file or
//! passphrase-only fallback.
//!
//! A core creates the item only while it owns [`CREATION_NAME`] on the
//! session bus ([`CreationLock`], on its own connection with a short reply
//! timeout), from its first search to its read-back.
//! The bus is what every core that reaches this Secret Service shares, so
//! two such cores never create two items, whatever their environments: a
//! core that waits for the name finds the item the other created. Without
//! it, a core that waits for its user's answer could create a second item
//! after another core already returned the first, and later reads could
//! choose the second (ADR-0061).
//!
//! The secret is copied into zbus's message buffers on its way to and from
//! the provider; those copies are outside the zeroing guarantee.

use super::super::unix::{SECRET_BYTES, SecretStore};
use super::bus::{self, BusError};
use crate::credentials::PromptError;
use crate::vault::{Missing, VaultError};
use std::{
    collections::HashMap,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use zbus::{
    blocking::{Connection, Proxy, proxy::Builder as ProxyBuilder},
    fdo::{RequestNameFlags, RequestNameReply},
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
/// Longest wait for the user to answer the provider's unlock or
/// create-keyring prompt. The prompt is then dismissed and the profile stays
/// locked.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Most matching items accepted in one search; more is refused.
const MAX_ITEMS: usize = 64;
/// The session-bus name a core owns while it may create the item.
pub const CREATION_NAME: &str = "io.github.omg_software.persephone.StoreKeyWrapCreation";
/// Longest wait for another core that owns [`CREATION_NAME`]: long enough
/// for a creation without prompts, short enough that a core started while
/// another asks its user is not held before its window opens. Then this
/// attempt is [`VaultError::Busy`], and Unlock can try again. One request
/// that started before the end can add at most [`CREATION_CALL_TIMEOUT`].
pub const CREATION_WAIT: Duration = Duration::from_secs(5);
/// Longest wait for one reply of the bus while a core asks for
/// [`CREATION_NAME`]. The bus answers this at once when it works; the
/// general [`bus::METHOD_TIMEOUT`] would let a bus that does not answer hold
/// a starting core for far longer than [`CREATION_WAIT`].
pub const CREATION_CALL_TIMEOUT: Duration = Duration::from_secs(1);
/// How often a waiting core asks for [`CREATION_NAME`] again.
const CREATION_POLL: Duration = Duration::from_millis(100);

const SECRETS: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const SESSION_INTERFACE: &str = "org.freedesktop.Secret.Session";
const PROMPT_INTERFACE: &str = "org.freedesktop.Secret.Prompt";
/// The object path that means "none" (no prompt, no collection).
const NONE: &str = "/";
/// The alias of the collection that keeps the item.
const DEFAULT_ALIAS: &str = "default";
/// The label of a keyring that the vault asks the provider to create, as
/// libsecret's own clients name the login keyring.
pub const NEW_KEYRING_LABEL: &str = "Login";

/// The core's question before it asks the provider to create a default
/// keyring: `Ok(true)` only for an explicit yes.
pub type Ask = fn() -> Result<bool, PromptError>;

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

/// The vault error for a failed connection: a missing session bus is
/// [`Missing::SessionBus`], anything else `Failed`.
pub fn connect_error(error: BusError) -> VaultError {
    match error {
        BusError::Absent => VaultError::Missing(Missing::SessionBus),
        BusError::Failed => VaultError::Failed,
    }
}

/// The vault error for a failed call: no provider owns the name, and none
/// can be started, is [`Missing::SecretService`]; anything else `Failed`.
pub fn call_error(error: BusError) -> VaultError {
    match error {
        BusError::Absent => VaultError::Missing(Missing::SecretService),
        BusError::Failed => VaultError::Failed,
    }
}

/// What the answer to the create-keyring question leaves: only a yes goes
/// on. No, a closed window or a window that did not run is "no default
/// keyring"; no display to ask on is [`Missing::Display`].
pub fn creation_allowed(answer: Result<bool, PromptError>) -> Result<(), VaultError> {
    match answer {
        Ok(true) => Ok(()),
        Err(PromptError::Unsupported) => Err(VaultError::Missing(Missing::Display)),
        Ok(false) | Err(PromptError::Cancelled | PromptError::Invalid | PromptError::Failed) => {
            Err(VaultError::Missing(Missing::DefaultKeyring))
        }
    }
}

/// What a failed `CreateCollection` (or its prompt) leaves: a refusal, a
/// dismissed or unanswered prompt, or any other failure is "no default
/// keyring"; a provider or bus that went away keeps its own reason.
pub fn creation_error(error: VaultError) -> VaultError {
    match error {
        VaultError::Failed => VaultError::Missing(Missing::DefaultKeyring),
        other => other,
    }
}

/// The provider calls that finding or creating the default collection
/// needs. There is deliberately no `SetAlias`.
pub trait Keyrings {
    /// The collection the `default` alias names, `None` for none.
    fn read_default(&self) -> Result<Option<OwnedObjectPath>, VaultError>;
    /// Asks the provider to create a collection with the `default` alias,
    /// waits for its prompt, if any, and returns the collection it made.
    fn create_default(&self) -> Result<OwnedObjectPath, VaultError>;
}

/// The default collection. When there is none, asks once with `ask`, and
/// only after a yes has the provider create one; afterwards the created
/// collection is used only if the provider now names it `default`. Another
/// collection under the alias (another client changed it meanwhile) is not
/// used (ADR-0061).
pub fn default_collection(
    keyrings: &impl Keyrings,
    ask: Ask,
) -> Result<OwnedObjectPath, VaultError> {
    if let Some(collection) = keyrings.read_default()? {
        return Ok(collection);
    }
    creation_allowed(ask())?;
    let created = keyrings.create_default().map_err(creation_error)?;
    match keyrings.read_default()? {
        Some(collection) if collection == created => Ok(collection),
        _ => Err(VaultError::Missing(Missing::DefaultKeyring)),
    }
}

/// The collection that `CreateCollection` made: the path in its reply or,
/// when that is `/` because the provider prompted, the object path in the
/// prompt's result. `None` when neither names a collection.
pub fn created_collection(
    reply: OwnedObjectPath,
    prompted: Option<&Value<'_>>,
) -> Option<OwnedObjectPath> {
    if reply.as_str() != NONE {
        return Some(reply);
    }
    match prompted? {
        Value::ObjectPath(path) if path.as_str() != NONE => Some(path.clone().into()),
        Value::Value(inner) => created_collection(reply, Some(inner)),
        _ => None,
    }
}

/// The item every process uses when more than one matches (an older core
/// without the [`CreationLock`] that stored at the same moment, or a core on
/// another bus to the same provider): the smallest object path. Items keep their paths, so every reader picks the
/// same one.
pub fn canonical(items: &[OwnedObjectPath]) -> Option<&OwnedObjectPath> {
    items.iter().min_by(|a, b| a.as_str().cmp(b.as_str()))
}

/// What one request for [`CREATION_NAME`] leaves: the name is ours, another
/// core owns it and there is time to wait, another core still owns it when
/// the wait is over, or anything else (a refusal, a failed call, or a queue
/// entry that `DoNotQueue` rules out), which fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Owned,
    Wait,
    Busy,
    Refused,
}

/// The [`Claim`] for one reply to `RequestName`, when the wait has (or has
/// not) `expired`.
pub fn claim(reply: Option<RequestNameReply>, expired: bool) -> Claim {
    match reply {
        Some(RequestNameReply::PrimaryOwner) => Claim::Owned,
        Some(RequestNameReply::Exists) if !expired => Claim::Wait,
        Some(RequestNameReply::Exists) => Claim::Busy,
        _ => Claim::Refused,
    }
}

/// Ownership of [`CREATION_NAME`] on the lock's own connection. Every core
/// that reaches this Secret Service through the same session bus asks for
/// the same name, so at most one of them may create the item at a time. The
/// bus releases the name when the connection closes, also when the process
/// ends, so a stopped core leaves no stale lock.
pub struct CreationLock {
    connection: Connection,
}

impl CreationLock {
    /// Owns the name on a new connection to the bus at `address` (`None`:
    /// the user's session bus), waiting at most `wait` for another owner.
    /// Each call to the bus has at most [`CREATION_CALL_TIMEOUT`], so the
    /// whole wait is at most `wait` plus one such call. Another owner after
    /// the wait is `Busy`; any other failure, also a bus that does not
    /// answer in time, is `Failed`.
    pub fn acquire(address: Option<&str>, wait: Duration) -> Result<Self, VaultError> {
        let connection = match address {
            None => bus::session_with(CREATION_CALL_TIMEOUT),
            Some(address) => bus::at_with(address, CREATION_CALL_TIMEOUT),
        }
        .map_err(connect_error)?;
        let deadline = Instant::now() + wait;
        loop {
            // The bus's own method, not zbus's wrapper: the wrapper turns
            // `Exists` into an error and adds signal matches on each try.
            let reply = bus_call(
                &connection,
                "RequestName",
                &(CREATION_NAME, RequestNameFlags::DoNotQueue as u32),
            )
            .and_then(|reply| reply.body().deserialize::<RequestNameReply>().ok());
            match claim(reply, Instant::now() >= deadline) {
                Claim::Owned => return Ok(Self { connection }),
                Claim::Wait => thread::sleep(CREATION_POLL),
                Claim::Busy => return Err(VaultError::Busy),
                Claim::Refused => return Err(VaultError::Failed),
            }
        }
    }
}

impl Drop for CreationLock {
    fn drop(&mut self) {
        // Closing the connection releases it too; this lets a waiting core
        // go on at once.
        let _ = bus_call(&self.connection, "ReleaseName", &(CREATION_NAME,));
    }
}

/// One method call to the message bus itself; `None` when it fails.
fn bus_call<B>(connection: &Connection, method: &str, body: &B) -> Option<Message>
where
    B: serde::Serialize + DynamicType,
{
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            method,
            body,
        )
        .ok()
}

/// The wrapping secret's Secret Service item, reached over the session
/// bus on each use.
pub struct SecretService {
    /// `None`: the user's session bus. Tests name another address.
    address: Option<String>,
    /// The question before a default keyring is created.
    ask: Ask,
}

impl SecretService {
    /// The item on the user's session bus; a missing default keyring is
    /// asked about in the core's own window.
    pub fn session_bus() -> Self {
        Self {
            address: None,
            ask: super::credentials::confirm_keyring_creation,
        }
    }

    /// The item behind the bus at `address`, asking with `ask` (tests).
    pub fn at(address: String, ask: Ask) -> Self {
        Self {
            address: Some(address),
            ask,
        }
    }

    fn connect(&self) -> Result<Connection, VaultError> {
        match &self.address {
            None => bus::session(),
            Some(address) => bus::at(address),
        }
        .map_err(connect_error)
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
        // Held until the read-back, through every wait for the user, so no
        // other core on this bus creates an item meanwhile.
        let _creating = CreationLock::acquire(self.address.as_deref(), CREATION_WAIT)?;
        // Another process may have stored one since our caller looked.
        if let Some(stored) = load(&connection, &session)? {
            return Ok(stored);
        }
        let collection = default_collection(&Provider(&connection), self.ask)?;
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
        // Read back the item that later reads choose; under the lock it is
        // the one just created, unless an older core raced without it.
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
        .map_err(|error| call_error(bus::classify(&error)))
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

/// The provider on one connection.
struct Provider<'c>(&'c Connection);

impl Keyrings for Provider<'_> {
    fn read_default(&self) -> Result<Option<OwnedObjectPath>, VaultError> {
        let reply = call(
            self.0,
            SERVICE_PATH,
            SERVICE_INTERFACE,
            "ReadAlias",
            &(DEFAULT_ALIAS,),
        )?;
        let collection: OwnedObjectPath =
            reply.body().deserialize().map_err(|_| VaultError::Failed)?;
        Ok((collection.as_str() != NONE).then_some(collection))
    }

    /// `CreateCollection` with the label [`NEW_KEYRING_LABEL`] and the
    /// `default` alias. The provider asks for the keyring's password in its
    /// own prompt; Persephone neither sees nor sets it.
    fn create_default(&self) -> Result<OwnedObjectPath, VaultError> {
        let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
        properties.insert(
            "org.freedesktop.Secret.Collection.Label",
            Value::from(NEW_KEYRING_LABEL),
        );
        let reply = call(
            self.0,
            SERVICE_PATH,
            SERVICE_INTERFACE,
            "CreateCollection",
            &(properties, DEFAULT_ALIAS),
        )?;
        let (collection, prompt): (OwnedObjectPath, OwnedObjectPath) =
            reply.body().deserialize().map_err(|_| VaultError::Failed)?;
        let prompted = complete(self.0, &prompt)?;
        created_collection(collection, prompted.as_deref()).ok_or(VaultError::Failed)
    }
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
    complete(connection, &prompt).map(drop)
}

/// Runs the provider's prompt at `prompt` (`/`: none), waits for it to
/// complete and returns its result for the caller that needs it (only
/// `create_default`, for [`created_collection`]; [`unlock`] and `store_new`
/// ignore it); `None` when there is no prompt. A dismissed prompt, or one not answered
/// within [`PROMPT_TIMEOUT`] (then dismissed here), is `Failed`.
fn complete(
    connection: &Connection,
    prompt: &OwnedObjectPath,
) -> Result<Option<OwnedValue>, VaultError> {
    if prompt.as_str() == NONE {
        return Ok(None);
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
    let (dismissed, result): (bool, OwnedValue) = message.body().deserialize().map_err(failed)?;
    if dismissed {
        Err(VaultError::Failed)
    } else {
        Ok(Some(result))
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

    /// GIVEN bus errors THEN a missing bus and a missing provider each keep
    /// their own reason and any other refusal fails; GIVEN several matching
    /// items THEN every reader picks the same one.
    #[test]
    fn given_errors_and_duplicates_then_the_choice_is_fixed() {
        assert_eq!(
            connect_error(BusError::Absent),
            VaultError::Missing(Missing::SessionBus)
        );
        assert_eq!(connect_error(BusError::Failed), VaultError::Failed);
        assert_eq!(
            call_error(BusError::Absent),
            VaultError::Missing(Missing::SecretService)
        );
        assert_eq!(call_error(BusError::Failed), VaultError::Failed);
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

    /// GIVEN replies to `RequestName` THEN only primary ownership takes the
    /// lock; another owner waits until the wait expires and is then busy,
    /// and a queue entry, an existing own claim or a failed call fails.
    #[test]
    fn given_name_replies_then_only_primary_ownership_takes_the_lock() {
        assert_eq!(
            claim(Some(RequestNameReply::PrimaryOwner), false),
            Claim::Owned
        );
        assert_eq!(
            claim(Some(RequestNameReply::PrimaryOwner), true),
            Claim::Owned
        );
        assert_eq!(claim(Some(RequestNameReply::Exists), false), Claim::Wait);
        assert_eq!(claim(Some(RequestNameReply::Exists), true), Claim::Busy);
        for expired in [false, true] {
            assert_eq!(
                claim(Some(RequestNameReply::InQueue), expired),
                Claim::Refused
            );
            assert_eq!(
                claim(Some(RequestNameReply::AlreadyOwner), expired),
                Claim::Refused
            );
            assert_eq!(claim(None, expired), Claim::Refused);
        }
        assert!(zbus::names::WellKnownName::try_from(CREATION_NAME).is_ok());
    }

    /// GIVEN no reachable session bus (an address with nothing behind it)
    /// WHEN the vault protects or opens THEN the reason is "no session bus",
    /// nothing is asked and no key comes back: the profile stays locked.
    #[test]
    fn given_no_session_bus_then_the_vault_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", dir.path().join("no-bus").display());
        let no_bus = Some(VaultError::Missing(Missing::SessionBus));
        let store = SecretService::at(address.clone(), never_asked);
        assert_eq!(store.load().err(), no_bus);
        assert_eq!(store.store_new(&[1u8; SECRET_BYTES]).err(), no_bus);
        let vault = Wrapped(SecretService::at(address, never_asked));
        assert_eq!(vault.protect(b"sealed").err(), no_bus);
        assert_eq!(vault.unprotect(&[1u8; 64]).err(), no_bus);
    }

    fn never_asked() -> Result<bool, PromptError> {
        panic!("the create-keyring question must not be asked here")
    }

    /// A provider in memory: what `ReadAlias` gives before and after a
    /// creation, what `CreateCollection` does, and what was called.
    struct Fake {
        before: Result<Option<&'static str>, VaultError>,
        create: Result<&'static str, VaultError>,
        after: Option<&'static str>,
        calls: std::cell::RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(before: Option<&'static str>) -> Self {
            Self {
                before: Ok(before),
                create: Ok(LOGIN),
                after: Some(LOGIN),
                calls: std::cell::RefCell::default(),
            }
        }
    }

    const LOGIN: &str = "/org/freedesktop/secrets/collection/login";
    const OTHER: &str = "/org/freedesktop/secrets/collection/other";

    impl Keyrings for Fake {
        fn read_default(&self) -> Result<Option<OwnedObjectPath>, VaultError> {
            let created = self.calls.borrow().contains(&"create");
            self.calls.borrow_mut().push("read");
            let path = if created {
                self.after
            } else {
                match &self.before {
                    Ok(path) => *path,
                    Err(VaultError::Missing(missing)) => {
                        return Err(VaultError::Missing(*missing));
                    }
                    Err(_) => return Err(VaultError::Failed),
                }
            };
            Ok(path.map(|path| OwnedObjectPath::try_from(path).unwrap()))
        }

        fn create_default(&self) -> Result<OwnedObjectPath, VaultError> {
            self.calls.borrow_mut().push("create");
            match &self.create {
                Ok(path) => Ok(OwnedObjectPath::try_from(*path).unwrap()),
                Err(VaultError::Missing(missing)) => Err(VaultError::Missing(*missing)),
                Err(_) => Err(VaultError::Failed),
            }
        }
    }

    thread_local! {
        static ASKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn asked() -> u32 {
        ASKED.with(std::cell::Cell::take)
    }

    fn yes() -> Result<bool, PromptError> {
        ASKED.with(|asked| asked.set(asked.get() + 1));
        Ok(true)
    }

    fn no() -> Result<bool, PromptError> {
        ASKED.with(|asked| asked.set(asked.get() + 1));
        Ok(false)
    }

    fn closed() -> Result<bool, PromptError> {
        ASKED.with(|asked| asked.set(asked.get() + 1));
        Err(PromptError::Cancelled)
    }

    fn not_shown() -> Result<bool, PromptError> {
        ASKED.with(|asked| asked.set(asked.get() + 1));
        Err(PromptError::Failed)
    }

    fn no_display() -> Result<bool, PromptError> {
        ASKED.with(|asked| asked.set(asked.get() + 1));
        Err(PromptError::Unsupported)
    }

    /// GIVEN a default collection WHEN the item's collection is found THEN
    /// it is used as it is: nothing is asked and nothing is created.
    #[test]
    fn given_a_default_keyring_then_it_is_used_without_a_question() {
        asked();
        let fake = Fake::new(Some(LOGIN));
        assert_eq!(
            default_collection(&fake, never_asked).unwrap().as_str(),
            LOGIN
        );
        assert_eq!(*fake.calls.borrow(), ["read"]);
    }

    /// GIVEN no default collection WHEN the user says yes and the provider
    /// creates one under the `default` alias THEN that collection is used,
    /// after exactly one question and one creation.
    #[test]
    fn given_no_default_keyring_and_a_yes_then_one_is_created_once() {
        asked();
        let fake = Fake::new(None);
        assert_eq!(default_collection(&fake, yes).unwrap().as_str(), LOGIN);
        assert_eq!(asked(), 1);
        assert_eq!(*fake.calls.borrow(), ["read", "create", "read"]);
    }

    /// GIVEN no default collection WHEN the user says no, closes the window,
    /// or the window does not run THEN nothing is created and the reason is
    /// "no default keyring"; WHEN there is no display THEN nothing is
    /// created and the reason is "no display".
    #[test]
    fn given_no_default_keyring_and_no_yes_then_nothing_is_created() {
        asked();
        for (ask, reason) in [
            (no as Ask, Missing::DefaultKeyring),
            (closed, Missing::DefaultKeyring),
            (not_shown, Missing::DefaultKeyring),
            (no_display, Missing::Display),
        ] {
            let fake = Fake::new(None);
            assert_eq!(
                default_collection(&fake, ask).err(),
                Some(VaultError::Missing(reason))
            );
            assert_eq!(asked(), 1);
            assert_eq!(*fake.calls.borrow(), ["read"], "{reason:?}");
        }
    }

    /// GIVEN a yes WHEN the provider refuses `CreateCollection`, or its
    /// prompt is dismissed or not answered (both `Failed`) THEN the reason
    /// is "no default keyring", asked once; WHEN the provider goes away
    /// meanwhile THEN the reason is "no Secret Service".
    #[test]
    fn given_a_yes_and_a_refused_creation_then_the_profile_stays_locked() {
        asked();
        for (create, reason) in [
            (VaultError::Failed, Missing::DefaultKeyring),
            (
                VaultError::Missing(Missing::SecretService),
                Missing::SecretService,
            ),
        ] {
            let fake = Fake {
                create: Err(create),
                ..Fake::new(None)
            };
            assert_eq!(
                default_collection(&fake, yes).err(),
                Some(VaultError::Missing(reason))
            );
            assert_eq!(asked(), 1);
            assert_eq!(*fake.calls.borrow(), ["read", "create"]);
        }
    }

    /// GIVEN a yes WHEN the provider creates a collection but does not name
    /// it `default` THEN it is not used, no alias is set by the vault, and
    /// the reason is "no default keyring".
    #[test]
    fn given_a_created_keyring_that_is_not_the_default_then_it_is_not_used() {
        asked();
        let fake = Fake {
            after: None,
            ..Fake::new(None)
        };
        assert_eq!(
            default_collection(&fake, yes).err(),
            Some(VaultError::Missing(Missing::DefaultKeyring))
        );
        assert_eq!(asked(), 1);
        assert_eq!(*fake.calls.borrow(), ["read", "create", "read"]);
    }

    /// GIVEN a yes WHEN the provider creates a collection but the `default`
    /// alias then names another one (another client changed it meanwhile)
    /// THEN neither is used, and the reason is "no default keyring".
    #[test]
    fn given_the_default_alias_moved_after_creation_then_nothing_is_used() {
        asked();
        let fake = Fake {
            after: Some(OTHER),
            ..Fake::new(None)
        };
        assert_eq!(
            default_collection(&fake, yes).err(),
            Some(VaultError::Missing(Missing::DefaultKeyring))
        );
        assert_eq!(asked(), 1);
        assert_eq!(*fake.calls.borrow(), ["read", "create", "read"]);
    }

    /// GIVEN `CreateCollection`'s reply and prompt result THEN the created
    /// collection is the reply's path; when that is `/`, the object path in
    /// the prompt's result (also inside a variant); and none when neither
    /// names one.
    #[test]
    fn given_creation_replies_then_the_created_collection_is_found() {
        let path = |path: &str| OwnedObjectPath::try_from(path).unwrap();
        let object = |path: &'static str| Value::ObjectPath(path.try_into().unwrap());
        assert_eq!(created_collection(path(LOGIN), None), Some(path(LOGIN)));
        assert_eq!(
            created_collection(path(LOGIN), Some(&object(OTHER))),
            Some(path(LOGIN)),
            "the reply's path wins"
        );
        assert_eq!(
            created_collection(path(NONE), Some(&object(LOGIN))),
            Some(path(LOGIN))
        );
        assert_eq!(
            created_collection(path(NONE), Some(&Value::Value(Box::new(object(LOGIN))))),
            Some(path(LOGIN))
        );
        for prompted in [None, Some(object(NONE)), Some(Value::from("login"))] {
            assert_eq!(created_collection(path(NONE), prompted.as_ref()), None);
        }
    }

    /// GIVEN `ReadAlias` that fails or finds no provider THEN that error
    /// stands and nothing is asked.
    #[test]
    fn given_a_failed_alias_read_then_nothing_is_asked() {
        for (error, expected) in [
            (VaultError::Failed, VaultError::Failed),
            (
                VaultError::Missing(Missing::SecretService),
                VaultError::Missing(Missing::SecretService),
            ),
        ] {
            let fake = Fake {
                before: Err(error),
                ..Fake::new(None)
            };
            assert_eq!(default_collection(&fake, never_asked).err(), Some(expected));
            assert_eq!(*fake.calls.borrow(), ["read"]);
        }
    }

    /// GIVEN each answer and creation failure THEN only a yes goes on, and
    /// only a refusal or failed prompt becomes "no default keyring".
    #[test]
    fn given_answers_and_failures_then_each_maps_to_one_result() {
        assert_eq!(creation_allowed(Ok(true)), Ok(()));
        for answer in [
            Ok(false),
            Err(PromptError::Cancelled),
            Err(PromptError::Invalid),
            Err(PromptError::Failed),
        ] {
            assert_eq!(
                creation_allowed(answer),
                Err(VaultError::Missing(Missing::DefaultKeyring)),
                "{answer:?}"
            );
        }
        assert_eq!(
            creation_allowed(Err(PromptError::Unsupported)),
            Err(VaultError::Missing(Missing::Display))
        );
        assert_eq!(
            creation_error(VaultError::Failed),
            VaultError::Missing(Missing::DefaultKeyring)
        );
        for missing in [Missing::SessionBus, Missing::SecretService] {
            assert_eq!(
                creation_error(VaultError::Missing(missing)),
                VaultError::Missing(missing)
            );
        }
    }

    /// The disposable session bus that `dbus-run-session` names. These
    /// tests never use the user's own bus or keyring.
    fn disposable_bus() -> String {
        std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .expect("run under dbus-run-session with a disposable keyring")
    }

    /// GIVEN a disposable GNOME Keyring with an unlocked default keyring
    /// (`gnome-keyring-daemon --unlock` under `dbus-run-session`, never the
    /// user's own) WHEN the secret is stored twice and read THEN the first
    /// stays, the wrap opens with it, and nothing is asked.
    #[test]
    #[ignore = "needs a disposable Secret Service provider on the session bus"]
    fn given_a_disposable_keyring_then_the_secret_is_stored_once() {
        let store = SecretService::at(disposable_bus(), never_asked);
        let first = [7u8; SECRET_BYTES];
        let stored = store.store_new(&first).unwrap();
        let second = [9u8; SECRET_BYTES];
        assert_eq!(*store.store_new(&second).unwrap(), *stored, "kept");
        assert_eq!(store.load().unwrap().as_deref(), Some(&*stored));
        let vault = Wrapped(SecretService::at(disposable_bus(), never_asked));
        let blob = vault.protect(b"sealed store key").unwrap();
        assert_eq!(
            vault.unprotect(&blob).unwrap().as_slice(),
            b"sealed store key"
        );
    }

    /// GIVEN a disposable bus (shared with the other live tests, so the
    /// first claim may wait for them) WHEN one lock owns the creation name
    /// THEN another lock, as another core's would, waits and is busy after
    /// its wait, and takes it while waiting once the first lets go. `held`
    /// owns the name from before the busy check until it is dropped, so the
    /// other live tests on this bus cannot free or take it during the check;
    /// after the drop, the waiter may wait for one of them, within its 30 s.
    #[test]
    #[ignore = "needs a disposable session bus"]
    fn given_a_disposable_keyring_then_the_creation_name_has_one_owner() {
        let address = disposable_bus();
        let held = CreationLock::acquire(Some(&address), Duration::from_secs(30)).unwrap();
        let started = Instant::now();
        assert!(
            matches!(
                CreationLock::acquire(Some(&address), Duration::from_millis(250)),
                Err(VaultError::Busy)
            ),
            "`held` owns the name, so a second lock is busy"
        );
        assert!(started.elapsed() >= Duration::from_millis(250));
        let waiter = thread::spawn(move || {
            CreationLock::acquire(Some(&address), Duration::from_secs(30)).is_ok()
        });
        thread::sleep(Duration::from_millis(200));
        drop(held);
        assert!(waiter.join().unwrap(), "taken once the first lets go");
    }

    /// GIVEN a disposable unlocked keyring WHEN several cores, each with its
    /// own connection, store different secrets at once THEN they all get the
    /// same secret, and exactly one item exists.
    #[test]
    #[ignore = "needs a disposable Secret Service provider on the session bus"]
    fn given_a_disposable_keyring_then_racing_cores_share_one_secret() {
        let cores: Vec<_> = (0..4u8)
            .map(|index| {
                thread::spawn(move || {
                    let store = SecretService::at(disposable_bus(), never_asked);
                    *store.store_new(&[40 + index; SECRET_BYTES]).unwrap()
                })
            })
            .collect();
        let secrets: Vec<_> = cores.into_iter().map(|core| core.join().unwrap()).collect();
        assert!(
            secrets.iter().all(|secret| *secret == secrets[0]),
            "one secret"
        );
        let connection = bus::at(&disposable_bus()).unwrap();
        let (unlocked, locked) = search(&connection).unwrap();
        assert_eq!(unlocked.len() + locked.len(), 1, "one item");
    }

    /// GIVEN a disposable GNOME Keyring with no default keyring
    /// (`gnome-keyring-daemon --start` under `dbus-run-session`, no display)
    /// WHEN a secret is stored and the user says no THEN it is asked once,
    /// nothing is created and the reason is "no default keyring"; WHEN the
    /// user says yes but the keyring cannot show its password prompt THEN
    /// the reason is the same, and still no default keyring exists.
    #[test]
    #[ignore = "needs a disposable Secret Service provider without a default keyring"]
    fn given_a_disposable_provider_without_a_default_then_it_stays_locked() {
        asked();
        let no_keyring = Some(VaultError::Missing(Missing::DefaultKeyring));
        let store = SecretService::at(disposable_bus(), no);
        assert_eq!(store.store_new(&[3u8; SECRET_BYTES]).err(), no_keyring);
        assert_eq!(asked(), 1);
        assert_eq!(store.load(), Ok(None));
        let store = SecretService::at(disposable_bus(), yes);
        assert_eq!(store.store_new(&[4u8; SECRET_BYTES]).err(), no_keyring);
        assert_eq!(asked(), 1);
        let connection = bus::at(&disposable_bus()).unwrap();
        assert_eq!(Provider(&connection).read_default(), Ok(None));
    }
}
