//! The sync handlers for incoming rings and call flags (ADR-0052).
//!
//! Two handlers run for the life of the sync loop. One reads ring and
//! decline room events, after decryption. The other reads call-membership
//! state events, keeps each room's call flag and ends rings whose sender is
//! no longer in the call. All decisions are in [`super::rings`]; this module
//! gathers the facts and passes the result on. A ring never starts a join,
//! opens a connection or turns on a device.

use super::controller::now_ms;
use super::membership::{EVENT_TYPE as MEMBERSHIP_TYPE, read_members};
use super::rings::{
    DECLINE_TYPE, Early, Incoming, Late, MAX_EVENT_BYTES, NOTIFICATION_TYPE, Rings, confirm,
    declined_event, screen,
};
use crate::session::SessionListener;
use matrix_sdk::{
    Client, Room, RoomState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedEventId, OwnedUserId,
        events::{AnySyncMessageLikeEvent, AnySyncStateEvent},
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Weak};

/// What the handlers need from the session. Held weakly: the SDK keeps
/// handlers as long as the client.
pub trait Env: Send + Sync {
    fn listener(&self) -> Option<Arc<dyn SessionListener>>;
    /// `Some(names)` while desktop notifications are on: whether they may
    /// name the room and the caller (ADR-0022).
    fn toast(&self) -> Option<bool>;
    /// The generation of the open session, or `None` while it is not open.
    /// A lock advances it, so a ring handled across a lock is dropped.
    fn open_generation(&self) -> Option<u64>;
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    event_id: OwnedEventId,
    sender: OwnedUserId,
    origin_server_ts: u64,
    content: Value,
}

/// Registers both handlers. The caller removes them with the client's
/// `remove_event_handler`.
pub fn register(client: &Client, env: Weak<dyn Env>) -> [EventHandlerHandle; 2] {
    let messages = {
        let env = env.clone();
        client.add_event_handler(move |raw: Raw<AnySyncMessageLikeEvent>, room: Room| {
            let env = env.clone();
            async move {
                if raw.json().get().len() > MAX_EVENT_BYTES {
                    return;
                }
                let Ok(event) = serde_json::from_str::<Envelope>(raw.json().get()) else {
                    return;
                };
                let Some(env) = env.upgrade() else {
                    return;
                };
                match event.kind.as_str() {
                    NOTIFICATION_TYPE => ring_arrived(&*env, &room, event).await,
                    DECLINE_TYPE => decline_arrived(&*env, &room, event),
                    _ => {}
                }
            }
        })
    };
    let members = client.add_event_handler(move |raw: Raw<AnySyncStateEvent>, room: Room| {
        let env = env.clone();
        async move {
            #[derive(Deserialize)]
            struct Kind {
                #[serde(rename = "type")]
                kind: String,
            }
            if !serde_json::from_str::<Kind>(raw.json().get())
                .is_ok_and(|k| k.kind == MEMBERSHIP_TYPE)
            {
                return;
            }
            let Some(env) = env.upgrade() else {
                return;
            };
            members_changed(&*env, &room).await;
        }
    });
    [messages, members]
}

/// A ring event: the checks, then the ring and its one notification.
async fn ring_arrived(env: &dyn Env, room: &Room, event: Envelope) {
    let Some(generation) = env.open_generation() else {
        return;
    };
    let still_open = || env.open_generation() == Some(generation);
    let Some(listener) = env.listener() else {
        return;
    };
    let Some(rings) = listener.rings() else {
        return;
    };
    let client = room.client();
    let now = now_ms();
    let incoming = Incoming {
        room_id: room.room_id().to_owned(),
        event_id: event.event_id,
        sender: event.sender,
        origin_server_ts: event.origin_server_ts,
        content: event.content,
    };
    let early = Early {
        room_joined: room.state() == RoomState::Joined,
        open_call_room: rings.is_open_room(room.room_id()),
        sender_is_self: client.user_id() == Some(&*incoming.sender),
    };
    let Ok(ring) = screen(&incoming, early, now) else {
        return;
    };
    // The sender bound first, so a flood costs no lookups.
    if rings.sender_allowed(&ring.sender, now).is_err() {
        return;
    }
    let late = Late {
        joined_member: joined_member(room, &ring.sender).await,
        ignored: match crate::abuse::ignored_set(&client).await {
            Ok(ignored) => ignored.contains(&ring.sender),
            // Unreadable ignore state refuses the ring.
            Err(_) => true,
        },
        // The lookups above, and the state read itself, may outlive a
        // membership, so its end is checked again after the read.
        in_call: match read_members(room, now_ms()).await {
            Ok(members) => sender_in_call(&members, &ring.sender, now_ms()),
            Err(_) => false,
        },
    };
    if confirm(late).is_err() || !still_open() {
        return;
    }
    let sender = ring.sender.clone();
    let event_id = ring.event_id.clone();
    if rings.admit(ring, now_ms()).is_err() {
        return;
    }
    // A lock between the check and the admission has cleared the rings
    // already, or clears them next; either way this ring must not stay.
    if !still_open() {
        rings.retain(|ring| ring.event_id != event_id);
        return;
    }
    listener.rings_changed();
    if let Some(allowed) = env.toast() {
        // Without names, none leaves the core. The lookup runs only when the
        // names are allowed.
        let (room_name, caller) = if allowed {
            crate::notifications::names(&client, room, &sender).await
        } else {
            (String::new(), String::new())
        };
        let (room_name, caller) = super::rings::toast_names(allowed, room_name, caller);
        if still_open() {
            listener.notify_call(&room_name, &caller);
        }
    }
}

/// Whether `sender` has a membership that is still current at `now`.
fn sender_in_call(
    members: &[super::membership::ActiveMember],
    sender: &OwnedUserId,
    now: u64,
) -> bool {
    members
        .iter()
        .any(|m| m.user_id == *sender && m.expires_ts > now)
}

async fn joined_member(room: &Room, sender: &OwnedUserId) -> bool {
    use matrix_sdk::ruma::events::room::member::MembershipState;
    match room.get_member(sender).await {
        Ok(Some(member)) => *member.membership() == MembershipState::Join,
        _ => false,
    }
}

/// A decline sent by this user from another device ends the ring here too.
fn decline_arrived(env: &dyn Env, room: &Room, event: Envelope) {
    if room.client().user_id() != Some(&*event.sender) {
        return;
    }
    let Some(notification) = declined_event(&event.content) else {
        return;
    };
    let Some(listener) = env.listener() else {
        return;
    };
    let Some(rings) = listener.rings() else {
        return;
    };
    if rings.retain(|ring| ring.event_id != notification) {
        listener.rings_changed();
    }
}

/// A call-membership change: this room's flag, and the rings whose sender
/// left the call.
async fn members_changed(env: &dyn Env, room: &Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    let Some(listener) = env.listener() else {
        return;
    };
    let (Some(rings), Some(call_rooms)) = (listener.rings(), listener.call_rooms()) else {
        return;
    };
    let client = room.client();
    let Ok(members) = read_members(room, now_ms()).await else {
        return;
    };
    let own = (client.user_id(), client.device_id());
    let until = members
        .iter()
        .filter(|m| (Some(&*m.user_id), Some(&*m.device_id)) != own)
        .map(|m| m.expires_ts)
        .max();
    if call_rooms.set(room.room_id(), until) {
        listener.room_call_changed(room.room_id().as_str());
    }
    if end_rings_of_missing_callers(rings, room, &members) {
        listener.rings_changed();
    }
}

fn end_rings_of_missing_callers(
    rings: &Rings,
    room: &Room,
    members: &[super::membership::ActiveMember],
) -> bool {
    rings.retain(|ring| {
        ring.room_id != room.room_id() || members.iter().any(|m| m.user_id == ring.sender)
    })
}

/// How long a decline or a ring send waits for the homeserver.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Sends the decline of notification `event_id` in `room_id` as a normal
/// room event; the SDK encrypts it in an encrypted room. Once: no retry.
/// Whether the homeserver accepted it.
pub async fn send_decline(
    client: &Client,
    room_id: &matrix_sdk::ruma::RoomId,
    event_id: &matrix_sdk::ruma::EventId,
) -> bool {
    let Some(room) = client.get_room(room_id) else {
        return false;
    };
    let content = super::rings::decline_content(event_id.as_str());
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, room.send_raw(DECLINE_TYPE, content)).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    //! These tests run a mocked `/sync` through the registered handlers, so
    //! the SDK delivers the events as in a live session.
    use super::*;
    use crate::calls::rings::{CallRooms, Intent, Ring, decline_content};
    use matrix_sdk::{
        SessionMeta, SessionTokens,
        authentication::matrix::MatrixSession,
        config::{RequestConfig, SyncSettings},
        ruma::{RoomId, event_id, room_id, user_id},
    };
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, path_regex},
    };

    const ROOM: &str = "!call:synthetic.test";
    const ME: &str = "@alice:synthetic.test";
    const CALLER: &str = "@bob:synthetic.test";
    const STRANGER: &str = "@carol:synthetic.test";

    #[derive(Default)]
    struct Recorder {
        rings: Rings,
        call_rooms: CallRooms,
        rings_changed: AtomicUsize,
        room_calls: Mutex<Vec<String>>,
        toasts: Mutex<Vec<(String, String)>>,
    }

    impl SessionListener for Recorder {
        fn rooms_changed(&self, _rooms: &[String]) {}
        fn session_changed(&self) {}
        fn rings(&self) -> Option<&Rings> {
            Some(&self.rings)
        }
        fn call_rooms(&self) -> Option<&CallRooms> {
            Some(&self.call_rooms)
        }
        fn rings_changed(&self) {
            self.rings_changed.fetch_add(1, Ordering::SeqCst);
        }
        fn room_call_changed(&self, room: &str) {
            self.room_calls.lock().unwrap().push(room.to_owned());
        }
        fn notify_call(&self, room: &str, sender: &str) {
            self.toasts
                .lock()
                .unwrap()
                .push((room.to_owned(), sender.to_owned()));
        }
    }

    struct TestEnv {
        listener: Arc<Recorder>,
        toast: Option<bool>,
        /// The generations the session reports, one for each ask; the last
        /// one repeats. `None` is a locked session.
        generations: Mutex<Vec<Option<u64>>>,
    }

    impl TestEnv {
        fn new(listener: Arc<Recorder>, toast: Option<bool>) -> Self {
            Self::with_generations(listener, toast, vec![Some(1)])
        }

        fn with_generations(
            listener: Arc<Recorder>,
            toast: Option<bool>,
            generations: Vec<Option<u64>>,
        ) -> Self {
            Self {
                listener,
                toast,
                generations: Mutex::new(generations),
            }
        }
    }

    impl Env for TestEnv {
        fn listener(&self) -> Option<Arc<dyn SessionListener>> {
            Some(self.listener.clone())
        }
        fn toast(&self) -> Option<bool> {
            self.toast
        }
        fn open_generation(&self) -> Option<u64> {
            let mut generations = self.generations.lock().unwrap();
            if generations.len() > 1 {
                generations.remove(0)
            } else {
                generations[0]
            }
        }
    }

    fn state_event(kind: &str, key: &str, sender: &str, content: Value) -> Value {
        json!({
            "type": kind, "state_key": key, "sender": sender, "content": content,
            "event_id": format!("${kind}-{}", key.len()), "origin_server_ts": now_ms()
        })
    }

    fn joined(user: &str, name: &str) -> Value {
        json!({
            "type": "m.room.member", "state_key": user, "sender": user,
            "content": { "membership": "join", "displayname": name },
            "event_id": format!("$join-{name}"), "origin_server_ts": 1
        })
    }

    /// A current call membership of `user` with device `device`.
    fn in_call(user: &str, device: &str) -> Value {
        let mut event = state_event(
            MEMBERSHIP_TYPE,
            &format!("_{user}_{device}_m.call"),
            user,
            json!({
                "application": "m.call", "scope": "m.room", "call_id": "",
                "device_id": device, "expires": 3_600_000, "created_ts": now_ms(),
                "foci_preferred": [{
                    "type": "livekit",
                    "livekit_service_url": "https://sfu.synthetic.test",
                    "livekit_alias": ROOM
                }],
                "focus_active": { "type": "livekit", "focus_selection": "oldest_membership" }
            }),
        );
        event["event_id"] = json!(format!("$call-{device}"));
        event
    }

    fn timeline_event(kind: &str, id: &str, sender: &str, content: Value) -> Value {
        json!({
            "type": kind, "event_id": id, "sender": sender,
            "origin_server_ts": now_ms(), "content": content
        })
    }

    fn ring_content() -> Value {
        json!({
            "notification_type": "ring", "sender_ts": now_ms(), "lifetime": 30_000,
            "m.call.intent": "video", "m.mentions": { "user_ids": [ME] }
        })
    }

    fn ring_event(id: &str, sender: &str) -> Value {
        timeline_event(NOTIFICATION_TYPE, id, sender, ring_content())
    }

    /// A sync with room `ROOM`, named "Planning", with Alice and Bob joined.
    fn sync_reply(state: Vec<Value>, timeline: Vec<Value>, ignored: &[&str]) -> Value {
        let mut all = vec![
            state_event("m.room.create", "", ME, json!({ "room_version": "11" })),
            state_event("m.room.name", "", ME, json!({ "name": "Planning" })),
            joined(ME, "Alice"),
            joined(CALLER, "Bob"),
        ];
        all.extend(state);
        let ignored: serde_json::Map<String, Value> = ignored
            .iter()
            .map(|user| ((*user).to_owned(), json!({})))
            .collect();
        json!({
            "next_batch": "s1",
            "account_data": { "events": [{
                "type": "m.ignored_user_list",
                "content": { "ignored_users": ignored }
            }]},
            "rooms": { "join": { ROOM: {
                "summary": { "m.joined_member_count": 2, "m.invited_member_count": 0 },
                "state": { "events": all },
                "timeline": { "events": timeline, "limited": false }
            }}}
        })
    }

    /// A signed-in client of a mocked homeserver, before its first sync.
    async fn client(server: &MockServer) -> Client {
        let routes = [
            (
                "GET",
                "/_matrix/client/versions",
                json!({ "versions": ["v1.11"] }),
            ),
            (
                "POST",
                "/_matrix/client/v3/keys/upload",
                json!({ "one_time_key_counts": { "signed_curve25519": 50 } }),
            ),
            (
                "POST",
                "/_matrix/client/v3/keys/query",
                json!({ "device_keys": {} }),
            ),
            (
                "GET",
                "/_matrix/client/v3/rooms/!call:synthetic.test/members",
                json!({ "chunk": [joined(ME, "Alice"), joined(CALLER, "Bob")] }),
            ),
        ];
        for (verb, route, body) in routes {
            Mock::given(method(verb))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(server)
                .await;
        }
        let client = Client::builder()
            .homeserver_url(server.uri())
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();
        client
            .restore_session(MatrixSession {
                meta: SessionMeta {
                    user_id: user_id!("@alice:synthetic.test").to_owned(),
                    device_id: "PERSEPHONE".into(),
                },
                tokens: SessionTokens {
                    access_token: "token".into(),
                    refresh_token: None,
                },
            })
            .await
            .unwrap();
        client
    }

    async fn sync(server: &MockServer, client: &Client, reply: Value) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .mount(server)
            .await;
        client.sync_once(SyncSettings::default()).await.unwrap();
    }

    /// Registers the handlers with a fresh recorder, which `prepare` may
    /// fill, then syncs `reply`.
    async fn run(
        reply: Value,
        toast: Option<bool>,
        prepare: impl FnOnce(&Recorder),
    ) -> Arc<Recorder> {
        run_with(reply, toast, prepare, TestEnv::new).await
    }

    async fn run_with(
        reply: Value,
        toast: Option<bool>,
        prepare: impl FnOnce(&Recorder),
        make_env: impl FnOnce(Arc<Recorder>, Option<bool>) -> TestEnv,
    ) -> Arc<Recorder> {
        let server = MockServer::builder().start().await;
        let client = client(&server).await;
        let listener = Arc::new(Recorder::default());
        prepare(&listener);
        let env: Arc<dyn Env> = Arc::new(make_env(listener.clone(), toast));
        let handles = register(&client, Arc::downgrade(&env));
        sync(&server, &client, reply).await;
        for handle in handles {
            client.remove_event_handler(handle);
        }
        listener
    }

    fn ring_from(sender: &str, event: &str) -> Ring {
        Ring {
            room_id: room_id!("!call:synthetic.test").to_owned(),
            event_id: OwnedEventId::try_from(event).unwrap(),
            sender: OwnedUserId::try_from(sender).unwrap(),
            intent: Intent::Audio,
            ends_ms: now_ms() + 60_000,
        }
    }

    fn live_ids(recorder: &Recorder) -> Vec<OwnedEventId> {
        recorder
            .rings
            .live(now_ms())
            .into_iter()
            .map(|ring| ring.event_id)
            .collect()
    }

    /// GIVEN a ring from a joined member who is in the call WHEN it syncs
    /// THEN it rings once, and the notification has names only when they are
    /// allowed.
    #[tokio::test]
    async fn given_a_ring_from_a_member_in_the_call_then_it_rings_and_notifies() {
        for (toast, expected) in [
            (Some(true), vec![("Planning".to_owned(), "Bob".to_owned())]),
            (Some(false), vec![(String::new(), String::new())]),
            (None, vec![]),
        ] {
            let reply = sync_reply(
                vec![in_call(CALLER, "BOB")],
                vec![ring_event("$ring", CALLER)],
                &[],
            );
            let recorder = run(reply, toast, |_| {}).await;
            let rings = recorder.rings.live(now_ms());
            assert_eq!(rings.len(), 1, "{toast:?}");
            assert_eq!(rings[0].event_id, event_id!("$ring"));
            assert_eq!(rings[0].intent, Intent::Video);
            assert!(recorder.rings_changed.load(Ordering::SeqCst) >= 1);
            assert_eq!(*recorder.toasts.lock().unwrap(), expected, "{toast:?}");
        }
    }

    /// GIVEN rings that fail a check WHEN they sync THEN none rings and no
    /// notification is made.
    #[tokio::test]
    async fn given_a_refused_ring_then_nothing_rings() {
        let mut padded = ring_content();
        padded["padding"] = json!("x".repeat(MAX_EVENT_BYTES));
        let malformed = json!({ "notification_type": "ring" });
        let cases: [(&str, Vec<Value>, Value, &[&str]); 6] = [
            ("not in the call", vec![], ring_event("$a", CALLER), &[]),
            (
                "own ring",
                vec![in_call(ME, "OTHER")],
                ring_event("$b", ME),
                &[],
            ),
            (
                "ignored caller",
                vec![in_call(CALLER, "BOB")],
                ring_event("$c", CALLER),
                &[CALLER],
            ),
            (
                "malformed",
                vec![in_call(CALLER, "BOB")],
                timeline_event(NOTIFICATION_TYPE, "$d", CALLER, malformed),
                &[],
            ),
            (
                "over the size bound",
                vec![in_call(CALLER, "BOB")],
                timeline_event(NOTIFICATION_TYPE, "$e", CALLER, padded),
                &[],
            ),
            (
                "not a joined member",
                vec![in_call(STRANGER, "CAROL")],
                ring_event("$f", STRANGER),
                &[],
            ),
        ];
        for (case, state, event, ignored) in cases {
            let recorder = run(sync_reply(state, vec![event], ignored), Some(true), |_| {}).await;
            assert!(live_ids(&recorder).is_empty(), "{case}");
            assert!(recorder.toasts.lock().unwrap().is_empty(), "{case}");
        }
    }

    /// GIVEN a locked session, or a lock while the ring is checked or its
    /// names are read WHEN a ring syncs THEN it is not kept and no
    /// notification is made.
    #[tokio::test]
    async fn given_a_lock_around_a_ring_then_it_is_dropped_without_a_notification() {
        // The generation is asked at the start, before and after admission,
        // and before the notification.
        for (case, generations, kept) in [
            ("locked", vec![None], false),
            ("lock before admission", vec![Some(1), Some(2)], false),
            ("lock at admission", vec![Some(1), Some(1), Some(2)], false),
            (
                "lock before the notification",
                vec![Some(1), Some(1), Some(1), None],
                true,
            ),
        ] {
            let reply = sync_reply(
                vec![in_call(CALLER, "BOB")],
                vec![ring_event("$ring", CALLER)],
                &[],
            );
            let recorder = run_with(
                reply,
                Some(true),
                |_| {},
                |listener, toast| TestEnv::with_generations(listener, toast, generations.clone()),
            )
            .await;
            // A real lock clears the kept ring; this one only stops the
            // notification.
            assert_eq!(!live_ids(&recorder).is_empty(), kept, "{case}");
            assert!(recorder.toasts.lock().unwrap().is_empty(), "{case}");
        }
    }

    /// GIVEN memberships read just before WHEN the sender's membership ended
    /// during the read THEN the sender is not in the call; another member's
    /// current membership does not count for the sender.
    #[test]
    fn given_a_membership_that_ended_during_the_read_then_the_sender_is_not_in_the_call() {
        let member = |user: &str, expires_ts: u64| super::super::membership::ActiveMember {
            user_id: OwnedUserId::try_from(user).unwrap(),
            device_id: "DEVICE".into(),
            identity: format!("{user}:DEVICE"),
            service_url: "https://sfu.synthetic.test".parse().unwrap(),
            joined_ts: 1_000,
            expires_ts,
        };
        let caller = OwnedUserId::try_from(CALLER).unwrap();
        let read_at = 10_000;
        let members = [
            member(CALLER, read_at + 5),
            member(STRANGER, read_at + 60_000),
        ];
        assert!(sender_in_call(&members, &caller, read_at));
        assert!(sender_in_call(&members, &caller, read_at + 4));
        assert!(!sender_in_call(&members, &caller, read_at + 5));
        assert!(!sender_in_call(&members[1..], &caller, read_at));
    }

    /// GIVEN a ring in the room of the open call WHEN it syncs THEN it does
    /// not ring.
    #[tokio::test]
    async fn given_a_ring_in_the_open_call_room_then_it_does_not_ring() {
        let reply = sync_reply(
            vec![in_call(CALLER, "BOB")],
            vec![ring_event("$ring", CALLER)],
            &[],
        );
        let recorder = run(reply, Some(true), |r| {
            r.rings
                .set_open_room(Some(room_id!("!call:synthetic.test")));
        })
        .await;
        assert!(live_ids(&recorder).is_empty());
        assert!(recorder.toasts.lock().unwrap().is_empty());
    }

    /// GIVEN a live ring WHEN a decline of it syncs THEN only a decline by
    /// this user, from another device, ends it here.
    #[tokio::test]
    async fn given_a_decline_then_only_this_users_own_ends_the_ring() {
        for (sender, ends) in [(ME, true), (CALLER, false)] {
            let decline = timeline_event(DECLINE_TYPE, "$no", sender, decline_content("$ring"));
            let reply = sync_reply(vec![in_call(CALLER, "BOB")], vec![decline], &[]);
            let recorder = run(reply, None, |r| {
                r.rings.admit(ring_from(CALLER, "$ring"), now_ms()).unwrap();
            })
            .await;
            assert_eq!(live_ids(&recorder).is_empty(), ends, "{sender}");
        }
    }

    /// GIVEN call memberships WHEN they sync THEN the room's call flag counts
    /// only other devices, and a ring whose sender is not in the call ends.
    #[tokio::test]
    async fn given_call_memberships_then_the_flag_is_set_and_rings_of_absent_callers_end() {
        let room = room_id!("!call:synthetic.test");
        // One ring per room, so each caller's ring needs its own run.
        let reply = sync_reply(vec![in_call(CALLER, "BOB")], vec![], &[]);
        let recorder = run(reply.clone(), None, |r| {
            r.rings
                .admit(ring_from(STRANGER, "$carol"), now_ms())
                .unwrap();
        })
        .await;
        assert!(recorder.call_rooms.active(room, now_ms()));
        assert_eq!(*recorder.room_calls.lock().unwrap(), vec![ROOM.to_owned()]);
        assert!(live_ids(&recorder).is_empty());
        assert_eq!(recorder.rings_changed.load(Ordering::SeqCst), 1);

        let recorder = run(reply, None, |r| {
            r.rings.admit(ring_from(CALLER, "$bob"), now_ms()).unwrap();
        })
        .await;
        assert_eq!(live_ids(&recorder), vec![event_id!("$bob").to_owned()]);
        assert_eq!(recorder.rings_changed.load(Ordering::SeqCst), 0);

        let reply = sync_reply(vec![in_call(ME, "PERSEPHONE")], vec![], &[]);
        let recorder = run(reply, None, |_| {}).await;
        assert!(!recorder.call_rooms.active(room, now_ms()));
        assert!(recorder.room_calls.lock().unwrap().is_empty());
    }

    /// GIVEN the session's environment has gone WHEN a ring and a membership
    /// sync THEN the handlers do nothing.
    #[tokio::test]
    async fn given_a_dropped_environment_then_the_handlers_do_nothing() {
        let server = MockServer::builder().start().await;
        let client = client(&server).await;
        let listener = Arc::new(Recorder::default());
        let env: Arc<dyn Env> = Arc::new(TestEnv::new(listener.clone(), Some(true)));
        let _handles = register(&client, Arc::downgrade(&env));
        drop(env);
        let reply = sync_reply(
            vec![in_call(CALLER, "BOB")],
            vec![ring_event("$ring", CALLER)],
            &[],
        );
        sync(&server, &client, reply).await;
        assert!(live_ids(&listener).is_empty());
        assert!(listener.room_calls.lock().unwrap().is_empty());
        assert_eq!(listener.rings_changed.load(Ordering::SeqCst), 0);
    }

    /// GIVEN a joined room WHEN a decline is sent THEN it is one PUT of the
    /// decline type and the result follows the homeserver's answer; an
    /// unknown room sends nothing.
    #[tokio::test]
    async fn given_a_decline_send_then_the_result_follows_the_homeserver() {
        for (status, accepted) in [(200, true), (500, false)] {
            let server = MockServer::builder().start().await;
            let client = client(&server).await;
            sync(&server, &client, sync_reply(vec![], vec![], &[])).await;
            Mock::given(method("GET"))
                .and(path_regex(r"/state/m\.room\.encryption"))
                .respond_with(
                    ResponseTemplate::new(404)
                        .set_body_json(json!({ "errcode": "M_NOT_FOUND", "error": "none" })),
                )
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path_regex(
                    r"/rooms/!call:synthetic\.test/send/org\.matrix\.msc4310\.rtc\.decline/",
                ))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(json!({ "event_id": "$sent" })),
                )
                .expect(1)
                .mount(&server)
                .await;
            let room = RoomId::parse(ROOM).unwrap();
            assert_eq!(
                send_decline(&client, &room, event_id!("$ring")).await,
                accepted,
                "{status}"
            );
            let unknown = room_id!("!other:synthetic.test");
            assert!(!send_decline(&client, unknown, event_id!("$ring")).await);
        }
    }
}
