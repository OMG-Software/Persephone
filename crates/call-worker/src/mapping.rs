//! The pure parts of the worker: no LiveKit types, no I/O, no clock reads.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use persephone_call_protocol::{
    FailureCode, FromWorker, MAX_DEVICES, MAX_IDENTITY_BYTES, MAX_OTHER_CONNECTIONS,
    MAX_PARTICIPANTS, MediaDevice, ToWorker,
};

/// The most `Speaking` messages sent in any one second.
pub const MAX_SPEAKING_PER_SECOND: usize = 10;
const WINDOW: Duration = Duration::from_secs(1);

/// An identity is forwarded only when the protocol would accept it back.
pub fn valid_identity(identity: &str) -> bool {
    !identity.is_empty()
        && identity.len() <= MAX_IDENTITY_BYTES
        && !identity.chars().any(char::is_control)
}

/// The first message must be `Join` or `ListDevices`; nothing else may start.
pub fn is_first_message_allowed(message: &ToWorker) -> bool {
    matches!(message, ToWorker::Join { .. } | ToWorker::ListDevices)
}

/// LiveKit makes `index:N` IDs when the OS gives no GUID. They change when a
/// device is plugged in or out, so they are never listed or selected.
pub fn is_listable_device_id(id: &str) -> bool {
    !id.starts_with("index:")
}

/// Picks the wanted device from the available IDs. `None` means the system
/// default (no choice). A missing or unstable ID is `DeviceNotFound`.
pub fn choose_device<'a>(
    wanted: Option<&str>,
    available: impl IntoIterator<Item = &'a str>,
) -> Result<Option<String>, FailureCode> {
    let Some(wanted) = wanted else {
        return Ok(None);
    };
    if !is_listable_device_id(wanted) {
        return Err(FailureCode::DeviceNotFound);
    }
    if available.into_iter().any(|id| id == wanted) {
        Ok(Some(wanted.to_owned()))
    } else {
        Err(FailureCode::DeviceNotFound)
    }
}

/// Builds one wire list from (ID, name) pairs. It skips unstable IDs, IDs the
/// wire refuses and repeated IDs, and keeps at most `MAX_DEVICES`.
pub fn bounded_device_list<'a>(
    devices: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<MediaDevice> {
    let mut list: Vec<MediaDevice> = Vec::new();
    for (id, name) in devices {
        if list.len() >= MAX_DEVICES {
            break;
        }
        if !is_listable_device_id(id) || list.iter().any(|device| device.id == id) {
            continue;
        }
        if let Some(device) = MediaDevice::bounded(id, name) {
            list.push(device);
        }
    }
    list
}

/// What the worker saw in the room, reduced to what it reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoomChange {
    Joined(u8, String),
    Left(u8, String),
    /// The complete set of current speakers of one connection.
    Speakers(u8, Vec<String>),
    /// The room of this connection ended without a request from the core.
    /// The own SFU (0) ends the call; another SFU (1 to 3) only ends that
    /// connection (ADR-0054).
    Disconnected(u8),
}

/// Turns Speaking changes into at most [`MAX_SPEAKING_PER_SECOND`] messages
/// per second. A change that does not fit is kept and sent later, so the
/// core's view still ends up correct.
#[derive(Debug, Default)]
pub struct SpeakingTracker {
    reported: BTreeSet<(u8, String)>,
    wanted: BTreeSet<(u8, String)>,
    sent: VecDeque<Instant>,
}

impl SpeakingTracker {
    /// Replaces the speakers of one connection; the others stay.
    pub fn set_speakers(&mut self, connection: u8, identities: impl IntoIterator<Item = String>) {
        self.wanted.retain(|(known, _)| *known != connection);
        self.wanted.extend(
            identities
                .into_iter()
                .filter(|identity| valid_identity(identity))
                .take(MAX_PARTICIPANTS)
                .map(|identity| (connection, identity)),
        );
    }

    /// A participant left: nothing more is said about them.
    pub fn forget(&mut self, connection: u8, identity: &str) {
        let key = (connection, identity.to_owned());
        self.wanted.remove(&key);
        self.reported.remove(&key);
    }

    /// A connection ended: every speaker of it stops without a message
    /// (its participants are gone for the core as well).
    pub fn forget_connection(&mut self, connection: u8) {
        self.wanted.retain(|(known, _)| *known != connection);
        self.reported.retain(|(known, _)| *known != connection);
    }

    /// Returns the messages that may be sent at `now`. Stops come first.
    pub fn poll(&mut self, now: Instant) -> Vec<FromWorker> {
        while self
            .sent
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) >= WINDOW)
        {
            self.sent.pop_front();
        }
        let budget = MAX_SPEAKING_PER_SECOND.saturating_sub(self.sent.len());
        let stops = self.reported.difference(&self.wanted).cloned();
        let starts = self.wanted.difference(&self.reported).cloned();
        let due: Vec<((u8, String), bool)> = stops
            .map(|key| (key, false))
            .chain(starts.map(|key| (key, true)))
            .take(budget)
            .collect();
        let mut messages = Vec::with_capacity(due.len());
        for (key, speaking) in due {
            if speaking {
                self.reported.insert(key.clone());
            } else {
                self.reported.remove(&key);
            }
            self.sent.push_back(now);
            let (connection, identity) = key;
            messages.push(FromWorker::Speaking {
                connection,
                identity,
                speaking,
            });
        }
        messages
    }

    /// Whether a change is still waiting for budget.
    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        self.reported != self.wanted
    }
}

/// Applies one room change. Join and leave messages go out at once; speaking
/// changes wait for [`SpeakingTracker::poll`]. Identities that fail
/// [`valid_identity`] are ignored.
pub fn apply(change: RoomChange, tracker: &mut SpeakingTracker) -> Vec<FromWorker> {
    match change {
        RoomChange::Joined(connection, identity) if valid_identity(&identity) => {
            vec![FromWorker::ParticipantJoined {
                connection,
                identity,
            }]
        }
        RoomChange::Left(connection, identity) if valid_identity(&identity) => {
            tracker.forget(connection, &identity);
            vec![FromWorker::ParticipantLeft {
                connection,
                identity,
            }]
        }
        RoomChange::Speakers(connection, identities) => {
            tracker.set_speakers(connection, identities);
            Vec::new()
        }
        // The own SFU ending ends the call. Another SFU ending only ends its
        // connection: the core marks those members not reachable.
        RoomChange::Disconnected(0) => vec![FromWorker::Failed {
            code: FailureCode::Network,
        }],
        RoomChange::Disconnected(connection) => {
            tracker.forget_connection(connection);
            vec![FromWorker::Connection {
                connection,
                up: false,
            }]
        }
        RoomChange::Joined(..) | RoomChange::Left(..) => Vec::new(),
    }
}

/// One connection to another SFU (ADR-0054): `C` is the connect in flight,
/// `U` the open connection.
#[derive(Debug, Eq, PartialEq)]
pub enum Slot<C, U> {
    Connecting(C),
    Up(U),
    /// Failed or ended. The index stays used: it is never reopened.
    Ended,
}

/// The connections to other SFUs by index, 1 to [`MAX_OTHER_CONNECTIONS`].
#[derive(Debug)]
pub struct Slots<C, U>(HashMap<u8, Slot<C, U>>);

impl<C, U> Default for Slots<C, U> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<C, U> Slots<C, U> {
    /// Starts a connection at `index`. An index outside 1 to 3, or one used
    /// before (also after a failure), gives `connecting` back.
    pub fn begin(&mut self, index: u8, connecting: C) -> Result<(), C> {
        if !(1..=MAX_OTHER_CONNECTIONS).contains(&index) || self.0.contains_key(&index) {
            return Err(connecting);
        }
        self.0.insert(index, Slot::Connecting(connecting));
        Ok(())
    }

    pub fn is_connecting(&self, index: u8) -> bool {
        matches!(self.0.get(&index), Some(Slot::Connecting(_)))
    }

    /// The connect at `index` finished with an open connection.
    pub fn up(&mut self, index: u8, open: U) {
        self.0.insert(index, Slot::Up(open));
    }

    /// The connection at `index` failed or ended. Returns what it held.
    pub fn end(&mut self, index: u8) -> Option<Slot<C, U>> {
        self.0.insert(index, Slot::Ended)
    }

    /// Takes every connection out, for the end of the call.
    pub fn take_all(&mut self) -> Vec<Slot<C, U>> {
        self.0.drain().map(|(_, slot)| slot).collect()
    }
}

/// Words in a connection error that mean the TLS layer refused the server.
const TLS_WORDS: [&str; 6] = [
    "certificate",
    "tls",
    "rustls",
    "unknownissuer",
    "alert",
    "peer is incompatible",
];

/// The kind of failure a connection attempt had. The error text is read here
/// and never forwarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectFault {
    /// The server answered with an HTTP status.
    Refused,
    /// A transport error with this text.
    Transport,
    Timeout,
    Other,
}

pub fn failure_for_connect(fault: ConnectFault, text: &str) -> FailureCode {
    match fault {
        ConnectFault::Refused => FailureCode::SignallingRefused,
        ConnectFault::Timeout => FailureCode::Network,
        ConnectFault::Transport => {
            let text = text.to_ascii_lowercase();
            if TLS_WORDS.iter().any(|word| text.contains(word)) {
                FailureCode::Tls
            } else {
                FailureCode::Network
            }
        }
        ConnectFault::Other => FailureCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_call_protocol::FrameKeyMsg;
    use zeroize::Zeroizing;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn names(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("@u{index:02}:example.org:D"))
            .collect()
    }

    fn speaking(messages: &[FromWorker]) -> Vec<(String, bool)> {
        messages
            .iter()
            .map(|message| match message {
                FromWorker::Speaking {
                    identity, speaking, ..
                } => (identity.clone(), *speaking),
                other => panic!("not a Speaking message: {other:?}"),
            })
            .collect()
    }

    fn key() -> FrameKeyMsg {
        FrameKeyMsg {
            index: 1,
            bytes: Zeroizing::new(vec![1; 16]),
        }
    }

    #[test]
    fn given_join_message_when_it_is_first_then_it_is_allowed() {
        // GIVEN a Join, WHEN it is the first message, THEN the worker accepts it.
        let join = ToWorker::Join {
            sfu_url: "wss://sfu.example.org".to_owned(),
            jwt: Zeroizing::new("jwt".to_owned()),
            turn: Vec::new(),
            relay_only: false,
            own_identity: "@a:example.org:D".to_owned(),
            own_key: key(),
            recording_device: None,
            playout_device: None,
            camera_device: None,
            camera_on: false,
        };
        assert!(is_first_message_allowed(&join));
    }

    #[test]
    fn given_list_devices_when_it_is_first_then_it_is_allowed() {
        // GIVEN ListDevices, WHEN it is the first message, THEN it is accepted.
        assert!(is_first_message_allowed(&ToWorker::ListDevices));
    }

    #[test]
    fn given_device_ids_when_checked_then_index_ids_are_not_listable() {
        // GIVEN a GUID and index IDs, WHEN checked, THEN only the GUID passes.
        assert!(is_listable_device_id("{0.0.1.00000000}.{6a2c}"));
        assert!(!is_listable_device_id("index:0"));
        assert!(!is_listable_device_id("index:"));
    }

    #[test]
    fn given_no_wanted_device_when_chosen_then_system_default_is_kept() {
        // GIVEN no wanted ID, WHEN chosen, THEN nothing is selected.
        assert_eq!(choose_device(None, ["a", "b"]), Ok(None));
        assert_eq!(choose_device(None, []), Ok(None));
    }

    #[test]
    fn given_wanted_device_when_present_then_it_is_chosen() {
        // GIVEN a wanted ID in the list, WHEN chosen, THEN it is returned.
        assert_eq!(
            choose_device(Some("b"), ["a", "b"]),
            Ok(Some("b".to_owned()))
        );
    }

    #[test]
    fn given_wanted_device_when_absent_or_inexact_then_it_is_not_found() {
        // GIVEN absent and near-miss IDs, WHEN chosen, THEN DeviceNotFound.
        assert_eq!(
            choose_device(Some("c"), ["a", "b"]),
            Err(FailureCode::DeviceNotFound)
        );
        assert_eq!(
            choose_device(Some("A"), ["a"]),
            Err(FailureCode::DeviceNotFound)
        );
        assert_eq!(
            choose_device(Some("a"), []),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_index_id_when_chosen_then_it_never_matches() {
        // GIVEN an index ID that is also available, WHEN chosen, THEN refused.
        assert_eq!(
            choose_device(Some("index:1"), ["index:1"]),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_mixed_devices_when_listed_then_bad_and_repeated_ones_are_dropped() {
        // GIVEN an index ID, an empty ID, a control ID and a repeat,
        // WHEN the list is built, THEN only the first good ones stay.
        let list = bounded_device_list([
            ("index:0", "Unstable"),
            ("", "No id"),
            ("bad\u{7}id", "Control id"),
            ("a", "First\u{7}"),
            ("b", "Second"),
            ("a", "Repeat"),
        ]);
        assert_eq!(
            list,
            vec![
                MediaDevice {
                    id: "a".into(),
                    name: "First".into()
                },
                MediaDevice {
                    id: "b".into(),
                    name: "Second".into()
                },
            ]
        );
    }

    #[test]
    fn given_too_many_devices_when_listed_then_the_list_is_cut() {
        // GIVEN more devices than the limit, WHEN listed, THEN MAX_DEVICES stay.
        let ids: Vec<String> = (0..MAX_DEVICES + 5).map(|n| format!("id{n}")).collect();
        let list = bounded_device_list(ids.iter().map(|id| (id.as_str(), "n")));
        assert_eq!(list.len(), MAX_DEVICES);
        assert_eq!(list[0].id, "id0");
    }

    #[test]
    fn given_any_other_message_when_it_is_first_then_it_is_refused() {
        // GIVEN each non-Join message, WHEN it comes first, THEN it is refused.
        for message in [
            ToWorker::Leave,
            ToWorker::SetMuted { muted: true },
            ToWorker::SetCamera {
                on: true,
                remote: false,
            },
            ToWorker::SetScreen { target: None },
            ToWorker::SetScreen {
                target: Some(persephone_call_protocol::ScreenTarget::Portal { node: 7 }),
            },
            ToWorker::SetOwnKey { key: key() },
            ToWorker::SetKey {
                identity: "@a:example.org:D".to_owned(),
                key: key(),
            },
            ToWorker::RemoveParticipant {
                identity: "@a:example.org:D".to_owned(),
            },
            ToWorker::Subscribe {
                index: 1,
                url: "wss://sfu.other.example".to_owned(),
                jwt: Zeroizing::new("a.b.c".to_owned()),
            },
        ] {
            assert!(!is_first_message_allowed(&message), "{message:?}");
        }
    }

    #[test]
    fn given_identities_when_checked_then_only_protocol_valid_ones_pass() {
        // GIVEN empty, long, control-character and normal identities,
        // WHEN checked, THEN only the normal and the longest allowed pass.
        assert!(valid_identity("@alice:example.org:DEVICE"));
        assert!(valid_identity(&"a".repeat(MAX_IDENTITY_BYTES)));
        assert!(!valid_identity(""));
        assert!(!valid_identity(&"a".repeat(MAX_IDENTITY_BYTES + 1)));
        assert!(!valid_identity("bad\nname"));
    }

    #[test]
    fn given_participant_events_when_applied_then_messages_follow_the_identity_rules() {
        // GIVEN a tracker, WHEN a valid join and leave and an invalid join arrive,
        // THEN the valid ones map to messages and the invalid one is ignored.
        let mut tracker = SpeakingTracker::default();
        assert_eq!(
            apply(
                RoomChange::Joined(0, "@a:example.org:D".into()),
                &mut tracker
            ),
            vec![FromWorker::ParticipantJoined {
                connection: 0,
                identity: "@a:example.org:D".into()
            }]
        );
        assert_eq!(
            apply(RoomChange::Left(0, "@a:example.org:D".into()), &mut tracker),
            vec![FromWorker::ParticipantLeft {
                connection: 0,
                identity: "@a:example.org:D".into()
            }]
        );
        assert!(apply(RoomChange::Joined(0, "bad\u{7}".into()), &mut tracker).is_empty());
    }

    #[test]
    fn given_participants_of_other_connections_when_applied_then_messages_carry_the_index() {
        // GIVEN a tracker, WHEN a join and a leave arrive on connection 2,
        // THEN each message names connection 2.
        let mut tracker = SpeakingTracker::default();
        assert_eq!(
            apply(
                RoomChange::Joined(2, "@c:other.example:D".into()),
                &mut tracker
            ),
            vec![FromWorker::ParticipantJoined {
                connection: 2,
                identity: "@c:other.example:D".into()
            }]
        );
        assert_eq!(
            apply(
                RoomChange::Left(2, "@c:other.example:D".into()),
                &mut tracker
            ),
            vec![FromWorker::ParticipantLeft {
                connection: 2,
                identity: "@c:other.example:D".into()
            }]
        );
    }

    #[test]
    fn given_another_sfu_disconnects_then_only_its_connection_ends() {
        // GIVEN speakers on connections 0 and 1, WHEN connection 1 disconnects,
        // THEN the message is Connection down (not a failure of the call) and
        // the speakers of connection 0 stay.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, names(1));
        tracker.set_speakers(1, names(2));
        assert_eq!(tracker.poll(start).len(), 3);
        assert_eq!(
            apply(RoomChange::Disconnected(1), &mut tracker),
            vec![FromWorker::Connection {
                connection: 1,
                up: false
            }]
        );
        tracker.set_speakers(0, names(1));
        assert!(tracker.poll(at(start, 2000)).is_empty());
        assert!(!tracker.has_pending());
        // WHEN the own SFU disconnects, THEN the call fails with Network.
        assert_eq!(
            apply(RoomChange::Disconnected(0), &mut tracker),
            vec![FromWorker::Failed {
                code: FailureCode::Network
            }]
        );
    }

    #[test]
    fn given_speakers_of_two_connections_then_each_set_is_kept_apart() {
        // GIVEN speakers on connection 0, WHEN connection 2 reports its own
        // set, THEN connection 0's speakers are not replaced, and each
        // Speaking message names its connection.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, vec!["@a:x:D".into()]);
        tracker.set_speakers(2, vec!["@b:y:D".into()]);
        let sent = tracker.poll(start);
        assert!(sent.contains(&FromWorker::Speaking {
            connection: 0,
            identity: "@a:x:D".into(),
            speaking: true
        }));
        assert!(sent.contains(&FromWorker::Speaking {
            connection: 2,
            identity: "@b:y:D".into(),
            speaking: true
        }));
        tracker.set_speakers(2, Vec::new());
        let sent = tracker.poll(at(start, 2000));
        assert_eq!(
            sent,
            vec![FromWorker::Speaking {
                connection: 2,
                identity: "@b:y:D".into(),
                speaking: false
            }]
        );
    }

    #[test]
    fn given_connection_slots_then_an_index_opens_once_and_only_1_to_3() {
        // GIVEN empty slots, WHEN indices 0 and 4 are begun, THEN they are
        // refused; WHEN 1 is begun twice, THEN the second is refused, also
        // after the first failed.
        let mut slots: Slots<&str, &str> = Slots::default();
        assert_eq!(slots.begin(0, "c"), Err("c"));
        assert_eq!(slots.begin(4, "c"), Err("c"));
        assert_eq!(slots.begin(1, "c"), Ok(()));
        assert!(slots.is_connecting(1));
        assert_eq!(slots.begin(1, "d"), Err("d"));
        assert_eq!(slots.end(1), Some(Slot::Connecting("c")));
        assert_eq!(slots.begin(1, "e"), Err("e"));
        assert!(!slots.is_connecting(1));
    }

    #[test]
    fn given_open_and_pending_connections_when_the_call_ends_then_all_are_taken() {
        // GIVEN one open, one pending and one failed connection, WHEN the
        // call ends, THEN every one is handed over to be closed or stopped.
        let mut slots: Slots<&str, &str> = Slots::default();
        slots.begin(1, "c1").unwrap();
        slots.up(1, "u1");
        slots.begin(2, "c2").unwrap();
        slots.begin(3, "c3").unwrap();
        slots.end(3);
        let mut all = slots.take_all();
        all.sort_by_key(|slot| format!("{slot:?}"));
        assert_eq!(
            all,
            vec![Slot::Connecting("c2"), Slot::Ended, Slot::Up("u1")]
        );
        assert!(slots.take_all().is_empty());
    }

    #[test]
    fn given_room_disconnect_when_applied_then_the_call_fails_with_network() {
        // GIVEN a room that ends by itself, WHEN applied, THEN Failed(Network) results.
        let mut tracker = SpeakingTracker::default();
        assert_eq!(
            apply(RoomChange::Disconnected(0), &mut tracker),
            vec![FromWorker::Failed {
                code: FailureCode::Network
            }]
        );
    }

    #[test]
    fn given_speaker_set_when_it_changes_then_only_changes_are_reported() {
        // GIVEN two speakers reported, WHEN one stops and one starts,
        // THEN exactly one stop and one start follow, stop first.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        let ids = names(3);
        tracker.set_speakers(0, ids[..2].to_vec());
        assert_eq!(speaking(&tracker.poll(start)).len(), 2);
        assert!(tracker.poll(at(start, 10)).is_empty());
        tracker.set_speakers(0, vec![ids[1].clone(), ids[2].clone()]);
        assert_eq!(
            speaking(&tracker.poll(at(start, 20))),
            vec![(ids[0].clone(), false), (ids[2].clone(), true)]
        );
    }

    #[test]
    fn given_many_speakers_when_polled_then_at_most_ten_go_out_per_second() {
        // GIVEN 25 speakers at once, WHEN polled over time,
        // THEN each second carries at most 10 messages and all arrive in the end.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, names(25));
        assert_eq!(tracker.poll(start).len(), 10);
        assert!(tracker.has_pending());
        assert!(tracker.poll(at(start, 500)).is_empty());
        assert_eq!(tracker.poll(at(start, 1000)).len(), 10);
        assert_eq!(tracker.poll(at(start, 2000)).len(), 5);
        assert!(!tracker.has_pending());
    }

    #[test]
    fn given_a_leaving_participant_when_forgotten_then_no_stop_is_sent() {
        // GIVEN a reported speaker, WHEN they leave, THEN no Speaking message follows.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, names(1));
        tracker.poll(start);
        apply(RoomChange::Left(0, names(1).remove(0)), &mut tracker);
        tracker.set_speakers(0, Vec::new());
        assert!(tracker.poll(at(start, 2000)).is_empty());
    }

    #[test]
    fn given_invalid_or_too_many_speakers_when_set_then_they_are_dropped() {
        // GIVEN an invalid identity and more than 32 speakers,
        // WHEN set, THEN the invalid one is dropped and the set is capped.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        let mut ids = names(40);
        ids.push("bad\u{1}".to_owned());
        tracker.set_speakers(0, ids);
        let mut total = 0;
        for second in 0..5 {
            let messages = tracker.poll(start + Duration::from_secs(second * 2));
            assert!(
                speaking(&messages)
                    .iter()
                    .all(|(identity, _)| valid_identity(identity))
            );
            total += messages.len();
        }
        assert_eq!(total, MAX_PARTICIPANTS);
    }

    #[test]
    fn given_connect_faults_when_mapped_then_codes_match_without_text() {
        // GIVEN each kind of fault, WHEN mapped, THEN the closed code follows.
        use ConnectFault::*;
        assert_eq!(
            failure_for_connect(Refused, ""),
            FailureCode::SignallingRefused
        );
        assert_eq!(failure_for_connect(Timeout, ""), FailureCode::Network);
        assert_eq!(failure_for_connect(Other, ""), FailureCode::Internal);
        assert_eq!(
            failure_for_connect(Transport, "connection refused"),
            FailureCode::Network
        );
        assert_eq!(
            failure_for_connect(Transport, "invalid peer certificate: UnknownIssuer"),
            FailureCode::Tls
        );
        assert_eq!(
            failure_for_connect(Transport, "received fatal alert: ProtocolVersion"),
            FailureCode::Tls
        );
    }
}
