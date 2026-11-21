//! Frame keys for the call's second encryption layer (ADR-0043).
//!
//! The SFU ends each DTLS-SRTP connection, so every sender encrypts its own
//! media frames with its own key and sends that key to the other members.
//! This module makes and rotates this device's key and stores the keys of the
//! other members. Time is a parameter, so the rules are testable.

use super::MAX_CALL_MEMBERS;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Length of a key this device makes, in bytes.
pub const KEY_BYTES: usize = 32;

/// A key younger than this is shared with a new member; an older key is
/// replaced first.
pub const ROTATION_GRACE: Duration = Duration::from_secs(10);

/// A rotated key is used this long after it is sent, so the others have it.
pub const USE_DELAY: Duration = Duration::from_secs(1);

/// Most keys kept for one other member.
pub const MAX_KEYS_PER_MEMBER: usize = 16;

/// Shortest and longest key accepted from another member, in bytes.
const REMOTE_KEY_BYTES: std::ops::RangeInclusive<usize> = 16..=64;

/// The random source failed. No key exists; the call must not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyError;

/// One frame key and its index in the 256-slot ring.
pub struct FrameKey {
    pub index: u8,
    pub bytes: Zeroizing<[u8; KEY_BYTES]>,
}

impl fmt::Debug for FrameKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameKey")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Clone for FrameKey {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            bytes: Zeroizing::new(*self.bytes),
        }
    }
}

impl FrameKey {
    fn random(index: u8) -> Result<Self, KeyError> {
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        getrandom::fill(bytes.as_mut_slice()).map_err(|_| KeyError)?;
        Ok(Self { index, bytes })
    }
}

/// Who receives an outgoing key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipients {
    /// Every other member: the key is new.
    AllMembers,
    /// Only members that just joined: the others already have this key.
    NewMembersOnly,
}

/// A key to send, and when to start using it to encrypt.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub key: FrameKey,
    pub use_at: Instant,
    pub recipients: Recipients,
}

/// This device's current key.
#[derive(Debug)]
pub struct OwnKeys {
    current: FrameKey,
    created: Instant,
}

impl OwnKeys {
    /// Start with a random key at index 0.
    pub fn new(now: Instant) -> Result<Self, KeyError> {
        Ok(Self {
            current: FrameKey::random(0)?,
            created: now,
        })
    }

    pub fn current(&self) -> &FrameKey {
        &self.current
    }

    /// Replace the key with a new random one at the next index. The old key
    /// stays unchanged if the random source fails.
    pub fn rotate(&mut self, now: Instant) -> Result<Outgoing, KeyError> {
        let next = FrameKey::random(self.current.index.wrapping_add(1))?;
        self.current = next.clone();
        self.created = now;
        Ok(Outgoing {
            key: next,
            use_at: now + USE_DELAY,
            recipients: Recipients::AllMembers,
        })
    }

    /// A member joined: share the current key when it is young, else rotate.
    pub fn member_joined(&mut self, now: Instant) -> Result<Outgoing, KeyError> {
        if now.saturating_duration_since(self.created) < ROTATION_GRACE {
            Ok(Outgoing {
                key: self.current.clone(),
                use_at: now,
                recipients: Recipients::NewMembersOnly,
            })
        } else {
            self.rotate(now)
        }
    }

    /// A member left: that member may hold the key, so always rotate.
    pub fn member_left(&mut self, now: Instant) -> Result<Outgoing, KeyError> {
        self.rotate(now)
    }
}

/// Why a received key was not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertRefusal {
    NotMember,
    BadLength,
    TooManyMembers,
}

struct MemberKeys {
    /// Indices, oldest inserted first.
    order: VecDeque<u8>,
    keys: HashMap<u8, Zeroizing<Vec<u8>>>,
}

/// Keys received from other members, by participant identity.
#[derive(Default)]
pub struct RemoteKeys {
    members: HashMap<String, MemberKeys>,
}

impl fmt::Debug for RemoteKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteKeys")
            .field("members", &self.members.len())
            .finish_non_exhaustive()
    }
}

impl RemoteKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a key when `is_member(identity)` holds and the length fits. A
    /// repeated index replaces its key. At [`MAX_KEYS_PER_MEMBER`] the oldest
    /// inserted index goes first.
    pub fn insert(
        &mut self,
        identity: &str,
        index: u8,
        bytes: &[u8],
        is_member: &impl Fn(&str) -> bool,
    ) -> Result<(), InsertRefusal> {
        if !is_member(identity) {
            return Err(InsertRefusal::NotMember);
        }
        if !REMOTE_KEY_BYTES.contains(&bytes.len()) {
            return Err(InsertRefusal::BadLength);
        }
        if !self.members.contains_key(identity) && self.members.len() >= MAX_CALL_MEMBERS {
            return Err(InsertRefusal::TooManyMembers);
        }
        let member = self
            .members
            .entry(identity.to_owned())
            .or_insert_with(|| MemberKeys {
                order: VecDeque::new(),
                keys: HashMap::new(),
            });
        if member.keys.contains_key(&index) {
            member.order.retain(|i| *i != index);
        } else if member.order.len() >= MAX_KEYS_PER_MEMBER
            && let Some(oldest) = member.order.pop_front()
        {
            member.keys.remove(&oldest);
        }
        member.order.push_back(index);
        member.keys.insert(index, Zeroizing::new(bytes.to_vec()));
        Ok(())
    }

    pub fn remove_member(&mut self, identity: &str) {
        self.members.remove(identity);
    }

    /// Keep only the members for which `keep` holds.
    pub fn retain_members(&mut self, keep: impl Fn(&str) -> bool) {
        self.members.retain(|identity, _| keep(identity));
    }

    pub fn get(&self, identity: &str, index: u8) -> Option<&[u8]> {
        self.members
            .get(identity)?
            .keys
            .get(&index)
            .map(|k| k.as_slice())
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// The indices of the stored keys of `identity`, oldest inserted first.
    /// Only the indices: a caller reads a key with [`RemoteKeys::get`] when
    /// it gives that key on, so no other copy of a key is made.
    pub fn indices_of(&self, identity: &str) -> Vec<u8> {
        self.members
            .get(identity)
            .map_or_else(Vec::new, |member| member.order.iter().copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yes(_: &str) -> bool {
        true
    }

    /// GIVEN a new key set WHEN it starts THEN the key has index 0 and is not all zero.
    #[test]
    fn given_new_keys_then_index_zero_and_random_bytes() {
        let keys = OwnKeys::new(Instant::now()).unwrap();
        assert_eq!(keys.current().index, 0);
        assert!(keys.current().bytes.iter().any(|b| *b != 0));
    }

    /// GIVEN a key at index 255 WHEN it rotates THEN the index wraps to 0 and the bytes change.
    #[test]
    fn given_index_255_when_rotated_then_wraps_to_zero() {
        let now = Instant::now();
        let mut keys = OwnKeys::new(now).unwrap();
        for _ in 0..255 {
            keys.rotate(now).unwrap();
        }
        assert_eq!(keys.current().index, 255);
        let before = *keys.current().bytes;
        let out = keys.rotate(now).unwrap();
        assert_eq!(out.key.index, 0);
        assert_ne!(*out.key.bytes, before);
        assert_eq!(keys.current().index, 0);
    }

    /// GIVEN a rotation WHEN made THEN the key goes to everyone and is used one second later.
    #[test]
    fn given_rotation_then_all_members_and_use_delay() {
        let now = Instant::now();
        let mut keys = OwnKeys::new(now).unwrap();
        let out = keys.rotate(now).unwrap();
        assert_eq!(out.recipients, Recipients::AllMembers);
        assert_eq!(out.use_at, now + USE_DELAY);
    }

    /// GIVEN a key under ten seconds old WHEN a member joins THEN the same key goes to the new member only.
    #[test]
    fn given_young_key_when_member_joins_then_shared_not_rotated() {
        let now = Instant::now();
        let mut keys = OwnKeys::new(now).unwrap();
        let later = now + Duration::from_secs(9);
        let out = keys.member_joined(later).unwrap();
        assert_eq!(out.recipients, Recipients::NewMembersOnly);
        assert_eq!(out.key.index, 0);
        assert_eq!(*out.key.bytes, *keys.current().bytes);
        assert_eq!(out.use_at, later);
    }

    /// GIVEN a key of ten seconds or more WHEN a member joins THEN the key rotates for everyone.
    #[test]
    fn given_old_key_when_member_joins_then_rotated() {
        let now = Instant::now();
        let mut keys = OwnKeys::new(now).unwrap();
        let out = keys.member_joined(now + ROTATION_GRACE).unwrap();
        assert_eq!(out.recipients, Recipients::AllMembers);
        assert_eq!(out.key.index, 1);
    }

    /// GIVEN a young key WHEN a member leaves THEN the key still rotates.
    #[test]
    fn given_young_key_when_member_leaves_then_rotated() {
        let now = Instant::now();
        let mut keys = OwnKeys::new(now).unwrap();
        let out = keys.member_left(now).unwrap();
        assert_eq!(out.recipients, Recipients::AllMembers);
        assert_eq!(out.key.index, 1);
    }

    /// GIVEN a debug print of a key WHEN formatted THEN it shows the index and no bytes.
    #[test]
    fn given_key_debug_then_no_key_bytes() {
        let mut key = FrameKey::random(7).unwrap();
        *key.bytes = [0xAB; KEY_BYTES];
        let text = format!("{key:?}");
        assert!(text.contains('7'));
        assert!(!text.to_lowercase().contains("ab"));
        assert!(!text.contains("171"));
    }

    /// GIVEN a non-member WHEN a key arrives THEN it is refused and nothing is stored.
    #[test]
    fn given_non_member_then_key_refused() {
        let mut remote = RemoteKeys::new();
        let r = remote.insert("@a:x:D", 0, &[1; 32], &|_| false);
        assert_eq!(r, Err(InsertRefusal::NotMember));
        assert_eq!(remote.member_count(), 0);
    }

    /// GIVEN keys of 15 and 65 bytes WHEN they arrive THEN they are refused; 16 and 64 are kept.
    #[test]
    fn given_key_length_outside_range_then_refused() {
        let mut remote = RemoteKeys::new();
        assert_eq!(
            remote.insert("m", 0, &[1; 15], &yes),
            Err(InsertRefusal::BadLength)
        );
        assert_eq!(
            remote.insert("m", 0, &[1; 65], &yes),
            Err(InsertRefusal::BadLength)
        );
        assert_eq!(remote.insert("m", 0, &[1; 16], &yes), Ok(()));
        assert_eq!(remote.insert("m", 1, &[2; 64], &yes), Ok(()));
        assert_eq!(remote.get("m", 1).map(<[u8]>::len), Some(64));
    }

    /// GIVEN a full set of members WHEN a new identity sends a key THEN it is refused, a known one still works.
    #[test]
    fn given_member_cap_then_new_identity_refused() {
        let mut remote = RemoteKeys::new();
        for i in 0..MAX_CALL_MEMBERS {
            remote.insert(&format!("m{i}"), 0, &[1; 32], &yes).unwrap();
        }
        assert_eq!(
            remote.insert("extra", 0, &[1; 32], &yes),
            Err(InsertRefusal::TooManyMembers)
        );
        assert_eq!(remote.insert("m0", 1, &[2; 32], &yes), Ok(()));
        assert_eq!(remote.member_count(), MAX_CALL_MEMBERS);
    }

    /// GIVEN more keys than the limit for one member WHEN inserted THEN the oldest inserted goes first.
    #[test]
    fn given_key_cap_then_oldest_inserted_is_evicted() {
        let mut remote = RemoteKeys::new();
        for i in 0..=MAX_KEYS_PER_MEMBER as u8 {
            remote.insert("m", i, &[i; 32], &yes).unwrap();
        }
        assert!(remote.get("m", 0).is_none());
        assert!(remote.get("m", 1).is_some());
        assert!(remote.get("m", MAX_KEYS_PER_MEMBER as u8).is_some());
    }

    /// GIVEN a repeated index WHEN inserted THEN the key is replaced and counts as newest.
    #[test]
    fn given_repeated_index_then_replaced_and_newest() {
        let mut remote = RemoteKeys::new();
        for i in 0..MAX_KEYS_PER_MEMBER as u8 {
            remote.insert("m", i, &[i; 32], &yes).unwrap();
        }
        remote.insert("m", 0, &[9; 32], &yes).unwrap();
        remote.insert("m", 100, &[7; 32], &yes).unwrap();
        assert_eq!(remote.get("m", 0), Some(&[9u8; 32][..]));
        assert!(remote.get("m", 1).is_none());
    }

    /// GIVEN stored members WHEN one leaves or the set is trimmed THEN their keys are gone.
    #[test]
    fn given_member_removed_or_retained_then_keys_dropped() {
        let mut remote = RemoteKeys::new();
        for m in ["a", "b", "c"] {
            remote.insert(m, 0, &[1; 32], &yes).unwrap();
        }
        remote.remove_member("a");
        remote.retain_members(|m| m == "b");
        assert!(remote.get("a", 0).is_none());
        assert!(remote.get("b", 0).is_some());
        assert!(remote.get("c", 0).is_none());
    }

    /// GIVEN stored keys WHEN a member's keys are listed THEN they come
    /// oldest first with their bytes, and an unknown member has none.
    #[test]
    fn given_stored_keys_then_indices_of_lists_them_oldest_first() {
        let mut remote = RemoteKeys::new();
        remote.insert("a", 3, &[3; 16], &yes).unwrap();
        remote.insert("a", 1, &[1; 16], &yes).unwrap();
        assert_eq!(remote.indices_of("a"), [3, 1]);
        assert!(remote.indices_of("b").is_empty());
    }
}
