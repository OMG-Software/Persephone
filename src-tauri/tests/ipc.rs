//! IPC permission tests against the shell's real configuration and compiled
//! capabilities, using Tauri's mock runtime (no window or web view).

use persephone_core_protocol::{Call, ErrorCode, Event, MAX_CALL_SOUND_LEVEL, MAX_ROOM_ID_BYTES};
use persephone_desktop::{
    BAND_PREFIX_BYTES, CALL_WINDOW, MAIN_WINDOW, Subscriptions, VIDEO_PREFIX_BYTES, configure,
    context,
    core_link::{Band, CoreError, CoreLink, CoreTransport, Video},
    create_call_window, create_main_window, reset_subscriptions,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder},
    webview::InvokeRequest,
};

/// Where a held core call says that it arrived, and where it waits to go on.
type Gate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

/// A device choice as recording, playout and camera; none is the default.
type DeviceChoice = (Option<String>, Option<String>, Option<String>);

/// Stands in for the core process. Counts calls so tests can show which
/// requests never reached it.
struct FakeCore {
    available: bool,
    calls: AtomicUsize,
    // The room and user of the last `verify_member` that reached the core.
    verified_member: Mutex<Option<(String, String)>>,
    // The room and bytes of the last pasted image that reached the core.
    pasted: Mutex<Option<(String, Vec<u8>)>>,
    // The room and event IDs of the last `timeline_events` that reached the core.
    timeline_events: Mutex<Option<(String, Vec<String>)>>,
    // What the stored account settings say about capture protection.
    capture_protection: AtomicBool,
    // The call statistics switches that reached the core, in order.
    stats_switches: Mutex<Vec<bool>>,
    // Refuses a statistics start, as the core does without a call.
    refuse_stats: AtomicBool,
    // When set, the next statistics switch says that it arrived, then waits
    // until the test lets it go.
    stats_gate: Mutex<Option<Gate>>,
    // The last device choice that reached the core.
    call_devices: Mutex<Option<DeviceChoice>>,
}

impl FakeCore {
    fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            available,
            calls: AtomicUsize::new(0),
            verified_member: Mutex::new(None),
            pasted: Mutex::new(None),
            timeline_events: Mutex::new(None),
            capture_protection: AtomicBool::new(true),
            stats_switches: Mutex::new(Vec::new()),
            refuse_stats: AtomicBool::new(false),
            stats_gate: Mutex::new(None),
            call_devices: Mutex::new(None),
        })
    }
}

impl CoreTransport for FakeCore {
    /// Return synthetic typed replies and count each request that reaches the fake core.
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Call::VerifyMember { room_id, user_id } = &call {
            *self.verified_member.lock().unwrap() = Some((room_id.clone(), user_id.clone()));
        }
        if let Call::CallStats { on } = &call {
            self.stats_switches.lock().unwrap().push(*on);
            let gate = self.stats_gate.lock().unwrap().take();
            if let Some((arrived, release)) = gate {
                let _ = arrived.send(());
                let _ = release.recv();
            }
            if *on && self.refuse_stats.load(Ordering::SeqCst) {
                return Err(CoreError::Core(ErrorCode::Unavailable));
            }
        }
        if let Call::SetCallDevices {
            recording,
            playout,
            camera,
        } = &call
        {
            *self.call_devices.lock().unwrap() =
                Some((recording.clone(), playout.clone(), camera.clone()));
        }
        if let Call::TimelineEvents { room_id, event_ids } = &call {
            *self.timeline_events.lock().unwrap() = Some((room_id.clone(), event_ids.clone()));
        }
        if !self.available {
            return Err(CoreError::Unavailable);
        }
        match call {
            Call::RoomDrafts {} => Ok(serde_json::json!(["!left:example.invalid"])),
            Call::RoomDraft { room_id } if room_id == "!bad:example.invalid" => {
                Ok(serde_json::json!({"body":"x","replyTo":null,"key":"secret"}))
            }
            Call::RoomDraft { .. } => {
                Ok(serde_json::json!({"body":"<script>inert</script>","replyTo":"$reply"}))
            }
            Call::RoomPreferences {} => Ok(
                serde_json::json!({"rooms":[{"roomId":"!r:example.invalid","favourite":true,"section":"Work"}]}),
            ),
            Call::JumpToDate { room_id, .. } | Call::EventContext { room_id, .. } => Ok(
                serde_json::json!({"page":{"roomId":room_id,"items":[],"nextCursor":null,"names":[]},"eventId":"$match"}),
            ),
            // A reply item with a field outside the contract must not reach the page.
            Call::TimelineEvents { room_id, .. } if room_id == "!leaky:s" => Ok(
                serde_json::json!({"roomId":room_id,"items":[],"nextCursor":null,"names":[],"accessToken":"x"}),
            ),
            Call::TimelineEvents { room_id, .. } => {
                Ok(serde_json::json!({"roomId":room_id,"items":[],"nextCursor":null,"names":[]}))
            }
            Call::PinnedMessages { room_id } => Ok(
                serde_json::json!({"roomId":room_id,"eventIds":["$match"],"items":[],"canManage":true,"truncated":false}),
            ),
            Call::Attachments { room_id, .. } => Ok(
                serde_json::json!({"roomId":room_id,"items":[],"nextCursor":null,"scanned":2,"partial":true}),
            ),
            Call::SetPinned { .. } => Ok(serde_json::Value::Null),
            Call::ConnectionHealth {} => Ok(
                serde_json::json!({"accountState":"signed_out","storeState":"protected","syncMode":"unknown","lastAttemptAt":null,"lastSuccessAt":null,"consecutiveFailures":0,"retryDelayMs":0,"lastError":null}),
            ),
            Call::SearchMessages { term, .. } => Ok(
                serde_json::json!({"results":[{"roomId":"!r:example.invalid","eventId":"$match","sender":"@bob:example.invalid","body":term}],"scanned":2,"truncated":true}),
            ),
            Call::IgnoredUsers {} => {
                Ok(serde_json::json!({"users":["@bob:example.invalid"],"truncated":false}))
            }
            Call::SaveRoomDraft { .. }
            | Call::SetRoomPreference { .. }
            | Call::SetIgnoredUser { .. }
            | Call::ReportAbuse { .. } => Ok(serde_json::Value::Null),
            Call::AppStatus {} => Ok(
                serde_json::json!({"version": "9.9.9", "revision": "0123456789ab", "milestone": "P1", "store": "key_missing", "account": {"state": "signed_out", "userId": null, "error": null, "presenceRefused": false}}),
            ),
            Call::RoomList {} => Ok(serde_json::json!({
                "rooms": [{"roomId": "!r:example.invalid", "displayName": "<b>x</b>", "unreadCount": 2, "encryption": "encrypted"}],
                "truncated": false
            })),
            Call::Subscribe { room_id } => match room_id.as_str() {
                "!r:example.invalid" | "!s:example.invalid" => Ok(serde_json::Value::Null),
                "!full:example.invalid" => Err(CoreError::Core(ErrorCode::LimitExceeded)),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::Unsubscribe { .. } => Ok(serde_json::Value::Null),
            Call::LockSession {} => Ok(serde_json::json!("locked")),
            // A state outside the contract must not reach the page.
            Call::UnlockSession {} => Ok(serde_json::json!("plaintext")),
            Call::ResetProfile {} => Ok(serde_json::json!("unlocking")),
            // This fake device's keys are never in a backup.
            Call::SignOut {
                discard_keys: false,
            } => Err(CoreError::Core(ErrorCode::KeysNotBackedUp)),
            Call::SignOut { discard_keys: true } => Ok(serde_json::json!("restart_required")),
            Call::SecurityStatus {} => Ok(
                serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false}),
            ),
            Call::CallStatus {} | Call::CallLeave {} => Ok(call_idle()),
            Call::CallRings {} => Ok(serde_json::json!({"rings": [
                {"ringId": 9, "roomId": "!r:example.invalid", "caller": "@bob:example.invalid", "intent": "video", "secondsLeft": 42}
            ]})),
            Call::CallDeclineRing { room_id } => match room_id.as_str() {
                "!r:example.invalid" => Ok(serde_json::json!({"rings": []})),
                // A ring with a field outside the contract must not reach the page.
                "!leaky:example.invalid" => Ok(serde_json::json!({"rings": [
                    {"ringId": 9, "roomId": "!r:example.invalid", "caller": "@bob:example.invalid", "intent": "audio", "secondsLeft": 1, "body": "hi"}
                ]})),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::CallReadiness { room_id } => match room_id.as_str() {
                "!r:example.invalid" => {
                    Ok(serde_json::json!({"available": true, "reason": null, "activeMembers": 1}))
                }
                // A reason outside the contract must not reach the page.
                "!odd:example.invalid" => Ok(
                    serde_json::json!({"available": false, "reason": "no_microphone", "activeMembers": 0}),
                ),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::CallJoin { room_id, video } => match room_id.as_str() {
                "!r:example.invalid" => Ok(
                    serde_json::json!({"phase": "joining", "roomId": room_id, "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": if video { "starting" } else { "off" }, "screen": "off", "video": []}),
                ),
                // A refused join is a status with a code, not an error.
                "!plain:example.invalid" => Ok(
                    serde_json::json!({"phase": "idle", "roomId": null, "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": "not_encrypted", "camera": "off", "screen": "off", "video": []}),
                ),
                // A reply with a field the contract does not have.
                _ => Ok(
                    serde_json::json!({"phase": "idle", "roomId": null, "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": [], "sfuUrl": "wss://x"}),
                ),
            },
            Call::CallCamera { on } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": if on { "starting" } else { "off" }, "screen": "off", "video": []}),
            ),
            Call::CallStats { .. } => Ok(serde_json::Value::Null),
            Call::CallScreen { on } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": "off", "screen": if on { "starting" } else { "off" }, "video": []}),
            ),
            // Answered with a picture, through `video_frame`; the UI never
            // sends it as an ordinary call.
            Call::CallVideoFrame { .. } => Err(CoreError::BadReply),
            Call::CallMute { muted } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": muted, "deafened": false, "volume": 100, "participants": [{"userId": "@bob:example.invalid", "deviceId": "DEV", "speaking": false, "reachable": true, "keyed": true, "sharing": false}], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": []}),
            ),
            Call::CallDeafen { deafened } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": deafened, "deafened": deafened, "volume": 100, "participants": [{"userId": "@bob:example.invalid", "deviceId": "DEV", "speaking": false, "reachable": true, "keyed": true, "sharing": false}], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": []}),
            ),
            Call::CallVolume { percent } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": false, "deafened": false, "volume": percent, "participants": [{"userId": "@bob:example.invalid", "deviceId": "DEV", "speaking": false, "reachable": true, "keyed": true, "sharing": false}], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": []}),
            ),
            Call::CallMicrophone { .. } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": []}),
            ),
            Call::CallVideoEffect { effect, .. } => Ok(
                serde_json::json!({"phase": "active", "roomId": "!r:example.invalid", "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": [], "effectRefused": effect.is_some_and(|effect| effect.background == persephone_contracts::CallBackground::Blur)}),
            ),
            Call::VerifyThisDevice {} => Ok(serde_json::json!({
                "identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false,
                "verification": {"flowId": "f1", "phase": "waiting", "emoji": [], "otherDevice": null}
            })),
            Call::AcceptVerification { flow_id }
            | Call::ConfirmVerification { flow_id }
            | Call::CancelVerification { flow_id } => Ok(serde_json::json!({
                "identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false,
                "verification": {"flowId": flow_id, "phase": "compare",
                    "emoji": [{"symbol": "🐶", "description": "Dog"}], "otherDevice": "<b>Laptop</b>"}
            })),
            Call::ContactStatus { room_id } => match room_id.as_str() {
                "!dm:example.invalid" => Ok(serde_json::json!({
                    "roomId": room_id, "userId": "<b>@bob</b>:example.invalid", "trust": "unverified"
                })),
                // A trust level outside the contract must not reach the page.
                "!odd:example.invalid" => Ok(serde_json::json!({
                    "roomId": room_id, "userId": "@bob:example.invalid", "trust": "trusted_anyway"
                })),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::IdentityChanges { room_id } => match room_id.as_str() {
                "!dm:example.invalid" => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false,
                    "changes": [{"userId": "@bob:example.invalid", "wasVerified": true}]
                })),
                // A reply with a field the contract does not have.
                _ => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false, "changes": [], "pinned": true
                })),
            },
            Call::LoadImage { event_id, .. } => match event_id.as_str() {
                "$image" => Ok(serde_json::json!({
                    "handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4
                })),
                "$bad" => Err(CoreError::Core(ErrorCode::Rejected)),
                "$busy" => Err(CoreError::Core(ErrorCode::LimitExceeded)),
                // A view carrying a field outside the contract.
                _ => Ok(serde_json::json!({
                    "handle": HANDLE, "width": 1, "height": 1, "bands": 1, "bandRows": 1,
                    "url": "mxc://example.invalid/m"
                })),
            },
            Call::LoadRoomPicture { room_id } => match room_id.as_str() {
                "!pic:example.invalid" => Ok(serde_json::json!({
                    "pictureId": "0123456789abcdef",
                    "image": {"handle": HANDLE, "width": 96, "height": 96, "bands": 1, "bandRows": 96}
                })),
                "!none:example.invalid" => Err(CoreError::Core(ErrorCode::NotFound)),
                "!bad:example.invalid" => Err(CoreError::Core(ErrorCode::Rejected)),
                // A picture that names where it is: outside the contract.
                _ => Ok(serde_json::json!({
                    "pictureId": "0123456789abcdef", "url": "mxc://example.invalid/m",
                    "image": {"handle": HANDLE, "width": 1, "height": 1, "bands": 1, "bandRows": 1}
                })),
            },
            // Answered with pixels, through `image_band`; the UI never sends
            // it as an ordinary call.
            Call::ImageBand { .. } => Err(CoreError::BadReply),
            Call::ReleaseImage { handle } | Call::DiscardImage { handle } => {
                if handle == HANDLE {
                    Ok(serde_json::Value::Null)
                } else {
                    Err(CoreError::Core(ErrorCode::NotFound))
                }
            }
            // Only `paste_image`, with its image frame, sends one.
            Call::PasteImage { .. } => Err(CoreError::BadReply),
            Call::PickImage { room_id } => match room_id.as_str() {
                "!r:example.invalid" => Ok(serde_json::json!({
                    "kind": "draft", "size": 48_213, "mime": "image/png",
                    "preview": {"handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4}
                })),
                "!cancel:example.invalid" => Ok(serde_json::json!({"kind": "cancelled"})),
                // A draft carrying the file's path is refused by the contract.
                "!path:example.invalid" => Ok(serde_json::json!({
                    "kind": "draft", "size": 1, "mime": "image/png", "path": "C:\\Users\\a\\x.png",
                    "preview": {"handle": HANDLE, "width": 1, "height": 1, "bands": 1, "bandRows": 1}
                })),
                _ => Err(CoreError::Core(ErrorCode::Unavailable)),
            },
            Call::SendImage { room_id, handle } => {
                let mut queued = serde_json::json!({
                    "localId": 3, "roomId": room_id, "kind": "image", "body": "", "state": "queued",
                    "eventId": null, "failure": null, "queuedAt": 1_767_258_000_000_u64
                });
                match (room_id.as_str(), handle == HANDLE) {
                    ("!full:example.invalid", _) => Err(CoreError::Core(ErrorCode::LimitExceeded)),
                    (_, true) => Ok(queued),
                    // A reply carrying the draft handle is refused by the contract.
                    (_, false) => {
                        queued["handle"] = handle.into();
                        Ok(queued)
                    }
                }
            }
            Call::AcceptIdentity { .. } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled", "operation": "waiting_for_confirmation",
                "error": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false, "verification": null
            })),
            Call::VerifyContact { .. } | Call::VerifyMember { .. } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled", "operation": "working", "error": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false,
                "verification": null
            })),
            Call::SetUpRecovery {} => Ok(
                serde_json::json!({"identity": "verified", "recovery": "disabled", "operation": "working", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false}),
            ),
            Call::SetUpEncryption {} => Ok(
                serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "waiting_for_password", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false}),
            ),
            // A reply carrying a secret is refused by the contract.
            Call::SendMessage { room_id, body, .. } | Call::EditMessage { room_id, body, .. } => {
                match body.as_str() {
                    "hello" => Ok(serde_json::json!({
                        "localId": 1, "roomId": room_id, "kind": "text", "body": body, "state": "queued",
                        "eventId": null, "failure": null, "queuedAt": 1_767_258_000_000_u64
                    })),
                    // A reply carrying the transaction ID is refused by the contract.
                    _ => Ok(serde_json::json!({
                        "localId": 2, "roomId": room_id, "kind": "text", "body": body, "state": "queued",
                        "eventId": null, "failure": null, "queuedAt": 1_767_258_000_000_u64, "transactionId": "t"
                    })),
                }
            }
            Call::PendingMessages { .. } => Ok(serde_json::json!([])),
            Call::RetryMessage { local_id } | Call::DiscardMessage { local_id } => match local_id {
                1 => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::AllowUnencrypted { .. } => Ok(serde_json::Value::Null),
            Call::RecoverWithKey {} => Ok(
                serde_json::json!({"identity": "unverified", "recovery": "enabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false, "recoveryKey": "x"}),
            ),
            Call::Register { username, .. } => match username.as_str() {
                "taken" => Ok(
                    serde_json::json!({"state": "sign_in_failed", "userId": null, "error": "username_taken", "presenceRefused": false}),
                ),
                // A reason outside the contract is refused.
                "odd" => Ok(
                    serde_json::json!({"state": "sign_in_failed", "userId": null, "error": "password_was_hunter2", "presenceRefused": false}),
                ),
                _ => Ok(
                    serde_json::json!({"state": "signing_in", "userId": null, "error": null, "presenceRefused": false}),
                ),
            },
            Call::SignInWithOidc { .. } => Ok(
                serde_json::json!({"state": "signing_in", "userId": null, "error": null, "presenceRefused": false}),
            ),
            Call::SignIn { homeserver } => match homeserver.as_str() {
                "https://matrix.example.org" => Ok(
                    serde_json::json!({"state": "signing_in", "userId": null, "error": null, "presenceRefused": false}),
                ),
                // A reply carrying a secret is refused by the contract.
                _ => Ok(
                    serde_json::json!({"state": "signed_in", "userId": "@a:s", "error": null, "presenceRefused": false, "accessToken": "x"}),
                ),
            },
            Call::TimelinePage { room_id, cursor } => match (room_id.as_str(), cursor.as_deref()) {
                ("!r:example.invalid", None) => Ok(serde_json::json!({
                    "roomId": room_id,
                    "items": [{"kind": "message", "event_id": "$1", "sender": "@a:example.invalid", "body": "hi", "origin_server_ts": 1_767_258_000_000_u64}],
                    "nextCursor": null,
                    "names": [{"userId": "@a:example.invalid", "displayName": "A"}]
                })),
                // A reply with a field the contract does not have.
                ("!bad-reply:example.invalid", _) => Ok(serde_json::json!({
                    "roomId": room_id, "items": [], "nextCursor": null, "names": [], "accessToken": "x"
                })),
                (_, Some(_)) => Err(CoreError::Core(ErrorCode::InvalidArgument)),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::InviteList {} => Ok(serde_json::json!({
                "invites": [
                    {"roomId": "!inv:example.invalid", "roomName": null, "direct": true, "inviter": "@bob:example.invalid", "inviterName": "<b>Bob</b>"},
                    {"roomId": "!fed:remote.invalid", "roomName": "Plans", "direct": false, "inviter": null, "inviterName": null}
                ],
                "truncated": false
            })),
            Call::MarkRead { .. } => Ok(serde_json::Value::Null),
            Call::PickFile { .. } => Ok(serde_json::Value::Null),
            Call::SaveFile { event_id, .. } => Ok(serde_json::Value::Bool(event_id == "$f")),
            Call::RoomReaders { .. } => Ok(serde_json::json!({
                "readers": [{"userId": "@b:example.invalid", "trust": "verified",
                    "receiving": 2, "refused": 1, "own": false}],
                "truncated": false
            })),
            Call::RoomServers { .. } => Ok(serde_json::json!({
                "servers": [{"name": "example.invalid", "members": 2, "own": true}],
                "truncated": false
            })),
            Call::OpenLink { url } => Ok(serde_json::Value::Bool(url == "https://example.org/")),
            Call::RoomActivity { .. } => Ok(serde_json::json!({
                "typing": ["@b:example.invalid"], "readBy": [], "truncated": false
            })),
            Call::SetTyping { typing, .. } => match typing {
                true => Ok(serde_json::Value::Null),
                false => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::React { key, .. } => match key.as_str() {
                "👍" => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::DeleteMessage { event_id, .. } => match event_id.as_str() {
                "$mine" => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::AcceptInvite { room_id } | Call::DeclineInvite { room_id } => {
                match room_id.as_str() {
                    "!inv:example.invalid" => Ok(serde_json::Value::Null),
                    _ => Err(CoreError::Core(ErrorCode::NotFound)),
                }
            }
            Call::StartConversation { user_id } => match user_id.as_str() {
                "@bob:example.invalid" => {
                    Ok(serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"}))
                }
                "@nokeys:example.invalid" => Ok(serde_json::json!({"kind": "no_identity"})),
                // An outcome outside the contract must not reach the page.
                _ => Ok(
                    serde_json::json!({"kind": "created_unencrypted", "roomId": "!x:example.invalid"}),
                ),
            },
            Call::SearchUsers { term } => match term.as_str() {
                "bob" => Ok(serde_json::json!({
                    "users": [{"userId": "@bob:example.invalid", "displayName": "Bob"}],
                    "truncated": false
                })),
                // A match carrying a field the contract does not have.
                _ => Ok(serde_json::json!({
                    "users": [{"userId": "@a:example.invalid", "displayName": null, "avatarUrl": "mxc://x/y"}],
                    "truncated": false
                })),
            },
            Call::PublicRooms {
                server,
                filter,
                since,
            } => match (server.as_deref(), filter.as_deref(), since.as_deref()) {
                (None, None, None) => Ok(serde_json::json!({
                    "rooms": [{
                        "roomId": "!pub:example.invalid",
                        "name": "<b>Lobby</b>",
                        "alias": "#lobby:example.invalid",
                        "topic": null,
                        "members": 3,
                        "worldReadable": true,
                        "guestsCanJoin": false,
                        "joinable": true,
                        "joined": false
                    }],
                    "next": "p1",
                    "total": 1
                })),
                (Some("other.invalid"), Some("rust"), Some("p1")) => {
                    Ok(serde_json::json!({"rooms": [], "next": null, "total": null}))
                }
                // A room carrying an avatar the contract does not have.
                _ => Ok(serde_json::json!({
                    "rooms": [{
                        "roomId": "!a:example.invalid",
                        "name": null,
                        "alias": null,
                        "topic": null,
                        "members": 1,
                        "worldReadable": false,
                        "guestsCanJoin": false,
                        "joinable": true,
                        "joined": false,
                        "avatarUrl": "mxc://x/y"
                    }],
                    "next": null,
                    "total": null
                })),
            },
            Call::JoinPublicRoom { room_id } => match room_id.as_str() {
                "!pub:example.invalid" => Ok(serde_json::json!({"roomId": room_id})),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::CreateRoom { name, invite } => match (name.as_str(), invite.as_slice()) {
                ("Plans", [user]) if user == "@bob:example.invalid" => {
                    Ok(serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"}))
                }
                ("Plans", [user]) => Ok(serde_json::json!({"kind": "no_identity", "userId": user})),
                // An outcome outside the contract must not reach the page.
                _ => {
                    Ok(serde_json::json!({"kind": "created", "roomId": "!x:s", "encrypted": false}))
                }
            },
            Call::InviteUser { user_id, .. } => match user_id.as_str() {
                "@bob:example.invalid" => Ok(serde_json::json!({"kind": "invited"})),
                _ => Ok(serde_json::json!({"kind": "refused"})),
            },
            Call::DisplayName {} => Ok(serde_json::json!({"displayName": "<b>Al</b>"})),
            Call::SetDisplayName { name } => match name.as_str() {
                "" => Ok(serde_json::json!({"displayName": null})),
                "Al" => Ok(serde_json::json!({"displayName": "Al"})),
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::AccountSettings {} => Ok(serde_json::json!({
                "enterSends": false, "chatTextPercent": 120, "interfaceTextPercent": 100,
                "desktopNotifications": true, "sendTyping": false, "sendReadReceipts": false,
                "idleLockMinutes": 0, "notificationNames": true, "loadRoomPictures": false,
                "captureProtection": self.capture_protection.load(Ordering::SeqCst),
                "timelineEvents": all_timeline_events(),
                "theme": "red", "sharePresence": true, "presence": "online"
            })),
            Call::SetAccountSettings { settings } => match settings.enter_sends {
                true => Ok(serde_json::Value::Null),
                false => Err(CoreError::Core(ErrorCode::Locked)),
            },
            Call::DiscoverHomeserver { server_name } => match server_name.as_str() {
                "example.invalid" => Ok(serde_json::json!({
                    "homeserver": "https://matrix.example.invalid/", "fromWellKnown": true
                })),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
            Call::ProxySetting {} => Ok(serde_json::json!({"proxy": "127.0.0.1:9050"})),
            Call::DiagnosticsStatus {} => Ok(serde_json::json!({
                "on": false, "full": false, "failed": false, "available": true, "path": null
            })),
            Call::SetDiagnostics { on } => Ok(serde_json::Value::Bool(on)),
            // A page's records always arrive as the page's.
            Call::DiagnosticRecords { source, .. } => match source {
                persephone_core_protocol::RecordSource::Page => Ok(serde_json::Value::Null),
                persephone_core_protocol::RecordSource::Ui => {
                    Err(CoreError::Core(ErrorCode::InvalidArgument))
                }
            },
            Call::SetProxySetting { proxy } => match proxy.as_deref() {
                None | Some("127.0.0.1:9050") => Ok(serde_json::Value::Null),
                Some(_) => Err(CoreError::Core(ErrorCode::InvalidArgument)),
            },
            Call::CallDevices {} => Ok(serde_json::json!({
                "supported": true,
                "recording": [{"id": "mic-1", "name": "Headset"}],
                "playout": [],
                "cameras": [{"id": "cam-1", "name": "Webcam"}],
                "selectedRecording": null,
                "selectedPlayout": null,
                "selectedCamera": null,
                "unreadable": false,
                "volume": 100,
                "autoLevel": true,
                "micGain": 100,
                "effect": {"background": "none", "blur": 50, "filter": "none"},
                "mirror": false,
                "backgroundEffects": false
            })),
            Call::SetCallDevices {
                recording,
                playout,
                camera,
            } => match (recording.as_deref(), playout.as_deref(), camera.as_deref()) {
                (None | Some("mic-1"), None | Some("spk-1"), None | Some("cam-1")) => {
                    Ok(serde_json::Value::Null)
                }
                _ => Err(CoreError::Core(ErrorCode::InvalidArgument)),
            },
            Call::CallDeviceTest { test, .. } => Ok(serde_json::json!({
                "id": 3, "test": test, "state": "starting", "stream": 0
            })),
            Call::CallDeviceTestStop { id: None | Some(3) } => Ok(serde_json::Value::Null),
            Call::CallDeviceTestStop { .. } => Err(CoreError::Core(ErrorCode::InvalidArgument)),
            // A state outside the contract must not reach the page.
            Call::CallDeviceTestStatus {} => Ok(serde_json::json!({
                "id": 0, "test": "camera", "state": "exploded", "stream": 0
            })),
            Call::NewSessions {} => Ok(serde_json::json!({
                "sessions": [{"deviceId": "NEWDEV", "displayName": null}], "truncated": false
            })),
            Call::AcknowledgeSessions { .. } => Ok(serde_json::Value::Null),
            Call::ThirdPartyModules {} | Call::AcknowledgeModules { .. } => Ok(serde_json::json!({
                "state": "ready",
                "scannedAt": 1_700_000_000_000_u64,
                "modules": [{
                    "id": "hook.dll\u{1f}unsigned",
                    "fileName": "<b>hook.dll</b>",
                    "path": "C:\\Tools\\hook.dll",
                    "signer": null,
                    "signature": "unsigned",
                    "reason": "signer",
                    "processes": ["core", "web_view"],
                    "firstSeen": 1_700_000_000_000_u64,
                    "acknowledged": false
                }],
                "truncated": false,
                "incomplete": false
            })),
            Call::RemoveDevice { device_id } => match device_id.as_str() {
                "NEWDEV" => Ok(serde_json::json!("done")),
                _ => Ok(serde_json::json!("invalid_password")),
            },
            Call::ChangePassword { sign_out_others } => match sign_out_others {
                true => Ok(serde_json::json!("done")),
                // A code outside the contract must not reach the page.
                false => Ok(serde_json::json!("password_was_hunter2")),
            },
            Call::ThreePids {} => Ok(serde_json::json!({
                "threePids": [{"medium": "email", "address": "<b>a</b>@example.invalid"}],
                "truncated": false, "canChange": true, "pending": null
            })),
            Call::RequestEmailToken { email } => match email.as_str() {
                "a@example.invalid" => Ok(serde_json::json!("token_sent")),
                _ => Ok(serde_json::json!("medium_not_supported")),
            },
            Call::RequestPhoneToken { .. } => Ok(serde_json::json!("token_sent")),
            Call::AddThreePid {} => Ok(serde_json::json!("not_confirmed")),
            Call::RemoveThreePid { .. } => Ok(serde_json::json!("done")),
            Call::VerifyDevice { device_id } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled", "operation": "working",
                "error": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false,
                "verification": {"flowId": device_id, "phase": "waiting", "emoji": [],
                    "otherDevice": null, "contact": null}
            })),
            Call::ChangeRecoveryKey {} => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled",
                "operation": "waiting_for_confirmation", "error": null,
                "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false
            })),
            Call::SetProfilePassphrase { on } => Ok(serde_json::json!({
                "identity": "verified", "recovery": "enabled",
                "operation": "waiting_for_passphrase", "error": null,
                "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": !on
            })),
            Call::StartNewBackup {} => Ok(serde_json::json!({
                "identity": "verified", "recovery": "incomplete",
                "operation": "waiting_for_confirmation", "error": null,
                "verification": null, "canSetUpRecovery": false, "canStartNewBackup": true, "profilePassphrase": false
            })),
            Call::NotificationLevel {} => Ok(serde_json::json!({"level": "custom"})),
            Call::SetNotificationLevel { .. } => Ok(serde_json::Value::Null),
            Call::DeviceList {} => Ok(serde_json::json!({
                "devices": [{
                    "deviceId": "ME",
                    "displayName": "<i>Laptop</i>",
                    "lastSeen": 1_767_258_000_000u64,
                    "current": true,
                    "verified": true
                }],
                "truncated": false
            })),
            Call::ProfileList {} => Ok(serde_json::json!({
                "profiles": [{"slot": 1, "active": true}, {"slot": 2, "active": false}],
                "canAdd": true
            })),
            Call::RoomSettings { room_id } => {
                let mut settings = serde_json::json!({
                    "roomId": room_id, "name": "<b>Plans</b>", "topic": "a\nb",
                    "canonicalAlias": "#plans:example.invalid", "altAliases": [],
                    "localAliases": null, "joinRule": "invite", "historyVisibility": "shared",
                    "guestAccess": false, "encryption": "encrypted", "published": null,
                    "roomVersion": "11", "direct": false, "notifications": "default",
                    "levels": [{"key": "ban", "level": 50}], "ownLevel": null,
                    "may": {"name": true, "topic": true, "canonicalAlias": true,
                        "joinRules": true, "historyVisibility": true, "guestAccess": true,
                        "encryption": false, "powerLevels": true, "invite": true,
                        "kick": true, "ban": true}
                });
                match room_id.as_str() {
                    "!r:example.invalid" => Ok(settings),
                    // A level JavaScript cannot hold exactly is refused.
                    "!big:example.invalid" => {
                        settings["levels"][0]["level"] = 9_007_199_254_740_992_u64.into();
                        Ok(settings)
                    }
                    // A field outside the contract is refused.
                    "!extra:example.invalid" => {
                        settings["serverAcl"] = serde_json::json!({"allow": ["*"]});
                        Ok(settings)
                    }
                    _ => Err(CoreError::Core(ErrorCode::NotFound)),
                }
            }
            Call::RoomMembers { room_id, filter } => match filter.as_str() {
                "" => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false,
                    "members": [{"userId": "@bob:example.invalid", "displayName": "<i>Bob</i>",
                        "membership": "join", "level": 50, "own": false, "canKick": true,
                        "canBan": true, "canUnban": false, "canChangeLevel": true}]
                })),
                // A membership outside the contract is refused.
                _ => Ok(serde_json::json!({
                    "roomId": room_id, "truncated": false,
                    "members": [{"userId": "@bob:example.invalid", "displayName": null,
                        "membership": "leave", "level": 0, "own": false, "canKick": false,
                        "canBan": false, "canUnban": false, "canChangeLevel": false}]
                })),
            },
            Call::ChangeRoom { room_id, change } => match (room_id.as_str(), change) {
                ("!r:example.invalid", _) => Ok(serde_json::Value::Null),
                (_, persephone_contracts::RoomChange::Leave {}) => {
                    Err(CoreError::Core(ErrorCode::NotFound))
                }
                _ => Err(CoreError::Core(ErrorCode::Rejected)),
            },
            Call::AddProfile {} => Ok(serde_json::Value::Null),
            Call::SwitchProfile { slot } => match slot {
                2 => Ok(serde_json::Value::Null),
                _ => Err(CoreError::Core(ErrorCode::NotFound)),
            },
        }
    }

    /// Band 0 of [`HANDLE`] is 4 rows of 3 pixels, band 1 the last row.
    fn image_band(&self, handle: String, band: u32) -> Result<Band, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.available {
            return Err(CoreError::Unavailable);
        }
        if handle != HANDLE {
            return Err(CoreError::Core(ErrorCode::NotFound));
        }
        let rows = match band {
            0 => 4,
            1 => 1,
            _ => return Err(CoreError::Core(ErrorCode::InvalidArgument)),
        };
        let pixels = (0..3 * rows * 4).map(|byte| byte as u8).collect();
        Ok(Band {
            width: 3,
            rows,
            pixels,
        })
    }

    /// Tile 0 has picture 5 (2 by 1 pixels, one quarter turn), tile 1 has
    /// picture 7, tile 2 has none, tile 3 is busy and tile 4 gives pixels of
    /// the wrong tile. Every picture belongs to stream 1; another stream has
    /// none.
    fn video_frame(&self, tile: u8, after: u64, stream: u64) -> Result<Option<Video>, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.available {
            return Err(CoreError::Unavailable);
        }
        if stream != 1 && tile < 3 {
            return Ok(None);
        }
        let seq = match tile {
            0 => 5,
            1 => 7,
            2 => return Ok(None),
            3 => return Err(CoreError::Core(ErrorCode::LimitExceeded)),
            _ => return Err(CoreError::BadReply),
        };
        if seq <= after {
            return Ok(None);
        }
        Ok(Some(Video {
            tile,
            seq,
            rotation: 1,
            width: 2,
            height: 1,
            pixels: (0..8).collect(),
        }))
    }

    fn paste_image(&self, room_id: String, bytes: Vec<u8>) -> Result<serde_json::Value, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.available {
            return Err(CoreError::Unavailable);
        }
        let draft = room_id == "!r:example.invalid";
        *self.pasted.lock().unwrap() = Some((room_id, bytes));
        if draft {
            Ok(serde_json::json!({
                "kind": "draft", "size": 4, "mime": "image/png",
                "preview": {"handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4}
            }))
        } else {
            Err(CoreError::Core(ErrorCode::NotFound))
        }
    }
}

/// Every timeline switch on, in its wire form; the contracts crate checks
/// the spelling of each.
fn all_timeline_events() -> serde_json::Value {
    serde_json::to_value(persephone_contracts::TimelineEvents::default()).unwrap()
}

/// The call status when no call exists.
fn call_idle() -> serde_json::Value {
    serde_json::json!({"phase": "idle", "roomId": null, "muted": false, "deafened": false, "volume": 100, "participants": [], "ended": null, "joinError": null, "camera": "off", "screen": "off", "video": [], "ringDeclined": false})
}

/// A well-formed image handle the fake core knows.
const HANDLE: &str = "0123456789abcdef0123456789abcdef";

fn app_with(core: CoreLink) -> tauri::App<MockRuntime> {
    configure(mock_builder(), core, Arc::default())
        .build(context())
        .expect("shell builds on the mock runtime")
}

fn app() -> tauri::App<MockRuntime> {
    app_with(FakeCore::new(true))
}

fn app_origin() -> &'static str {
    if cfg!(windows) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    }
}

fn request(cmd: &str, origin: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: origin.parse().unwrap(),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

fn call(
    cmd: &str,
    origin: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    let app = app();
    let window = create_main_window(&app).expect("main window builds");
    assert_eq!(window.label(), MAIN_WINDOW);
    get_ipc_response(&window, request(cmd, origin, body))
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
}

#[test]
fn given_allowed_command_from_app_origin_then_typed_status_is_returned() {
    let reply = call("app_status", app_origin(), serde_json::json!({})).unwrap();
    // The reply is the core's answer, relayed and shape-checked by the UI.
    assert_eq!(
        reply,
        serde_json::json!({"version": "9.9.9", "revision": "0123456789ab", "milestone": "P1", "store": "key_missing", "account": {"state": "signed_out", "userId": null, "error": null, "presenceRefused": false}})
    );
}

#[test]
fn given_unregistered_command_then_it_is_rejected() {
    for cmd in ["app_statuses", "get_secrets", "", "APP_STATUS"] {
        assert!(
            call(cmd, app_origin(), serde_json::json!({})).is_err(),
            "{cmd:?} was accepted"
        );
    }
}

/// None of Tauri's built-in core plugins is granted, so a compromised page
/// cannot open windows, listen to events, resolve paths or change the webview.
#[test]
fn given_core_plugin_commands_then_every_one_is_denied() {
    for cmd in [
        "plugin:app|version",
        "plugin:app|app_show",
        "plugin:event|listen",
        "plugin:event|emit",
        "plugin:path|resolve_directory",
        "plugin:window|close",
        "plugin:window|create",
        "plugin:window|set_title",
        "plugin:webview|create_webview_window",
        "plugin:webview|print",
        "plugin:image|from_bytes",
        "plugin:resources|close",
        "plugin:menu|new",
        "plugin:tray|new",
        // ADR-0022: the notification plugin is used from Rust only.
        "plugin:notification|notify",
        "plugin:notification|request_permission",
        "plugin:notification|is_permission_granted",
        "plugin:notification|register_action_types",
        "plugin:notification|register_listener",
        "plugin:notification|remove_active",
    ] {
        assert!(
            call(cmd, app_origin(), serde_json::json!({})).is_err(),
            "{cmd} was allowed"
        );
    }
}

/// The capability is local-only: the same allowed command from a remote
/// origin must be refused.
#[test]
fn given_allowed_command_from_remote_origin_then_it_is_rejected() {
    for origin in [
        "https://example.org",
        "http://tauri.localhost.example.org",
        "http://localhost:1420",
    ] {
        assert!(
            call("app_status", origin, serde_json::json!({})).is_err(),
            "{origin} was accepted"
        );
    }
}

#[test]
fn given_wrong_invoke_key_then_the_request_is_dropped_unanswered() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let app = app();
    let window = create_main_window(&app).unwrap();
    let mut forged = request("app_status", app_origin(), serde_json::json!({}));
    forged.invoke_key = "forged-key".into();
    let answered = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&answered);
    // Tauri checks the key first and returns without calling the responder.
    window.as_ref().clone().on_message(
        forged,
        Box::new(move |_, _, _, _, _| flag.store(true, Ordering::SeqCst)),
    );
    assert!(
        !answered.load(Ordering::SeqCst),
        "forged request was answered"
    );
    // Control: the same command with the real key is answered.
    assert!(
        get_ipc_response(
            &window,
            request("app_status", app_origin(), serde_json::json!({}))
        )
        .is_ok()
    );
}

/// Check the shell configuration keeps restrictive capabilities and window policies.
#[test]
fn given_shell_config_then_security_settings_are_restrictive() {
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let app = &config["app"];
    assert_eq!(app["withGlobalTauri"], false);
    assert_eq!(app["windows"], serde_json::json!([]));
    assert_eq!(app["security"]["freezePrototype"], true);
    assert_eq!(
        app["security"]["capabilities"],
        serde_json::json!(["main", "call"])
    );
    assert!(
        app["security"]
            .get("dangerousDisableAssetCspModification")
            .is_none()
    );
    assert!(app["security"].get("pattern").is_none());
    assert!(
        config["build"].get("devUrl").is_none(),
        "no development server"
    );
    assert!(config["build"].get("beforeDevCommand").is_none());
    assert_eq!(config["bundle"]["active"], false);

    let csp = app["security"]["csp"].as_object().unwrap();
    assert_eq!(csp["default-src"], "'none'");
    for (directive, sources) in csp {
        let sources = sources.as_str().unwrap();
        for forbidden in [
            "'unsafe-inline'",
            "'unsafe-eval'",
            "*",
            "data:",
            "blob:",
            "https:",
            "http:",
        ] {
            assert!(
                !sources.split_whitespace().any(|source| source == forbidden),
                "{directive} allows {forbidden}"
            );
        }
    }

    let capability: serde_json::Value =
        serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
    assert_eq!(capability["windows"], serde_json::json!([MAIN_WINDOW]));
    assert_eq!(
        capability["permissions"],
        // A plain vector: `json!` with this many items hits the macro
        // recursion limit.
        serde_json::Value::from(vec![
            "allow-app-status",
            "allow-room-list",
            "allow-room-draft",
            "allow-room-drafts",
            "allow-save-room-draft",
            "allow-room-preferences",
            "allow-set-room-preference",
            "allow-search-messages",
            "allow-jump-to-date",
            "allow-event-context",
            "allow-timeline-events",
            "allow-pinned-messages",
            "allow-set-pinned",
            "allow-attachments",
            "allow-connection-health",
            "allow-ignored-users",
            "allow-set-ignored-user",
            "allow-report-abuse",
            "allow-timeline-page",
            "allow-subscribe-room",
            "allow-unsubscribe-room",
            "allow-watch-session",
            "allow-lock-session",
            "allow-unlock-session",
            "allow-reset-profile",
            "allow-sign-in",
            "allow-sign-in-with-oidc",
            "allow-discover-homeserver",
            "allow-proxy-setting",
            "allow-set-proxy-setting",
            "allow-diagnostics-status",
            "allow-set-diagnostics",
            "allow-diagnostic-records",
            "allow-call-devices",
            "allow-set-call-devices",
            "allow-call-device-test",
            "allow-call-device-test-stop",
            "allow-call-device-test-status",
            "allow-register",
            "allow-sign-out",
            "allow-security-status",
            "allow-call-readiness",
            "allow-call-join",
            "allow-call-leave",
            "allow-call-mute",
            "allow-call-deafen",
            "allow-call-volume",
            "allow-call-microphone",
            "allow-call-video-effect",
            "allow-call-camera",
            "allow-call-screen",
            "allow-call-video-frame",
            "allow-call-status",
            "allow-call-rings",
            "allow-call-decline-ring",
            "allow-open-call-window",
            "allow-close-call-window",
            "allow-set-up-encryption",
            "allow-recover-with-key",
            "allow-send-message",
            "allow-edit-message",
            "allow-delete-message",
            "allow-react",
            "allow-mark-read",
            "allow-room-activity",
            "allow-set-typing",
            "allow-open-link",
            "allow-room-servers",
            "allow-room-readers",
            "allow-new-sessions",
            "allow-acknowledge-sessions",
            "allow-third-party-modules",
            "allow-acknowledge-modules",
            "allow-display-session",
            "allow-remove-device",
            "allow-pending-messages",
            "allow-retry-message",
            "allow-discard-message",
            "allow-allow-unencrypted",
            "allow-set-up-recovery",
            "allow-verify-this-device",
            "allow-accept-verification",
            "allow-confirm-verification",
            "allow-cancel-verification",
            "allow-contact-status",
            "allow-verify-contact",
            "allow-verify-member",
            "allow-identity-changes",
            "allow-accept-identity",
            "allow-load-image",
            "allow-load-room-picture",
            "allow-image-band",
            "allow-release-image",
            "allow-pick-image",
            "allow-paste-image",
            "allow-pick-file",
            "allow-save-file",
            "allow-send-image",
            "allow-discard-image",
            "allow-invite-list",
            "allow-accept-invite",
            "allow-decline-invite",
            "allow-start-conversation",
            "allow-search-users",
            "allow-public-rooms",
            "allow-join-public-room",
            "allow-create-room",
            "allow-invite-user",
            "allow-room-settings",
            "allow-room-members",
            "allow-change-room",
            "allow-display-name",
            "allow-set-display-name",
            "allow-device-list",
            "allow-account-settings",
            "allow-set-account-settings",
            "allow-change-password",
            "allow-three-pids",
            "allow-request-email-token",
            "allow-request-phone-token",
            "allow-add-three-pid",
            "allow-remove-three-pid",
            "allow-verify-device",
            "allow-change-recovery-key",
            "allow-set-profile-passphrase",
            "allow-start-new-backup",
            "allow-notification-level",
            "allow-set-notification-level",
            "allow-profile-list",
            "allow-add-profile",
            "allow-switch-profile",
            "allow-exit-app",
            "allow-minimise-to-tray",
        ])
    );
    assert!(capability.get("remote").is_none(), "no remote origins");

    // The call window gets only what a call needs (ADR-0060).
    let call: serde_json::Value =
        serde_json::from_str(include_str!("../capabilities/call.json")).unwrap();
    assert_eq!(call["windows"], serde_json::json!([CALL_WINDOW]));
    assert_eq!(
        call["permissions"],
        serde_json::json!([
            "allow-watch-session",
            "allow-call-status",
            "allow-call-video-frame",
            "allow-call-leave",
            "allow-call-mute",
            "allow-call-deafen",
            "allow-call-volume",
            "allow-call-camera",
            "allow-call-screen",
            "allow-call-devices",
            "allow-set-call-devices",
            "allow-call-video-effect",
            "allow-call-stats",
            "allow-call-stats-read",
            "allow-close-call-window",
            "allow-call-window-ready"
        ])
    );
    assert!(call.get("remote").is_none(), "no remote origins");
}

/// GIVEN the call window WHEN its page calls a call command THEN it is
/// relayed; WHEN it calls a main window command, a device test or opens
/// windows THEN it is refused before the core.
#[test]
fn given_the_call_window_then_only_call_commands_are_allowed() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let _main = create_main_window(&app).unwrap();
    let window = create_call_window(&app).unwrap();
    assert_eq!(window.label(), CALL_WINDOW);
    assert_eq!(
        invoke_on(&window, "call_status", app_origin(), serde_json::json!({})).unwrap()["phase"],
        call_idle()["phase"]
    );
    let before = core.calls.load(Ordering::SeqCst);
    for (cmd, args) in [
        ("room_list", serde_json::json!({})),
        ("app_status", serde_json::json!({})),
        ("open_call_window", serde_json::json!({})),
        ("minimise_to_tray", serde_json::json!({})),
        ("exit_app", serde_json::json!({})),
        (
            "call_microphone",
            serde_json::json!({"autoLevel": true, "gain": 100}),
        ),
        (
            "call_device_test",
            serde_json::json!({"test": "microphone", "device": null}),
        ),
        (
            "call_join",
            serde_json::json!({"roomId": "!r:example.invalid", "video": false}),
        ),
    ] {
        assert!(
            invoke_on(&window, cmd, app_origin(), args).is_err(),
            "{cmd} refused"
        );
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), before, "nothing relayed");
    assert!(
        invoke_on(
            &window,
            "call_status",
            "https://evil.invalid",
            serde_json::json!({})
        )
        .is_err()
    );
}

/// GIVEN the call window WHEN it lists the call devices and changes them
/// THEN both are relayed (ADR-0065); WHEN an ID is empty or too long THEN it
/// is refused before the core, and the device test commands stay refused.
#[test]
fn given_the_call_window_then_it_lists_and_changes_call_devices() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let _main = create_main_window(&app).unwrap();
    let window = create_call_window(&app).unwrap();
    let invoke = |cmd: &str, args| invoke_on(&window, cmd, app_origin(), args);
    assert_eq!(
        invoke("call_devices", serde_json::json!({})).unwrap()["recording"][0]["id"],
        "mic-1"
    );
    assert_eq!(
        invoke(
            "set_call_devices",
            serde_json::json!({"recording": "mic-1", "playout": null, "camera": "cam-1"})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        *core.call_devices.lock().unwrap(),
        Some((Some("mic-1".to_owned()), None, Some("cam-1".to_owned()))),
        "each choice crosses in its place, and null stays the system default"
    );
    let relayed = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"recording": "", "playout": null}),
        serde_json::json!({"playout": "s".repeat(513)}),
        serde_json::json!({"camera": "cam\n1"}),
    ] {
        assert_eq!(
            invoke("set_call_devices", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        relayed,
        "nothing relayed"
    );
    for cmd in ["call_device_test_stop", "call_device_test_status"] {
        assert!(invoke(cmd, serde_json::json!({})).is_err(), "{cmd} refused");
    }
    // The call window may change the own video effect (ADR-0068).
    let relayed = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "call_video_effect",
            serde_json::json!({"effect": {"background": "none", "blur": 50, "filter": "warm"}, "mirror": true})
        )
        .unwrap()["phase"],
        "active"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), relayed + 1);
}

/// GIVEN the call statistics command WHEN the main window calls it THEN it is
/// refused before the core; WHEN the call window turns it on THEN the core
/// is told, and a reading reaches only that window's channel while it is on,
/// is dropped when out of its limits, and stops when the window turns it off,
/// reloads or closes.
#[test]
fn given_call_statistics_then_only_the_call_window_may_open_them_and_only_while_open() {
    use persephone_contracts::{CallStats, CallStatsAudioSent};
    let delivered: Arc<Mutex<Vec<(u32, String)>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let core = FakeCore::new(true);
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, body| {
            let body = match body {
                tauri::ipc::InvokeResponseBody::Json(json) => json.clone(),
                tauri::ipc::InvokeResponseBody::Raw(_) => "raw".into(),
            };
            record.lock().unwrap().push((callback.0, body));
            true
        }),
        core.clone(),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let main = create_main_window(&app).unwrap();
    let call = create_call_window(&app).unwrap();
    let reading = |level: u8| CallStats {
        microphone: Some(CallStatsAudioSent {
            level,
            bitrate_kbps: Some(32),
            packets_sent: 7,
            packets_lost: None,
            jitter_ms: None,
            rtt_ms: None,
        }),
        ..CallStats::default()
    };
    let stats_on = |on: bool, page: u64| serde_json::json!({"on": on, "page": page, "onStats": "__CHANNEL__:9"});
    let watch = serde_json::json!({"onChange": "__CHANNEL__:3", "onSoundLevel": "__CHANNEL__:4",
                                   "onLevels": "__CHANNEL__:5"});
    // A page's first request: it counts a new page and gives the page its
    // count, which each statistics request carries.
    let new_page = || {
        invoke_on(&call, "watch_session", app_origin(), watch.clone())
            .unwrap()
            .as_u64()
            .expect("watch_session gives the page count")
    };
    let switches = || core.stats_switches.lock().unwrap().clone();
    let sent = |level: u8| {
        let text = serde_json::to_string(&reading(level)).unwrap();
        delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|(channel, body)| *channel == 9 && *body == text)
            .count()
    };
    // Every delivery on the statistics channel, whatever it holds: a clamped
    // or repaired reading would count here, but not in `sent`.
    let total = || {
        delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|(channel, _)| *channel == 9)
            .count()
    };
    // A reading that the host refuses or that has no channel to go to must
    // add no delivery at all.
    let none_delivered = |stats: CallStats| {
        let before = total();
        subscriptions.dispatch(Event::CallStats { stats });
        assert_eq!(total(), before, "no delivery");
    };
    // The page handled its last reading (ADR-0066).
    let read = |from: u64| {
        invoke_on(
            &call,
            "call_stats_read",
            app_origin(),
            serde_json::json!({"page": from}),
        )
    };
    // A stop of the host goes on a task of its own, so wait for it, but not
    // forever: the mock runtime sends no page or window events, so nothing
    // else would end the wait. A switch that never comes fails here, by name.
    let wait_for = |count: usize| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let seen = core.stats_switches.lock().unwrap().clone();
            if seen.len() >= count {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waited 10 s for {count} statistics switches, saw {seen:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    // A start held at the core until `release` sends.
    let hold_start = || {
        let (arrived, at_core) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        *core.stats_gate.lock().unwrap() = Some((arrived, held));
        (at_core, release)
    };

    // The main window has no permission, so nothing reaches the core. Its
    // page is counted first, so the request is valid apart from the window:
    // only the capability can refuse it.
    let main_page = invoke_on(
        &main,
        "watch_session",
        app_origin(),
        serde_json::json!({"onChange": "__CHANNEL__:13", "onSoundLevel": "__CHANNEL__:14",
                           "onLevels": "__CHANNEL__:15"}),
    )
    .unwrap()
    .as_u64()
    .expect("watch_session gives the main page its count");
    let refused = invoke_on(&main, "call_stats", app_origin(), stats_on(true, main_page));
    assert!(refused.is_err());
    assert_ne!(refused, Err(serde_json::json!("invalid_argument")));
    assert!(switches().is_empty());

    // Not asked for: no channel, nothing is delivered.
    none_delivered(reading(1));

    // A bad argument, or no page count, never reaches the core.
    for bad in [
        serde_json::json!({"on": "yes", "page": 0, "onStats": "__CHANNEL__:9"}),
        serde_json::json!({"on": true, "onStats": "__CHANNEL__:9"}),
        serde_json::json!({"on": true, "page": -1, "onStats": "__CHANNEL__:9"}),
    ] {
        assert!(invoke_on(&call, "call_stats", app_origin(), bad).is_err());
    }
    assert!(switches().is_empty());

    // Before its page is counted, the window has no count: page 0, which
    // the map's default would give, is not one, so a start, a stop and a
    // read are all refused and nothing reaches the core.
    assert!(invoke_on(&call, "call_stats", app_origin(), stats_on(true, 0)).is_err());
    assert!(invoke_on(&call, "call_stats", app_origin(), stats_on(false, 0)).is_err());
    assert!(read(0).is_err());
    assert!(switches().is_empty());

    let page = new_page();

    // A start that the core refuses keeps no channel.
    core.refuse_stats.store(true, Ordering::SeqCst);
    assert!(invoke_on(&call, "call_stats", app_origin(), stats_on(true, page)).is_err());
    core.refuse_stats.store(false, Ordering::SeqCst);
    assert_eq!(switches(), [true], "the start was tried");
    none_delivered(reading(1));
    assert!(!subscriptions.forget_stats(CALL_WINDOW), "no channel kept");
    core.stats_switches.lock().unwrap().clear();

    // On: the core is told, and a reading in its limits is delivered.
    assert_eq!(
        invoke_on(&call, "call_stats", app_origin(), stats_on(true, page)),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(switches(), [true]);
    subscriptions.dispatch(Event::CallStats { stats: reading(2) });
    assert_eq!(sent(2), 1);

    // A reading outside its limits is dropped whole. The page read the
    // first reading, so a held-back delivery cannot hide a clamped one.
    assert_eq!(read(page), Ok(serde_json::Value::Null));
    none_delivered(reading(101));

    // Off: the channel goes and the core is told.
    assert_eq!(
        invoke_on(&call, "call_stats", app_origin(), stats_on(false, page)),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(switches(), [true, false]);
    none_delivered(reading(3));

    // A window that is reloaded or closed drops its channel and tells the
    // core to stop, through the handler its page load and its close use.
    invoke_on(&call, "call_stats", app_origin(), stats_on(true, page)).unwrap();
    assert_eq!(switches(), [true, false, true]);
    let link: CoreLink = core.clone();
    reset_subscriptions(&link, &subscriptions, CALL_WINDOW);
    wait_for(4);
    assert_eq!(switches(), [true, false, true, false]);
    // The channel went with it, so another reset has nothing to stop.
    assert!(!subscriptions.forget_stats(CALL_WINDOW), "once");
    none_delivered(reading(4));

    // A request of a page that is gone carries its old count, so it fails
    // and changes nothing, however late Tauri starts its task: after the
    // reload or close, and after the new page's `watch_session`.
    core.stats_switches.lock().unwrap().clear();
    let old = page;
    assert_eq!(
        invoke_on(&call, "call_stats", app_origin(), stats_on(true, old)),
        Err(serde_json::json!("invalid_argument"))
    );
    let page = new_page();
    assert!(invoke_on(&call, "call_stats", app_origin(), stats_on(true, old)).is_err());
    assert!(switches().is_empty(), "a stale start reaches no core");
    none_delivered(reading(5));
    // The new page's own start stands, and a stale stop does not end it.
    invoke_on(&call, "call_stats", app_origin(), stats_on(true, page)).unwrap();
    assert!(invoke_on(&call, "call_stats", app_origin(), stats_on(false, old)).is_err());
    assert_eq!(switches(), [true]);
    subscriptions.dispatch(Event::CallStats { stats: reading(6) });
    assert_eq!(sent(6), 1);

    // A start already at the core when the window reloads, or when the new
    // page's `watch_session` runs: it keeps no channel and turns the numbers
    // off again, so nothing reads for a page that is gone.
    reset_subscriptions(&link, &subscriptions, CALL_WINDOW);
    wait_for(2);
    core.stats_switches.lock().unwrap().clear();
    for reload_by_watch in [false, true] {
        let page = new_page();
        let (at_core, release) = hold_start();
        let window = call.clone();
        let start = std::thread::spawn(move || {
            invoke_on(
                &window,
                "call_stats",
                app_origin(),
                serde_json::json!({"on": true, "page": page, "onStats": "__CHANNEL__:9"}),
            )
        });
        at_core
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the start reached the core");
        if reload_by_watch {
            new_page();
        } else {
            reset_subscriptions(&link, &subscriptions, CALL_WINDOW);
        }
        release.send(()).unwrap();
        assert_eq!(start.join().unwrap(), Ok(serde_json::Value::Null));
        assert_eq!(
            switches(),
            [true, false],
            "reload by watch: {reload_by_watch}"
        );
        none_delivered(reading(7));
        core.stats_switches.lock().unwrap().clear();
    }

    // A channel that a page kept goes when a new page starts, also when no
    // load event came first, and the core is told to stop.
    let page = new_page();
    invoke_on(&call, "call_stats", app_origin(), stats_on(true, page)).unwrap();
    new_page();
    wait_for(2);
    assert_eq!(switches(), [true, false]);
    none_delivered(reading(8));

    // One reading in flight per page, and only the newest waits.
    let page = new_page();
    invoke_on(&call, "call_stats", app_origin(), stats_on(true, page)).unwrap();
    let base = total();
    for level in [10, 11, 12] {
        subscriptions.dispatch(Event::CallStats {
            stats: reading(level),
        });
    }
    assert_eq!((sent(10), sent(11), sent(12)), (1, 0, 0), "one in flight");
    assert_eq!(total() - base, 1);
    assert_eq!(read(page), Ok(serde_json::Value::Null));
    assert_eq!((sent(11), sent(12)), (0, 1), "the newest waited");
    assert_eq!(total() - base, 2);
    // Nothing waits now, so a read only frees the page and the next reading
    // goes at once.
    assert_eq!(read(page), Ok(serde_json::Value::Null));
    assert_eq!(total() - base, 2);
    subscriptions.dispatch(Event::CallStats { stats: reading(13) });
    assert_eq!(sent(13), 1);
    // A read of a page that is gone, or from the main window, changes nothing.
    // The main window's read names its own counted page, so only the
    // capability can refuse it.
    assert_eq!(read(page - 1), Err(serde_json::json!("invalid_argument")));
    let refused = invoke_on(
        &main,
        "call_stats_read",
        app_origin(),
        serde_json::json!({"page": main_page}),
    );
    assert!(refused.is_err());
    assert_ne!(refused, Err(serde_json::json!("invalid_argument")));
    subscriptions.dispatch(Event::CallStats { stats: reading(14) });
    assert_eq!(sent(14), 0, "the stale read did not free the page");
    // With no statistics channel a read is fine and does nothing.
    assert!(subscriptions.forget_stats(CALL_WINDOW));
    assert_eq!(read(page), Ok(serde_json::Value::Null));
}

/// GIVEN a call window WHEN the main window says its page is ready THEN it
/// is refused; WHEN the call page says so THEN the main window's open
/// returns without waiting.
#[test]
fn given_a_ready_call_page_then_the_open_returns() {
    let app = app();
    let main = create_main_window(&app).unwrap();
    let call = create_call_window(&app).unwrap();
    assert!(
        invoke_on(
            &main,
            "call_window_ready",
            app_origin(),
            serde_json::json!({})
        )
        .is_err()
    );
    assert_eq!(
        invoke_on(
            &call,
            "call_window_ready",
            app_origin(),
            serde_json::json!({})
        ),
        Ok(serde_json::Value::Null)
    );
    let started = std::time::Instant::now();
    assert_eq!(
        invoke_on(
            &main,
            "open_call_window",
            app_origin(),
            serde_json::json!({})
        ),
        Ok(serde_json::Value::Null)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "no wait"
    );
}

/// GIVEN device test commands THEN bounded IDs cross, a bad one never
/// reaches the core, and a reply outside the contract is refused. A reply
/// without the effect refusal (ADR-0068) reaches the page as no refusal.
#[test]
fn given_device_test_commands_then_bounded_arguments_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args| invoke_on(&window, cmd, app_origin(), args);
    assert_eq!(
        invoke(
            "call_device_test",
            serde_json::json!({"test": "speaker", "device": "{0.0.0.00000000}.{a}"})
        ),
        Ok(serde_json::json!({
            "id": 3, "test": "speaker", "state": "starting", "stream": 0, "effectRefused": false
        }))
    );
    assert_eq!(
        invoke("call_device_test_stop", serde_json::json!({})),
        Ok(serde_json::Value::Null)
    );
    // The test ID crosses to the core.
    assert_eq!(
        invoke("call_device_test_stop", serde_json::json!({"id": 3})),
        Ok(serde_json::Value::Null)
    );
    assert!(invoke("call_device_test_stop", serde_json::json!({"id": 4})).is_err());
    assert!(invoke("call_device_test_stop", serde_json::json!({"id": -1})).is_err());
    assert!(invoke("call_device_test_status", serde_json::json!({})).is_err());
    let before = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"test": "speaker", "device": ""}),
        serde_json::json!({"test": "speaker", "device": "a\nb"}),
        serde_json::json!({"test": "speaker", "device": "a".repeat(1025)}),
        serde_json::json!({"test": "kettle", "device": null}),
        serde_json::json!({"device": null}),
    ] {
        assert!(invoke("call_device_test", args.clone()).is_err(), "{args}");
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), before, "nothing relayed");
}

#[test]
fn given_unreachable_core_then_the_page_gets_only_a_generic_error() {
    let app = app_with(FakeCore::new(false));
    let window = create_main_window(&app).unwrap();
    let reply = get_ipc_response(
        &window,
        request("app_status", app_origin(), serde_json::json!({})),
    );
    assert_eq!(reply.unwrap_err(), serde_json::json!("core_unavailable"));
}

#[test]
fn given_view_commands_then_replies_are_relayed_and_checked_against_contracts() {
    let rooms = call("room_list", app_origin(), serde_json::json!({})).unwrap();
    assert_eq!(rooms["rooms"][0]["displayName"], "<b>x</b>");
    let page = call(
        "timeline_page",
        app_origin(),
        serde_json::json!({"roomId": "!r:example.invalid", "cursor": null}),
    )
    .unwrap();
    assert_eq!(page["items"][0]["body"], "hi");

    let error = |args| call("timeline_page", app_origin(), args).unwrap_err();
    assert_eq!(
        error(serde_json::json!({"roomId": "!missing:example.invalid", "cursor": null})),
        "not_found"
    );
    assert_eq!(
        error(serde_json::json!({"roomId": "!r:example.invalid", "cursor": "t1.0"})),
        "invalid_argument"
    );
    // The UI refuses a reply carrying a field outside the contract.
    assert_eq!(
        error(serde_json::json!({"roomId": "!bad-reply:example.invalid", "cursor": null})),
        "core_unavailable"
    );
}

/// GIVEN valid arguments WHEN the page asks to read events again THEN the
/// room and event IDs reach the core unchanged and the contract-checked page
/// comes back. GIVEN no IDs, too many, a repeated, empty, oversized or
/// control-character ID, an oversized room ID, a missing or extra argument,
/// or a remote origin THEN nothing reaches the core. GIVEN a reply with a
/// field outside the contract THEN the page gets only a generic error.
#[test]
fn given_timeline_events_then_only_validated_arguments_are_relayed() {
    let fake = FakeCore::new(true);
    let app = app_with(fake.clone());
    let window = create_main_window(&app).unwrap();
    let ids = |count: usize| (0..count).map(|n| format!("$e{n}")).collect::<Vec<_>>();
    let max = persephone_core_protocol::MAX_TIMELINE_EVENTS;

    let body = serde_json::json!({"roomId": "!r:s", "eventIds": ids(max)});
    let page = invoke_on(&window, "timeline_events", app_origin(), body.clone()).unwrap();
    assert_eq!(
        page,
        serde_json::json!({"roomId": "!r:s", "items": [], "nextCursor": null, "names": []})
    );
    assert_eq!(
        fake.timeline_events.lock().unwrap().clone(),
        Some(("!r:s".to_owned(), ids(max)))
    );
    let calls = fake.calls.load(Ordering::SeqCst);
    assert_eq!(calls, 1);

    for bad in [
        serde_json::json!({"roomId": "!r:s", "eventIds": []}),
        serde_json::json!({"roomId": "!r:s", "eventIds": ids(max + 1)}),
        serde_json::json!({"roomId": "!r:s", "eventIds": ["$a", "$a"]}),
        serde_json::json!({"roomId": "!r:s", "eventIds": [""]}),
        serde_json::json!({"roomId": "!r:s", "eventIds": ["x".repeat(256)]}),
        serde_json::json!({"roomId": "!r:s", "eventIds": ["$a\n"]}),
        serde_json::json!({"roomId": "!r:s", "eventIds": "$a"}),
        serde_json::json!({"roomId": "!r:s", "eventIds": [1]}),
        serde_json::json!({"roomId": "x".repeat(256), "eventIds": ["$a"]}),
        serde_json::json!({"roomId": "!r:s"}),
        serde_json::json!({"eventIds": ["$a"]}),
    ] {
        assert!(
            invoke_on(&window, "timeline_events", app_origin(), bad.clone()).is_err(),
            "{bad}"
        );
    }
    assert!(invoke_on(&window, "timeline_events", "https://evil.invalid", body).is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls, "nothing relayed");

    let leaky = serde_json::json!({"roomId": "!leaky:s", "eventIds": ["$a"]});
    let error = invoke_on(&window, "timeline_events", app_origin(), leaky).unwrap_err();
    assert_eq!(error, serde_json::json!("core_unavailable"));
}

#[test]
fn given_oversized_arguments_then_the_ui_refuses_without_calling_the_core() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    for args in [
        serde_json::json!({"roomId": format!("!{}", "r".repeat(300)), "cursor": null}),
        serde_json::json!({"roomId": "!r:example.invalid", "cursor": "c".repeat(65)}),
    ] {
        let reply = get_ipc_response(&window, request("timeline_page", app_origin(), args));
        assert_eq!(reply.unwrap_err(), serde_json::json!("invalid_argument"));
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), 0);
}

/// Subscribing stores the page's channel only once the core accepts the room;
/// core events reach exactly that channel, as an empty message; unsubscribing
/// stops them.
#[test]
fn given_subscriptions_then_core_events_reach_only_the_subscribed_rooms_channel() {
    use std::sync::Mutex;
    let delivered: Arc<Mutex<Vec<(u32, String)>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let core = FakeCore::new(true);
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, body| {
            let body = match body {
                tauri::ipc::InvokeResponseBody::Json(json) => json.clone(),
                tauri::ipc::InvokeResponseBody::Raw(_) => "raw".into(),
            };
            record.lock().unwrap().push((callback.0, body));
            true
        }),
        core.clone(),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let window = create_main_window(&app).unwrap();
    let subscribe = |room: &str, channel: u32| {
        get_ipc_response(
            &window,
            request(
                "subscribe_room",
                app_origin(),
                serde_json::json!({"roomId": room, "onChange": format!("__CHANNEL__:{channel}")}),
            ),
        )
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        subscribe("!r:example.invalid", 7),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        subscribe("!s:example.invalid", 8),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        subscribe("!missing:example.invalid", 9).unwrap_err(),
        "not_found"
    );
    assert_eq!(
        subscribe("!full:example.invalid", 10).unwrap_err(),
        "limit_exceeded"
    );
    assert_eq!(
        subscribe(&format!("!{}", "r".repeat(300)), 11).unwrap_err(),
        "invalid_argument"
    );
    // A channel value that is not a page channel is refused by Tauri.
    assert!(
        get_ipc_response(
            &window,
            request(
                "subscribe_room",
                app_origin(),
                serde_json::json!({"roomId": "!r:example.invalid", "onChange": "javascript:alert(1)"}),
            ),
        )
        .is_err()
    );
    assert_eq!(
        subscriptions.rooms(),
        ["!r:example.invalid", "!s:example.invalid"]
    );

    let changed = |room: &str| Event::RoomChanged {
        room_id: room.into(),
    };
    subscriptions.dispatch(changed("!r:example.invalid"));
    subscriptions.dispatch(changed("!missing:example.invalid"));
    subscriptions.dispatch(changed("!s:example.invalid"));
    subscriptions.dispatch(changed("!r:example.invalid"));
    assert_eq!(
        *delivered.lock().unwrap(),
        [(7, "null".into()), (8, "null".into()), (7, "null".into())]
    );

    let reply = get_ipc_response(
        &window,
        request(
            "unsubscribe_room",
            app_origin(),
            serde_json::json!({"roomId": "!r:example.invalid"}),
        ),
    );
    assert!(reply.is_ok());
    assert_eq!(subscriptions.rooms(), ["!s:example.invalid"]);
    delivered.lock().unwrap().clear();
    subscriptions.dispatch(changed("!r:example.invalid"));
    assert!(delivered.lock().unwrap().is_empty());
}

/// A session change drops every room channel and reaches the page's session
/// channel as `"session"`, a room-list change as `"rooms"`; lock and unlock relay the core's state and
/// refuse one outside the contract.
#[test]
fn given_a_session_change_then_room_channels_drop_and_the_session_channel_is_told() {
    use std::sync::Mutex;
    let delivered: Arc<Mutex<Vec<(u32, String)>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, body| {
            let body = match body {
                tauri::ipc::InvokeResponseBody::Json(json) => json.clone(),
                tauri::ipc::InvokeResponseBody::Raw(_) => "raw".into(),
            };
            record.lock().unwrap().push((callback.0, body));
            true
        }),
        FakeCore::new(true),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert!(
        invoke(
            "watch_session",
            serde_json::json!({"onChange": "__CHANNEL__:3", "onSoundLevel": "__CHANNEL__:4", "onLevels": "__CHANNEL__:5"})
        )
        .is_ok()
    );
    assert!(
        invoke(
            "subscribe_room",
            serde_json::json!({"roomId": "!r:example.invalid", "onChange": "__CHANNEL__:7"})
        )
        .is_ok()
    );

    subscriptions.dispatch(Event::SessionChanged {});
    assert!(subscriptions.rooms().is_empty(), "room channels dropped");
    subscriptions.dispatch(Event::RoomChanged {
        room_id: "!r:example.invalid".into(),
    });
    subscriptions.dispatch(Event::RoomListChanged {});
    subscriptions.dispatch(Event::SecurityChanged {});
    subscriptions.dispatch(Event::CallChanged {});
    subscriptions.dispatch(Event::CallVideo {});
    subscriptions.dispatch(Event::ModulesChanged {});
    subscriptions.dispatch(Event::AccountChanged {});
    subscriptions.dispatch(Event::DiagnosticsChanged { on: false });
    subscriptions.dispatch(Event::CallSoundLevel { level: 42 });
    subscriptions.dispatch(Event::CallSoundLevel {
        level: MAX_CALL_SOUND_LEVEL + 1,
    });
    let level = |level| persephone_contracts::CallParticipantLevel {
        user_id: "@bob:example.invalid".into(),
        device_id: "BOBDEV".into(),
        level,
    };
    subscriptions.dispatch(Event::CallLevels {
        levels: vec![level(30)],
    });
    subscriptions.dispatch(Event::CallLevels {
        levels: vec![level(MAX_CALL_SOUND_LEVEL + 1)],
    });
    subscriptions.dispatch(Event::CallDeviceTestChanged {});
    let delivered = delivered.lock().unwrap().clone();
    // The other devices' levels go on their own channel; a level over the
    // top drops the whole event.
    let others: Vec<&String> = delivered
        .iter()
        .filter(|(channel, _)| *channel == 5)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        others,
        [&r#"[{"userId":"@bob:example.invalid","deviceId":"BOBDEV","level":30}]"#.to_owned()],
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""deviceTest""#.into())),
        "{delivered:?}"
    );
    // The level goes on its own channel as a number; one over the top is
    // dropped, and nothing about it reaches the session channel.
    let levels: Vec<&String> = delivered
        .iter()
        .filter(|(channel, _)| *channel == 4)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(levels, [&"42".to_owned()], "{delivered:?}");
    assert!(
        delivered.contains(&(3, r#""diagnostics""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""security""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""account""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""calls""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""callVideo""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""modules""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""session""#.into())),
        "{delivered:?}"
    );
    assert!(
        delivered.contains(&(3, r#""rooms""#.into())),
        "{delivered:?}"
    );
    assert!(
        !delivered
            .iter()
            .any(|(id, body)| *id == 7 && body == "null"),
        "no room message after the session changed: {delivered:?}"
    );

    assert_eq!(
        invoke("lock_session", serde_json::json!({})),
        Ok(serde_json::json!("locked"))
    );
    assert_eq!(
        invoke("unlock_session", serde_json::json!({})).unwrap_err(),
        "core_unavailable"
    );
    assert_eq!(
        invoke("reset_profile", serde_json::json!({})),
        Ok(serde_json::json!("unlocking"))
    );
}

/// Sign-in relays only the homeserver URL, refuses oversized input before the
/// core, and checks the reply against the contract.
#[test]
fn given_sign_in_then_only_the_homeserver_crosses_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |args: serde_json::Value| {
        get_ipc_response(&window, request("sign_in", app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke(serde_json::json!({"homeserver": "https://matrix.example.org"})),
        Ok(
            serde_json::json!({"state": "signing_in", "userId": null, "error": null, "presenceRefused": false})
        )
    );
    assert_eq!(
        invoke(serde_json::json!({"homeserver": "https://evil.example"})).unwrap_err(),
        "core_unavailable"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(serde_json::json!({"homeserver": "h".repeat(3000)})).unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized URL never reached the core"
    );
    let sign_out = |args: serde_json::Value| {
        get_ipc_response(&window, request("sign_out", app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let calls = core.calls.load(Ordering::SeqCst);
    assert!(sign_out(serde_json::json!({})).is_err());
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "a sign-out without the key choice never reached the core"
    );
    assert_eq!(
        sign_out(serde_json::json!({"discardKeys": false})).unwrap_err(),
        "keys_not_backed_up"
    );
    assert_eq!(
        sign_out(serde_json::json!({"discardKeys": true})),
        Ok(serde_json::json!("restart_required"))
    );
}

/// Registration relays only the homeserver URL and the username; the reason
/// a registration failed crosses as a fixed code, and anything else is
/// refused. No password or token argument exists.
#[test]
fn given_register_then_only_the_homeserver_and_username_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |args: serde_json::Value| {
        get_ipc_response(&window, request("register", app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let homeserver = "https://matrix.example.org";
    assert_eq!(
        invoke(serde_json::json!({"homeserver": homeserver, "username": "newbie"})).unwrap()["state"],
        "signing_in"
    );
    assert_eq!(
        invoke(serde_json::json!({"homeserver": homeserver, "username": "taken"})).unwrap()["error"],
        "username_taken"
    );
    assert_eq!(
        invoke(serde_json::json!({"homeserver": homeserver, "username": "odd"})).unwrap_err(),
        "core_unavailable"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"homeserver": "h".repeat(3000), "username": "newbie"}),
        serde_json::json!({"homeserver": homeserver, "username": "n".repeat(256)}),
    ] {
        assert_eq!(invoke(args).unwrap_err(), "invalid_argument");
    }
    assert!(invoke(serde_json::json!({"homeserver": homeserver})).is_err());
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "refused arguments never reached the core"
    );
}

/// The encryption commands take no arguments and relay codes only: a reply
/// carrying a recovery key is refused by the contract.
#[test]
fn given_encryption_commands_then_only_codes_cross_and_secrets_are_refused() {
    let app = app_with(FakeCore::new(true));
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str| {
        get_ipc_response(&window, request(cmd, app_origin(), serde_json::json!({})))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke("security_status"),
        Ok(
            serde_json::json!({"identity": "none", "recovery": "disabled", "operation": "idle", "error": null, "verification": null, "canSetUpRecovery": false, "canStartNewBackup": false, "profilePassphrase": false})
        )
    );
    assert_eq!(
        invoke("set_up_encryption").unwrap()["operation"],
        "waiting_for_password"
    );
    assert_eq!(invoke("recover_with_key").unwrap_err(), "core_unavailable");
    assert_eq!(invoke("set_up_recovery").unwrap()["operation"], "working");
}

/// The call commands relay the room and flag only. An oversized room ID never
/// reaches the core; a refused join is a status with a code; a reply with a
/// field or a code outside the contract is refused.
#[test]
fn given_call_commands_then_bounded_arguments_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let room = |id: &str| serde_json::json!({"roomId": id});

    assert_eq!(
        invoke("call_readiness", room("!r:example.invalid")).unwrap(),
        serde_json::json!({"available": true, "reason": null, "activeMembers": 1})
    );
    assert!(invoke("call_readiness", room("!odd:example.invalid")).is_err());
    assert_eq!(
        invoke("call_readiness", room("!nope:example.invalid")).unwrap_err(),
        "not_found"
    );
    let join = |id: &str, video: bool| {
        invoke(
            "call_join",
            serde_json::json!({"roomId": id, "video": video}),
        )
    };
    assert_eq!(join("!r:example.invalid", false).unwrap()["camera"], "off");
    assert_eq!(
        join("!r:example.invalid", true).unwrap()["camera"],
        "starting"
    );
    assert_eq!(
        join("!plain:example.invalid", false).unwrap()["joinError"],
        "not_encrypted"
    );
    assert!(join("!leaky:example.invalid", false).is_err());
    // A join without the video flag, or with a flag of another type, is
    // refused by the argument types.
    assert!(invoke("call_join", room("!r:example.invalid")).is_err());
    assert!(
        invoke(
            "call_join",
            serde_json::json!({"roomId": "!r:example.invalid", "video": "yes"})
        )
        .is_err()
    );
    assert_eq!(
        invoke("call_camera", serde_json::json!({"on": true})).unwrap()["camera"],
        "starting"
    );
    assert_eq!(
        invoke("call_camera", serde_json::json!({"on": false})).unwrap()["camera"],
        "off"
    );
    assert_eq!(
        invoke("call_screen", serde_json::json!({"on": true})).unwrap()["screen"],
        "starting"
    );
    assert_eq!(
        invoke("call_screen", serde_json::json!({"on": false})).unwrap()["screen"],
        "off"
    );
    assert_eq!(
        invoke("call_leave", serde_json::json!({})).unwrap()["phase"],
        "idle"
    );
    assert_eq!(
        invoke("call_status", serde_json::json!({})).unwrap()["phase"],
        "idle"
    );
    let muted = invoke("call_mute", serde_json::json!({"muted": true})).unwrap();
    assert_eq!(muted["muted"], true);
    assert_eq!(muted["participants"][0]["userId"], "@bob:example.invalid");
    let deafened = invoke("call_deafen", serde_json::json!({"deafened": true})).unwrap();
    assert_eq!(
        (&deafened["deafened"], &deafened["muted"]),
        (&serde_json::json!(true), &serde_json::json!(true))
    );
    let quiet = invoke("call_volume", serde_json::json!({"percent": 35})).unwrap();
    assert_eq!(quiet["volume"], 35);

    // The handler takes the flag only, so an extra argument is dropped before
    // the relay: what reaches the core is `CallScreen { on }` and nothing else.
    for extra in ["source", "monitor", "window", "node", "handle", "device"] {
        assert_eq!(
            invoke("call_screen", serde_json::json!({"on": true, extra: "x"})).unwrap()["screen"],
            "starting",
            "{extra}"
        );
    }

    // The rings are listed without arguments, and a decline names one room.
    let rings = invoke("call_rings", serde_json::json!({})).unwrap();
    assert_eq!(rings["rings"][0]["caller"], "@bob:example.invalid");
    assert_eq!(rings["rings"][0]["secondsLeft"], 42);
    assert_eq!(
        invoke("call_decline_ring", room("!r:example.invalid")).unwrap(),
        serde_json::json!({"rings": []})
    );
    assert_eq!(
        invoke("call_decline_ring", room("!nope:example.invalid")).unwrap_err(),
        "not_found"
    );
    assert!(invoke("call_decline_ring", room("!leaky:example.invalid")).is_err());
    assert!(invoke("call_decline_ring", serde_json::json!({})).is_err());
    assert!(invoke("call_decline_ring", serde_json::json!({"roomId": 5})).is_err());

    let before = core.calls.load(Ordering::SeqCst);
    let long = "!".to_owned() + &"r".repeat(MAX_ROOM_ID_BYTES);
    assert_eq!(
        invoke("call_readiness", room(&long)).unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        join(&long, true).unwrap_err(),
        "invalid_argument",
        "call_join"
    );
    // Arguments of the wrong type or extra arguments do not reach the core.
    assert!(invoke("call_mute", serde_json::json!({"muted": "yes"})).is_err());
    assert!(invoke("call_mute", serde_json::json!({})).is_err());
    assert!(invoke("call_camera", serde_json::json!({"on": 1})).is_err());
    assert!(invoke("call_camera", serde_json::json!({})).is_err());
    // The page names no screen: the command is the flag, and a wrong or
    // missing flag is refused before it reaches the core.
    assert!(invoke("call_screen", serde_json::json!({"on": 1})).is_err());
    assert!(invoke("call_screen", serde_json::json!({})).is_err());
    assert_eq!(
        invoke("call_decline_ring", room(&long)).unwrap_err(),
        "invalid_argument"
    );
    assert!(invoke("call_deafen", serde_json::json!({"deafened": "yes"})).is_err());
    assert!(invoke("call_deafen", serde_json::json!({})).is_err());
    assert!(invoke("call_volume", serde_json::json!({"percent": "50"})).is_err());
    assert!(invoke("call_volume", serde_json::json!({"percent": 256})).is_err());
    assert!(invoke("call_volume", serde_json::json!({"percent": -1})).is_err());
    assert!(invoke("call_volume", serde_json::json!({})).is_err());
    // In range for the type but over 100 percent: the host refuses it.
    assert_eq!(
        invoke("call_volume", serde_json::json!({"percent": 101})).unwrap_err(),
        "invalid_argument"
    );
    // The microphone settings: a flag and a gain of 0 to 400 percent in
    // steps of 5. A wrong type never reaches the core; a gain of the right
    // type but off the range or the step is refused by the host (ADR-0062).
    let before_microphone = core.calls.load(Ordering::SeqCst);
    for gain in [0, 5, 100, 395, 400] {
        assert_eq!(
            invoke(
                "call_microphone",
                serde_json::json!({"autoLevel": false, "gain": gain})
            )
            .unwrap()["phase"],
            "active",
            "{gain}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        before_microphone + 5,
        "each valid change reached the core"
    );
    for args in [
        serde_json::json!({"autoLevel": "yes", "gain": 100}),
        serde_json::json!({"autoLevel": 1, "gain": 100}),
        serde_json::json!({"autoLevel": true, "gain": "100"}),
        serde_json::json!({"autoLevel": true, "gain": -5}),
        serde_json::json!({"autoLevel": true, "gain": 65536}),
        serde_json::json!({"autoLevel": true, "gain": 100.5}),
        serde_json::json!({"autoLevel": true}),
        serde_json::json!({"gain": 100}),
        serde_json::json!({}),
    ] {
        assert!(invoke("call_microphone", args.clone()).is_err(), "{args}");
    }
    for gain in [1, 99, 401, 405, 1000, 65535] {
        assert_eq!(
            invoke(
                "call_microphone",
                serde_json::json!({"autoLevel": true, "gain": gain})
            )
            .unwrap_err(),
            "invalid_argument",
            "{gain}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        before_microphone + 5,
        "a refused change never reached the core"
    );
    // The video effect (ADR-0068): a closed set of names and a blur of 10
    // to 100 percent in steps of 10. A wrong type or an unknown name never
    // reaches the core; a blur off the range or the step is refused by the
    // host before the core.
    let before_effect = core.calls.load(Ordering::SeqCst);
    let effect = |background: &str, blur: serde_json::Value, filter: &str| serde_json::json!({"background": background, "blur": blur, "filter": filter});
    for (background, filter) in [
        ("none", "none"),
        ("dusk", "greyscale"),
        ("sea", "warm"),
        ("forest", "cool"),
        ("studio", "brighten"),
    ] {
        assert_eq!(
            invoke(
                "call_video_effect",
                serde_json::json!({"effect": effect(background, 50.into(), filter), "mirror": true})
            )
            .unwrap()["phase"],
            "active",
            "{background} {filter}"
        );
    }
    let refused = invoke(
        "call_video_effect",
        serde_json::json!({"effect": effect("blur", 10.into(), "none"), "mirror": false}),
    )
    .unwrap();
    assert_eq!(
        refused["effectRefused"], true,
        "the worker's refusal reaches the page"
    );
    // A window that changes one of them sends only that one; the core keeps
    // the other.
    for args in [
        serde_json::json!({"effect": effect("none", 50.into(), "none")}),
        serde_json::json!({"mirror": true}),
    ] {
        assert!(invoke("call_video_effect", args.clone()).is_ok(), "{args}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        before_effect + 8,
        "each valid effect reached the core"
    );
    for args in [
        serde_json::json!({"effect": effect("moon", 50.into(), "none"), "mirror": false}),
        serde_json::json!({"effect": effect("none", 50.into(), "sepia"), "mirror": false}),
        serde_json::json!({"effect": effect("Blur", 50.into(), "none"), "mirror": false}),
        serde_json::json!({"effect": effect("blur", "50".into(), "none"), "mirror": false}),
        serde_json::json!({"effect": effect("blur", (-10).into(), "none"), "mirror": false}),
        serde_json::json!({"effect": effect("blur", 256.into(), "none"), "mirror": false}),
        serde_json::json!({"effect": {"background": "none", "blur": 50}, "mirror": false}),
        serde_json::json!({"effect": {"background": "none", "blur": 50, "filter": "none", "url": "x"}, "mirror": false}),
        serde_json::json!({"effect": effect("none", 50.into(), "none"), "mirror": "yes"}),
        serde_json::json!({}),
        serde_json::json!({"effect": null, "mirror": null}),
    ] {
        assert!(invoke("call_video_effect", args.clone()).is_err(), "{args}");
    }
    for blur in [0, 5, 55, 101, 110, 255] {
        assert!(
            invoke(
                "call_video_effect",
                serde_json::json!({"effect": effect("blur", blur.into(), "none"), "mirror": false})
            )
            .is_err(),
            "{blur}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        before_effect + 8,
        "a refused effect never reached the core"
    );
    // The five valid microphone changes and the eight valid effects above
    // are the only calls since the start of this check.
    assert_eq!(core.calls.load(Ordering::SeqCst), before + 13);
}

/// Sending relays the room and body only; oversized input never reaches the
/// core, and a reply carrying more than the contract (a transaction ID) is
/// refused.
#[test]
fn given_sending_commands_then_bounded_arguments_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let room = "!r:example.invalid";
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "hello"})
        )
        .unwrap()["state"],
        "queued"
    );
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "other"})
        )
        .unwrap_err(),
        "core_unavailable"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "x".repeat(16 * 1024 + 1)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "hi", "replyTo": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized body and reply target never crossed"
    );
    assert_eq!(
        invoke(
            "send_message",
            serde_json::json!({"roomId": room, "body": "hello", "replyTo": "$e"})
        )
        .unwrap()["state"],
        "queued"
    );
    assert_eq!(
        invoke(
            "edit_message",
            serde_json::json!({"roomId": room, "eventId": "$mine", "body": "hello"})
        )
        .unwrap()["state"],
        "queued"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255)), "body": "x"}),
        serde_json::json!({"roomId": room, "eventId": "$mine", "body": "x".repeat(16 * 1024 + 1)}),
    ] {
        assert_eq!(
            invoke("edit_message", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized edits never crossed"
    );
    assert_eq!(
        invoke(
            "delete_message",
            serde_json::json!({"roomId": room, "eventId": "$mine"})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke(
            "delete_message",
            serde_json::json!({"roomId": room, "eventId": "$theirs"})
        )
        .unwrap_err(),
        "rejected"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "delete_message",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "an oversized event ID never crossed"
    );
    assert_eq!(
        invoke(
            "react",
            serde_json::json!({"roomId": room, "eventId": "$e", "key": "👍"})
        ),
        Ok(serde_json::Value::Null)
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"roomId": room, "eventId": "$e", "key": "x".repeat(65)}),
        serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255)), "key": "👍"}),
    ] {
        assert_eq!(invoke("react", args).unwrap_err(), "invalid_argument");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized reactions never crossed"
    );
    assert_eq!(
        invoke(
            "mark_read",
            serde_json::json!({"roomId": room, "eventId": "$e"})
        ),
        Ok(serde_json::Value::Null)
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "mark_read",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke(
            "room_activity",
            serde_json::json!({"roomId": room, "eventId": "$e"})
        )
        .unwrap()["typing"][0],
        "@b:example.invalid"
    );
    assert_eq!(
        invoke(
            "set_typing",
            serde_json::json!({"roomId": room, "typing": true})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke(
            "set_typing",
            serde_json::json!({"roomId": room, "typing": false})
        )
        .unwrap_err(),
        "rejected"
    );
    assert_eq!(
        invoke(
            "open_link",
            serde_json::json!({"url": "https://example.org/"})
        ),
        Ok(serde_json::Value::Bool(true))
    );
    assert_eq!(
        invoke("room_servers", serde_json::json!({"roomId": room})).unwrap()["servers"][0]["members"],
        2
    );
    assert_eq!(
        invoke("pick_file", serde_json::json!({"roomId": room})),
        Ok(serde_json::Value::Null),
        "cancelled"
    );
    assert_eq!(
        invoke(
            "save_file",
            serde_json::json!({"roomId": room, "eventId": "$f"})
        ),
        Ok(serde_json::Value::Bool(true))
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "save_file",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke("room_readers", serde_json::json!({"roomId": room})).unwrap()["readers"][0]["refused"],
        1
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "open_link",
            serde_json::json!({"url": format!("https://example.org/{}", "a".repeat(2048))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke(
            "room_activity",
            serde_json::json!({"roomId": room, "eventId": format!("${}", "e".repeat(255))})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke("pending_messages", serde_json::json!({"roomId": room})),
        Ok(serde_json::json!([]))
    );
    assert_eq!(
        invoke("retry_message", serde_json::json!({"localId": 1})),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke("discard_message", serde_json::json!({"localId": 7})).unwrap_err(),
        "not_found"
    );
    assert_eq!(
        invoke("allow_unencrypted", serde_json::json!({"roomId": room})),
        Ok(serde_json::Value::Null)
    );
}

/// Verification relays the flow ID only, refuses an oversized one before the
/// core, and passes the emoji and device name through unchanged for the page
/// to render as text.
#[test]
fn given_verification_commands_then_only_the_flow_id_crosses() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke("verify_this_device", serde_json::json!({})).unwrap()["verification"]["phase"],
        "waiting"
    );
    for cmd in [
        "accept_verification",
        "confirm_verification",
        "cancel_verification",
    ] {
        let reply = invoke(cmd, serde_json::json!({"flowId": "f1"})).unwrap();
        assert_eq!(reply["verification"]["flowId"], "f1", "{cmd}");
        assert_eq!(reply["verification"]["emoji"][0]["symbol"], "🐶");
    }
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "confirm_verification",
            serde_json::json!({"flowId": "f".repeat(129)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized flow ID never crossed"
    );
}

/// Contact commands relay the room ID only and refuse an oversized one before
/// the core; the contact's user ID comes back unchanged for the page to render
/// as text, and a trust level outside the contract is refused.
#[test]
fn given_contact_commands_then_only_the_room_id_crosses() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let status = invoke(
        "contact_status",
        serde_json::json!({"roomId": "!dm:example.invalid"}),
    )
    .unwrap();
    assert_eq!(status["userId"], "<b>@bob</b>:example.invalid");
    assert_eq!(status["trust"], "unverified");
    assert!(
        invoke(
            "contact_status",
            serde_json::json!({"roomId": "!odd:example.invalid"})
        )
        .is_err()
    );
    assert_eq!(
        invoke(
            "verify_contact",
            serde_json::json!({"roomId": "!dm:example.invalid"})
        )
        .unwrap()["operation"],
        "working"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for cmd in ["contact_status", "verify_contact"] {
        assert_eq!(
            invoke(cmd, serde_json::json!({"roomId": "!".repeat(256)})).unwrap_err(),
            "invalid_argument",
            "{cmd}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized room IDs never crossed"
    );
    assert_eq!(
        invoke(
            "verify_member",
            serde_json::json!({"roomId": "!group:example.invalid", "userId": "@bob:example.invalid"})
        )
        .unwrap()["operation"],
        "working"
    );
    assert_eq!(
        *core.verified_member.lock().unwrap(),
        Some((
            "!group:example.invalid".to_string(),
            "@bob:example.invalid".to_string()
        )),
        "the room and user cross unchanged"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"roomId": "!".repeat(256), "userId": "@b:s"}),
        serde_json::json!({"roomId": "!r:s", "userId": "@".repeat(256)}),
    ] {
        assert_eq!(
            invoke("verify_member", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized IDs never crossed"
    );
}

/// Conversation and room-directory commands relay only their named
/// arguments, refuse an oversized one before the core, and refuse a reply
/// outside the contract: an unencrypted outcome, or a directory match or
/// public room with an avatar URL, never reaches the page.
#[test]
fn given_conversation_commands_then_one_argument_crosses_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let invites = invoke("invite_list", serde_json::json!({})).unwrap();
    assert_eq!(invites["invites"][0]["inviterName"], "<b>Bob</b>");
    for cmd in ["accept_invite", "decline_invite"] {
        assert!(
            invoke(cmd, serde_json::json!({"roomId": "!inv:example.invalid"})).is_ok(),
            "{cmd}"
        );
        assert_eq!(
            invoke(cmd, serde_json::json!({"roomId": "!gone:example.invalid"})).unwrap_err(),
            "not_found",
            "{cmd}"
        );
    }
    assert_eq!(
        invoke(
            "start_conversation",
            serde_json::json!({"userId": "@bob:example.invalid"})
        )
        .unwrap(),
        serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"})
    );
    assert_eq!(
        invoke(
            "start_conversation",
            serde_json::json!({"userId": "@nokeys:example.invalid"})
        )
        .unwrap()["kind"],
        "no_identity"
    );
    assert!(
        invoke(
            "start_conversation",
            serde_json::json!({"userId": "@odd:example.invalid"})
        )
        .is_err()
    );
    assert_eq!(
        invoke("search_users", serde_json::json!({"term": "bob"})).unwrap()["users"][0]["userId"],
        "@bob:example.invalid"
    );
    assert!(invoke("search_users", serde_json::json!({"term": "a"})).is_err());
    let page = invoke("public_rooms", serde_json::json!({})).unwrap();
    assert_eq!(page["rooms"][0]["name"], "<b>Lobby</b>");
    assert_eq!(page["next"], "p1");
    assert_eq!(
        invoke(
            "public_rooms",
            serde_json::json!({"server": "other.invalid", "filter": "rust", "since": "p1"})
        )
        .unwrap(),
        serde_json::json!({"rooms": [], "next": null, "total": null})
    );
    assert!(
        invoke("public_rooms", serde_json::json!({"filter": "avatar"})).is_err(),
        "a reply outside the contract never reaches the page"
    );
    assert_eq!(
        invoke(
            "join_public_room",
            serde_json::json!({"roomId": "!pub:example.invalid"})
        )
        .unwrap(),
        serde_json::json!({"roomId": "!pub:example.invalid"})
    );
    assert_eq!(
        invoke(
            "join_public_room",
            serde_json::json!({"roomId": "!other:example.invalid"})
        )
        .unwrap_err(),
        "not_found"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for (cmd, args) in [
        (
            "accept_invite",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
        (
            "decline_invite",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
        (
            "start_conversation",
            serde_json::json!({"userId": "@".repeat(256)}),
        ),
        ("search_users", serde_json::json!({"term": "b".repeat(257)})),
        (
            "public_rooms",
            serde_json::json!({"server": "a".repeat(262)}),
        ),
        (
            "public_rooms",
            serde_json::json!({"filter": "a".repeat(257)}),
        ),
        (
            "public_rooms",
            serde_json::json!({"since": "a".repeat(513)}),
        ),
        (
            "join_public_room",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized arguments never crossed"
    );
}

/// Room, account and profile commands relay only their named arguments,
/// refuse oversized ones (and slots out of range) before the core, and
/// refuse replies outside the contract: an unencrypted room never reaches
/// the page, and a device carries no IP address.
#[test]
fn given_room_account_and_profile_commands_then_bounded_arguments_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let invites = invoke("invite_list", serde_json::json!({})).unwrap();
    assert_eq!(invites["invites"][1]["direct"], false);
    assert_eq!(invites["invites"][1]["inviter"], serde_json::Value::Null);
    assert_eq!(
        invoke(
            "create_room",
            serde_json::json!({"name": "Plans", "invite": ["@bob:example.invalid"]})
        )
        .unwrap(),
        serde_json::json!({"kind": "created", "roomId": "!new:example.invalid"})
    );
    assert_eq!(
        invoke(
            "create_room",
            serde_json::json!({"name": "Plans", "invite": ["@nokeys:example.invalid"]})
        )
        .unwrap(),
        serde_json::json!({"kind": "no_identity", "userId": "@nokeys:example.invalid"})
    );
    assert!(
        invoke(
            "create_room",
            serde_json::json!({"name": "Other", "invite": []})
        )
        .is_err(),
        "an outcome outside the contract"
    );
    assert_eq!(
        invoke(
            "invite_user",
            serde_json::json!({"roomId": "!r:example.invalid", "userId": "@bob:example.invalid"})
        )
        .unwrap()["kind"],
        "invited"
    );
    assert_eq!(
        invoke("display_name", serde_json::json!({})).unwrap()["displayName"],
        "<b>Al</b>"
    );
    assert_eq!(
        invoke("set_display_name", serde_json::json!({"name": ""})).unwrap()["displayName"],
        serde_json::Value::Null
    );
    assert_eq!(
        invoke("set_display_name", serde_json::json!({"name": "Bo"})).unwrap_err(),
        "rejected"
    );
    let devices = invoke("device_list", serde_json::json!({})).unwrap();
    assert_eq!(devices["devices"][0]["current"], true);
    assert!(devices["devices"][0].get("lastSeenIp").is_none());
    assert_eq!(
        invoke("account_settings", serde_json::json!({})).unwrap()["chatTextPercent"],
        120
    );
    let settings_with = |enter_sends: bool, theme: &str| {
        serde_json::json!({"settings": {"enterSends": enter_sends, "chatTextPercent": 100,
            "interfaceTextPercent": 100, "desktopNotifications": false,
            "sendTyping": false, "sendReadReceipts": false, "idleLockMinutes": 0,
            "notificationNames": true, "loadRoomPictures": false, "captureProtection": true, "timelineEvents": all_timeline_events(),
            "theme": theme, "sharePresence": true, "presence": "online"}})
    };
    let settings = |enter_sends: bool| settings_with(enter_sends, "orange");
    assert_eq!(
        invoke("account_settings", serde_json::json!({})).unwrap()["theme"],
        "red"
    );
    assert!(invoke("set_account_settings", settings(true)).is_ok());
    // GIVEN each known theme WHEN saved THEN it crosses; an unknown theme
    // or one in another case does not reach the core.
    for theme in ["red", "blue"] {
        assert!(invoke("set_account_settings", settings_with(true, theme)).is_ok());
    }
    for theme in ["green", "Red", ""] {
        assert!(
            invoke("set_account_settings", settings_with(true, theme)).is_err(),
            "{theme}"
        );
    }
    assert_eq!(
        invoke("set_account_settings", settings(false)).unwrap_err(),
        "locked"
    );
    // Every setting is required: an older page's call does not cross.
    assert!(
        invoke(
            "set_account_settings",
            serde_json::json!({"enterSends": true})
        )
        .is_err()
    );
    assert_eq!(
        invoke("profile_list", serde_json::json!({})).unwrap()["profiles"][1]["slot"],
        2
    );
    assert!(invoke("add_profile", serde_json::json!({})).is_ok());
    assert!(invoke("switch_profile", serde_json::json!({"slot": 2})).is_ok());
    assert_eq!(
        invoke("switch_profile", serde_json::json!({"slot": 3})).unwrap_err(),
        "not_found"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let many: Vec<String> = (0..11).map(|n| format!("@u{n}:s")).collect();
    for (cmd, args) in [
        (
            "create_room",
            serde_json::json!({"name": "x".repeat(256), "invite": []}),
        ),
        (
            "create_room",
            serde_json::json!({"name": "x", "invite": many}),
        ),
        (
            "create_room",
            serde_json::json!({"name": "x", "invite": ["@".repeat(256)]}),
        ),
        (
            "invite_user",
            serde_json::json!({"roomId": "!".repeat(256), "userId": "@b:s"}),
        ),
        (
            "invite_user",
            serde_json::json!({"roomId": "!r:s", "userId": "@".repeat(256)}),
        ),
        (
            "set_display_name",
            serde_json::json!({"name": "a".repeat(257)}),
        ),
        ("switch_profile", serde_json::json!({"slot": 0})),
        ("switch_profile", serde_json::json!({"slot": 9})),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized arguments never crossed"
    );
}

/// The account-change commands (ADR-0019 to ADR-0022) relay only codes,
/// addresses and IDs; no password, code, key or path argument exists. Bad
/// arguments never reach the core, and replies outside the contract are
/// refused.
#[test]
fn given_account_change_commands_then_only_non_secret_arguments_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    assert_eq!(
        invoke(
            "change_password",
            serde_json::json!({"signOutOthers": true})
        )
        .unwrap(),
        "done"
    );
    assert!(
        invoke(
            "change_password",
            serde_json::json!({"signOutOthers": false})
        )
        .is_err(),
        "a code outside the contract"
    );
    assert_eq!(
        invoke("new_sessions", serde_json::json!({})).unwrap()["sessions"][0]["deviceId"],
        "NEWDEV"
    );
    assert_eq!(
        invoke(
            "sign_in_with_oidc",
            serde_json::json!({"homeserver": "https://matrix.example.invalid"})
        )
        .unwrap()["state"],
        "signing_in"
    );
    assert_eq!(
        invoke(
            "discover_homeserver",
            serde_json::json!({"serverName": "example.invalid"})
        )
        .unwrap()["homeserver"],
        "https://matrix.example.invalid/"
    );
    assert_eq!(
        invoke(
            "discover_homeserver",
            serde_json::json!({"serverName": "x".repeat(262)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke("proxy_setting", serde_json::json!({})).unwrap()["proxy"],
        "127.0.0.1:9050"
    );
    for proxy in [serde_json::json!("127.0.0.1:9050"), serde_json::Value::Null] {
        assert_eq!(
            invoke("set_proxy_setting", serde_json::json!({ "proxy": proxy })),
            Ok(serde_json::Value::Null)
        );
    }
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "set_proxy_setting",
            serde_json::json!({"proxy": "h".repeat(301)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    // The diagnostic log (ADR-0058): the switch and status relay; the page's
    // records go as the page's, and too many or too long never reach the
    // core.
    assert_eq!(
        invoke("diagnostics_status", serde_json::json!({})).unwrap()["available"],
        true
    );
    assert_eq!(
        invoke("set_diagnostics", serde_json::json!({"on": true})),
        Ok(serde_json::Value::Bool(true))
    );
    let record = serde_json::json!({"level": "info", "target": "bridge", "message": "ok"});
    assert_eq!(
        invoke(
            "diagnostic_records",
            serde_json::json!({"records": [record]})
        ),
        Ok(serde_json::Value::Null)
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let many = vec![record.clone(); 65];
    let long =
        serde_json::json!({"level": "info", "target": "bridge", "message": "m".repeat(1025)});
    for records in [serde_json::json!(many), serde_json::json!([long])] {
        assert_eq!(
            invoke(
                "diagnostic_records",
                serde_json::json!({ "records": records })
            )
            .unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke("call_devices", serde_json::json!({})).unwrap()["recording"][0]["id"],
        "mic-1"
    );
    assert_eq!(
        invoke("call_devices", serde_json::json!({})).unwrap()["cameras"][0]["id"],
        "cam-1"
    );
    assert_eq!(
        invoke("call_devices", serde_json::json!({})).unwrap()["volume"],
        100,
        "the saved call volume crosses with the devices"
    );
    for args in [
        serde_json::json!({"recording": "mic-1", "playout": null}),
        serde_json::json!({"recording": null, "playout": "spk-1", "camera": "cam-1"}),
        serde_json::json!({"camera": "cam-1"}),
        serde_json::json!({}),
    ] {
        assert_eq!(
            invoke("set_call_devices", args),
            Ok(serde_json::Value::Null)
        );
    }
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({"recording": "m".repeat(513), "playout": null}),
        serde_json::json!({"recording": null, "playout": "s".repeat(513)}),
        serde_json::json!({"recording": "", "playout": null}),
        serde_json::json!({"camera": "c".repeat(513)}),
        serde_json::json!({"camera": ""}),
        serde_json::json!({"camera": "cam\n1"}),
    ] {
        assert_eq!(
            invoke("set_call_devices", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert!(invoke("set_call_devices", serde_json::json!({"recording": 7})).is_err());
    assert!(invoke("set_call_devices", serde_json::json!({"camera": 7})).is_err());
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        invoke(
            "acknowledge_sessions",
            serde_json::json!({"deviceIds": ["NEWDEV"]})
        ),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(
        invoke("remove_device", serde_json::json!({"deviceId": "NEWDEV"})).unwrap(),
        "done"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let eleven: Vec<String> = (0..11).map(|n| format!("D{n}")).collect();
    assert_eq!(
        invoke(
            "acknowledge_sessions",
            serde_json::json!({"deviceIds": eleven})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(
        invoke(
            "remove_device",
            serde_json::json!({"deviceId": "D".repeat(256)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls);
    let listed = invoke("three_pids", serde_json::json!({})).unwrap();
    assert_eq!(listed["threePids"][0]["medium"], "email");
    assert_eq!(
        invoke(
            "request_email_token",
            serde_json::json!({"email": "a@example.invalid"})
        )
        .unwrap(),
        "token_sent"
    );
    assert_eq!(
        invoke(
            "request_phone_token",
            serde_json::json!({"country": "GB", "phoneNumber": "07700900123"})
        )
        .unwrap(),
        "token_sent"
    );
    assert_eq!(
        invoke("add_three_pid", serde_json::json!({})).unwrap(),
        "not_confirmed"
    );
    assert_eq!(
        invoke(
            "remove_three_pid",
            serde_json::json!({"medium": "msisdn", "address": "447700900123"})
        )
        .unwrap(),
        "done"
    );
    assert_eq!(
        invoke("verify_device", serde_json::json!({"deviceId": "OTHER"})).unwrap()["verification"]
            ["flowId"],
        "OTHER"
    );
    assert_eq!(
        invoke("change_recovery_key", serde_json::json!({})).unwrap()["operation"],
        "waiting_for_confirmation"
    );
    let turned_on = invoke("set_profile_passphrase", serde_json::json!({"on": true})).unwrap();
    assert_eq!(turned_on["operation"], "waiting_for_passphrase");
    assert_eq!(turned_on["profilePassphrase"], false, "not changed yet");
    // Only the choice crosses: a passphrase argument or a missing or
    // mistyped choice never reaches the core.
    let calls = core.calls.load(Ordering::SeqCst);
    for args in [
        serde_json::json!({}),
        serde_json::json!({"on": "yes"}),
        serde_json::json!({"on": null}),
    ] {
        assert!(
            invoke("set_profile_passphrase", args).is_err(),
            "bad arguments refused"
        );
    }
    assert!(
        invoke(
            "set_profile_passphrase",
            serde_json::json!({"on": true, "passphrase": "hunter2 hunter2"})
        )
        .is_ok(),
        "unknown arguments are ignored by Tauri and never forwarded"
    );
    assert_eq!(core.calls.load(Ordering::SeqCst), calls + 1);
    assert_eq!(
        invoke("start_new_backup", serde_json::json!({})).unwrap()["operation"],
        "waiting_for_confirmation"
    );
    assert_eq!(
        invoke("notification_level", serde_json::json!({})).unwrap()["level"],
        "custom"
    );
    assert!(
        invoke(
            "set_notification_level",
            serde_json::json!({"level": "mentions"})
        )
        .is_ok()
    );
    let calls = core.calls.load(Ordering::SeqCst);
    for (cmd, args) in [
        (
            "request_email_token",
            serde_json::json!({"email": "a".repeat(255)}),
        ),
        (
            "request_phone_token",
            serde_json::json!({"country": "GBR", "phoneNumber": "1"}),
        ),
        (
            "request_phone_token",
            serde_json::json!({"country": "GB", "phoneNumber": "1".repeat(33)}),
        ),
        (
            "remove_three_pid",
            serde_json::json!({"medium": "email", "address": "a".repeat(255)}),
        ),
        ("verify_device", serde_json::json!({"deviceId": ""})),
        (
            "verify_device",
            serde_json::json!({"deviceId": "D".repeat(256)}),
        ),
        (
            "set_notification_level",
            serde_json::json!({"level": "custom"}),
        ),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    // Missing or unknown kinds of argument fail before the handler runs.
    for (cmd, args) in [
        ("change_password", serde_json::json!({})),
        (
            "remove_three_pid",
            serde_json::json!({"medium": "fax", "address": "1"}),
        ),
        (
            "set_notification_level",
            serde_json::json!({"level": "loud"}),
        ),
    ] {
        assert!(invoke(cmd, args).is_err(), "{cmd}");
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "refused arguments never crossed"
    );
}

/// GIVEN a desktop-notification event from the core WHEN it is dispatched
/// THEN only the toaster sees its names, the latest one waits for the toaster
/// to be installed, no page channel gets anything, and oversized names are
/// dropped.
#[test]
fn given_a_notification_event_then_only_the_toaster_sees_it() {
    use std::sync::Mutex;
    let delivered: Arc<Mutex<Vec<u32>>> = Arc::default();
    let record = delivered.clone();
    let subscriptions = Arc::new(Subscriptions::default());
    let app = configure(
        mock_builder().channel_interceptor(move |_, callback, _, _| {
            record.lock().unwrap().push(callback.0);
            true
        }),
        FakeCore::new(true),
        subscriptions.clone(),
    )
    .build(context())
    .unwrap();
    let window = create_main_window(&app).unwrap();
    get_ipc_response(
        &window,
        request(
            "watch_session",
            app_origin(),
            serde_json::json!({"onChange": "__CHANNEL__:3", "onSoundLevel": "__CHANNEL__:4", "onLevels": "__CHANNEL__:5"}),
        ),
    )
    .unwrap();
    get_ipc_response(
        &window,
        request(
            "subscribe_room",
            app_origin(),
            serde_json::json!({"roomId": "!r:example.invalid", "onChange": "__CHANNEL__:4"}),
        ),
    )
    .unwrap();
    // Before a toaster is set, only the latest notification is kept, and
    // shown once it is.
    for room in ["Earlier", "Plans"] {
        subscriptions.dispatch(Event::Notification {
            room: room.into(),
            sender: "Bob".into(),
            call: false,
        });
    }
    let toasts: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::default();
    let shown = toasts.clone();
    subscriptions.set_toaster(Box::new(move |room, sender, call| {
        shown
            .lock()
            .unwrap()
            .push((room.into(), sender.into(), call));
    }));
    subscriptions.dispatch(Event::Notification {
        room: "Later".into(),
        sender: "Ann".into(),
        call: false,
    });
    subscriptions.dispatch(Event::Notification {
        room: "x".repeat(513),
        sender: "Bob".into(),
        call: false,
    });
    subscriptions.dispatch(Event::Notification {
        room: String::new(),
        sender: "Cy".into(),
        call: true,
    });
    assert_eq!(
        *toasts.lock().unwrap(),
        vec![
            ("Plans".to_owned(), "Bob".to_owned(), false),
            ("Later".to_owned(), "Ann".to_owned(), false),
            (String::new(), "Cy".to_owned(), true)
        ]
    );
    assert!(
        delivered.lock().unwrap().is_empty(),
        "no page channel was told"
    );
}

/// Room settings commands relay a room ID, a filter or one change of a closed
/// set; oversized arguments and unknown change kinds never reach the core,
/// and replies outside the contract are refused.
#[test]
fn given_room_settings_commands_then_bounded_changes_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let settings = invoke(
        "room_settings",
        serde_json::json!({"roomId": "!r:example.invalid"}),
    )
    .unwrap();
    assert_eq!(settings["name"], "<b>Plans</b>");
    assert_eq!(settings["ownLevel"], serde_json::Value::Null);
    for room in ["!big:example.invalid", "!extra:example.invalid"] {
        assert_eq!(
            invoke("room_settings", serde_json::json!({ "roomId": room })).unwrap_err(),
            "core_unavailable",
            "{room}"
        );
    }
    let members = invoke(
        "room_members",
        serde_json::json!({"roomId": "!r:example.invalid", "filter": ""}),
    )
    .unwrap();
    assert_eq!(members["members"][0]["canKick"], true);
    assert_eq!(
        invoke(
            "room_members",
            serde_json::json!({"roomId": "!r:example.invalid", "filter": "left"}),
        )
        .unwrap_err(),
        "core_unavailable"
    );
    for change in [
        serde_json::json!({"kind": "name", "name": "Plans"}),
        serde_json::json!({"kind": "join_rule", "rule": "knock"}),
        serde_json::json!({"kind": "canonical_alias", "alias": null, "altAliases": ["#a:s"]}),
        serde_json::json!({"kind": "level", "key": "kick", "level": 50}),
        serde_json::json!({"kind": "member_level", "userId": "@b:s", "level": -1}),
        serde_json::json!({"kind": "encryption"}),
    ] {
        assert_eq!(
            invoke(
                "change_room",
                serde_json::json!({"roomId": "!r:example.invalid", "change": change.clone()}),
            )
            .unwrap(),
            serde_json::Value::Null,
            "{change}"
        );
    }
    assert_eq!(
        invoke(
            "change_room",
            serde_json::json!({"roomId": "!s:example.invalid", "change": {"kind": "ban", "userId": "@b:s"}}),
        )
        .unwrap_err(),
        "rejected"
    );
    assert_eq!(
        invoke(
            "change_room",
            serde_json::json!({"roomId": "!s:example.invalid", "change": {"kind": "leave"}}),
        )
        .unwrap_err(),
        "not_found"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    let room = "!r:example.invalid";
    for (cmd, args) in [
        (
            "room_settings",
            serde_json::json!({"roomId": "!".repeat(256)}),
        ),
        (
            "room_members",
            serde_json::json!({"roomId": room, "filter": "b".repeat(257)}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "name", "name": "x".repeat(256)}}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "topic", "topic": "x".repeat(4097)}}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "kick", "userId": "@".repeat(256)}}),
        ),
        (
            "change_room",
            serde_json::json!({"roomId": room, "change": {"kind": "canonical_alias", "alias": null,
                "altAliases": vec!["#a:s"; 51]}}),
        ),
    ] {
        assert_eq!(invoke(cmd, args).unwrap_err(), "invalid_argument", "{cmd}");
    }
    // Kinds and fields outside the closed set fail to deserialize before
    // the handler runs.
    for change in [
        serde_json::json!({"kind": "state", "type": "m.room.server_acl", "content": {}}),
        serde_json::json!({"kind": "leave", "forget": true}),
        serde_json::json!({"kind": "level", "key": "ban", "level": 9_007_199_254_740_992_u64}),
        serde_json::json!({"kind": "canonical_alias", "altAliases": []}),
    ] {
        assert!(
            invoke(
                "change_room",
                serde_json::json!({"roomId": room, "change": change.clone()}),
            )
            .is_err(),
            "{change}"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "refused arguments never crossed"
    );
}

/// Identity-change commands relay only the room and user IDs, refuse
/// oversized ones before the core, and refuse a reply outside the contract.
/// Accepting only reports that the core is asking in its own window.
#[test]
fn given_identity_change_commands_then_only_the_ids_cross() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| {
        get_ipc_response(&window, request(cmd, app_origin(), args))
            .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let changes = invoke(
        "identity_changes",
        serde_json::json!({"roomId": "!dm:example.invalid"}),
    )
    .unwrap();
    assert_eq!(changes["changes"][0]["userId"], "@bob:example.invalid");
    assert_eq!(changes["changes"][0]["wasVerified"], true);
    assert!(
        invoke(
            "identity_changes",
            serde_json::json!({"roomId": "!odd:example.invalid"})
        )
        .is_err()
    );
    assert_eq!(
        invoke(
            "accept_identity",
            serde_json::json!({"roomId": "!dm:example.invalid", "userId": "@bob:example.invalid"})
        )
        .unwrap()["operation"],
        "waiting_for_confirmation"
    );
    let calls = core.calls.load(Ordering::SeqCst);
    assert_eq!(
        invoke(
            "identity_changes",
            serde_json::json!({"roomId": "!".repeat(256)})
        )
        .unwrap_err(),
        "invalid_argument"
    );
    for args in [
        serde_json::json!({"roomId": "!".repeat(256), "userId": "@b:s"}),
        serde_json::json!({"roomId": "!r:s", "userId": "@".repeat(256)}),
    ] {
        assert_eq!(
            invoke("accept_identity", args).unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(
        core.calls.load(Ordering::SeqCst),
        calls,
        "oversized IDs never crossed"
    );
}

fn invoke_on(
    window: &tauri::WebviewWindow<MockRuntime>,
    cmd: &str,
    origin: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    get_ipc_response(window, request(cmd, origin, args))
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
}

/// The arguments of each image command with a bad handle or an oversized
/// ID; none of them may reach the core.
fn bad_image_arguments() -> Vec<(&'static str, serde_json::Value)> {
    let long_id = "!".repeat(256);
    let mut cases = vec![
        (
            "load_image",
            serde_json::json!({"roomId": long_id, "eventId": "$image"}),
        ),
        (
            "load_image",
            serde_json::json!({"roomId": "!r:example.invalid", "eventId": "$".repeat(256)}),
        ),
        ("pick_image", serde_json::json!({"roomId": long_id})),
        ("load_room_picture", serde_json::json!({"roomId": long_id})),
        (
            "send_image",
            serde_json::json!({"roomId": long_id, "handle": HANDLE}),
        ),
    ];
    for handle in [
        String::new(),
        HANDLE.to_uppercase(),
        HANDLE[..31].to_owned(),
        format!("{HANDLE}0"),
        // The right length, but not hex.
        "0123456789abcdef0123456789abcdeg".to_owned(),
        // The right byte length with a multi-byte character.
        format!("{}\u{e9}", &HANDLE[..30]),
        "../../../../../../../Windows/x.png".to_owned(),
    ] {
        cases.push((
            "image_band",
            serde_json::json!({"handle": handle, "band": 0}),
        ));
        cases.push(("release_image", serde_json::json!({"handle": handle})));
        cases.push(("discard_image", serde_json::json!({"handle": handle})));
        cases.push((
            "send_image",
            serde_json::json!({"roomId": "!r:example.invalid", "handle": handle}),
        ));
    }
    cases
}

/// GIVEN the image commands WHEN called from the app THEN views and drafts
/// are relayed and checked against the contracts, the core's codes reach the
/// page as fixed strings (`rejected` among them), and a reply outside the
/// contract (an extra URL, file path or handle) is refused.
#[test]
fn given_image_commands_then_handles_cross_and_replies_are_checked() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args| invoke_on(&window, cmd, app_origin(), args);
    let room = "!r:example.invalid";
    let load = |event: &str| {
        invoke(
            "load_image",
            serde_json::json!({"roomId": room, "eventId": event}),
        )
    };
    assert_eq!(
        load("$image"),
        Ok(
            serde_json::json!({"handle": HANDLE, "width": 3, "height": 5, "bands": 2, "bandRows": 4})
        )
    );
    assert_eq!(load("$bad").unwrap_err(), "rejected");
    assert_eq!(load("$busy").unwrap_err(), "limit_exceeded");
    assert_eq!(load("$odd").unwrap_err(), "core_unavailable");

    let picture = |room: &str| invoke("load_room_picture", serde_json::json!({"roomId": room}));
    assert_eq!(
        picture("!pic:example.invalid"),
        Ok(serde_json::json!({
            "pictureId": "0123456789abcdef",
            "image": {"handle": HANDLE, "width": 96, "height": 96, "bands": 1, "bandRows": 96}
        }))
    );
    assert_eq!(picture("!none:example.invalid").unwrap_err(), "not_found");
    assert_eq!(picture("!bad:example.invalid").unwrap_err(), "rejected");
    assert_eq!(
        picture("!url:example.invalid").unwrap_err(),
        "core_unavailable",
        "a picture's URL never reaches the page"
    );

    let draft = invoke("pick_image", serde_json::json!({"roomId": room})).unwrap();
    assert_eq!(draft["kind"], "draft");
    assert_eq!(draft["preview"]["handle"], HANDLE);
    assert_eq!(draft["mime"], "image/png");
    assert_eq!(
        invoke(
            "pick_image",
            serde_json::json!({"roomId": "!cancel:example.invalid"})
        ),
        Ok(serde_json::json!({"kind": "cancelled"}))
    );
    assert_eq!(
        invoke(
            "pick_image",
            serde_json::json!({"roomId": "!path:example.invalid"})
        )
        .unwrap_err(),
        "core_unavailable",
        "a file path never reaches the page"
    );
    assert_eq!(
        invoke(
            "pick_image",
            serde_json::json!({"roomId": "!other:example.invalid"})
        )
        .unwrap_err(),
        "core_unavailable"
    );

    let queued = invoke(
        "send_image",
        serde_json::json!({"roomId": room, "handle": HANDLE}),
    )
    .unwrap();
    assert_eq!(queued["kind"], "image");
    assert_eq!(queued["body"], "");
    assert_eq!(
        invoke(
            "send_image",
            serde_json::json!({"roomId": "!full:example.invalid", "handle": HANDLE})
        )
        .unwrap_err(),
        "limit_exceeded"
    );
    assert_eq!(
        invoke(
            "send_image",
            serde_json::json!({"roomId": room, "handle": "fedcba9876543210fedcba9876543210"})
        )
        .unwrap_err(),
        "core_unavailable",
        "a reply carrying the handle is refused"
    );

    for cmd in ["release_image", "discard_image"] {
        assert_eq!(
            invoke(cmd, serde_json::json!({"handle": HANDLE})),
            Ok(serde_json::Value::Null),
            "{cmd}"
        );
        assert_eq!(
            invoke(
                cmd,
                serde_json::json!({"handle": "fedcba9876543210fedcba9876543210"})
            )
            .unwrap_err(),
            "not_found",
            "{cmd}"
        );
    }
}

/// GIVEN an image band WHEN the page asks for it THEN it is a raw binary
/// response, never JSON: width and rows as little-endian `u32`, then exactly
/// `width * rows * 4` RGBA bytes. A short last band keeps the layout; a
/// missing image or band is a fixed error.
#[test]
fn given_an_image_band_then_the_page_gets_raw_bytes_with_width_and_rows() {
    let app = app();
    let window = create_main_window(&app).unwrap();
    let band = |handle: &str, band: u32| {
        get_ipc_response(
            &window,
            request(
                "image_band",
                app_origin(),
                serde_json::json!({"handle": handle, "band": band}),
            ),
        )
    };
    assert_eq!(BAND_PREFIX_BYTES, 8);
    for (index, rows) in [(0u32, 4u32), (1, 1)] {
        let Ok(tauri::ipc::InvokeResponseBody::Raw(body)) = band(HANDLE, index) else {
            panic!("band {index} is not a raw response");
        };
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), rows);
        let pixels = &body[BAND_PREFIX_BYTES..];
        assert_eq!(pixels.len(), (3 * rows * 4) as usize, "band {index}");
        assert!(
            pixels.iter().enumerate().all(|(i, byte)| *byte == i as u8),
            "band {index} bytes are unchanged"
        );
    }
    assert_eq!(
        band("fedcba9876543210fedcba9876543210", 0).unwrap_err(),
        "not_found"
    );
    assert_eq!(band(HANDLE, 9).unwrap_err(), "invalid_argument");
    // A band index outside u32 is refused by the argument type.
    for args in [
        serde_json::json!({"handle": HANDLE, "band": -1}),
        serde_json::json!({"handle": HANDLE, "band": 4_294_967_296_u64}),
        serde_json::json!({"handle": HANDLE, "band": "0"}),
        serde_json::json!({"handle": HANDLE}),
    ] {
        assert!(
            get_ipc_response(&window, request("image_band", app_origin(), args.clone())).is_err(),
            "{args}"
        );
    }
}

/// GIVEN a call video picture WHEN the page asks for it THEN it is a raw
/// binary response: the picture number as a little-endian `u64`, then the
/// rotation, width and height as little-endian `u32`, then exactly
/// `width * height * 4` RGBA bytes. No newer picture is an empty body, and a
/// core error is a fixed code.
#[test]
fn given_a_call_video_frame_then_the_page_gets_raw_bytes_or_an_empty_body() {
    let app = app();
    let window = create_main_window(&app).unwrap();
    let frame = |tile: u64, after: u64, stream: u64| {
        get_ipc_response(
            &window,
            request(
                "call_video_frame",
                app_origin(),
                serde_json::json!({"tile": tile, "after": after, "stream": stream}),
            ),
        )
    };
    assert_eq!(VIDEO_PREFIX_BYTES, 20);
    let Ok(tauri::ipc::InvokeResponseBody::Raw(body)) = frame(1, 6, 1) else {
        panic!("the picture is not a raw response");
    };
    assert_eq!(u64::from_le_bytes(body[0..8].try_into().unwrap()), 7);
    assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(body[12..16].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(body[16..20].try_into().unwrap()), 1);
    assert_eq!(&body[VIDEO_PREFIX_BYTES..], &(0..8).collect::<Vec<u8>>());
    // No newer picture, or none of the asked stream: an empty raw body,
    // never JSON.
    for (tile, after, stream) in [(1, 7, 1), (1, 9, 1), (2, 0, 1), (1, 0, 2)] {
        let Ok(tauri::ipc::InvokeResponseBody::Raw(body)) = frame(tile, after, stream) else {
            panic!("tile {tile} after {after} stream {stream} is not a raw response");
        };
        assert!(body.is_empty(), "tile {tile} after {after} stream {stream}");
    }
    assert_eq!(frame(3, 0, 1).unwrap_err(), "limit_exceeded");
    // A reply that breaks the contract is a generic error.
    assert_eq!(frame(4, 0, 1).unwrap_err(), "core_unavailable");
}

/// GIVEN a tile outside 0 to 4 or an `after` or `stream` the page cannot
/// hold exactly WHEN `call_video_frame` is called THEN it is refused without calling the
/// core, from the app origin and from a remote one.
#[test]
fn given_bad_call_video_arguments_then_the_ui_refuses_without_calling_the_core() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    for args in [
        serde_json::json!({"tile": 6, "after": 0, "stream": 0}),
        serde_json::json!({"tile": 255, "after": 0, "stream": 0}),
        serde_json::json!({"tile": 0, "after": 9_007_199_254_740_992_u64, "stream": 0}),
        serde_json::json!({"tile": 0, "after": u64::MAX, "stream": 0}),
        serde_json::json!({"tile": 0, "after": 0, "stream": 9_007_199_254_740_992_u64}),
        serde_json::json!({"tile": 0, "after": 0, "stream": u64::MAX}),
    ] {
        assert_eq!(
            invoke_on(&window, "call_video_frame", app_origin(), args.clone()).unwrap_err(),
            "invalid_argument",
            "{args}"
        );
    }
    for args in [
        serde_json::json!({"tile": 256, "after": 0, "stream": 0}),
        serde_json::json!({"tile": -1, "after": 0, "stream": 0}),
        serde_json::json!({"tile": 0, "after": -1, "stream": 0}),
        serde_json::json!({"tile": 0, "after": 0, "stream": -1}),
        serde_json::json!({"tile": "0", "after": 0, "stream": 0}),
        serde_json::json!({"tile": 0, "stream": 0}),
        serde_json::json!({"after": 0, "stream": 0}),
        serde_json::json!({"tile": 0, "after": 0}),
    ] {
        assert!(
            get_ipc_response(
                &window,
                request("call_video_frame", app_origin(), args.clone())
            )
            .is_err(),
            "{args}"
        );
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), 0);
    let ok = serde_json::json!({"tile": 0, "after": 0, "stream": 0});
    for (cmd, args) in [
        ("call_video_frame", ok.clone()),
        ("call_camera", serde_json::json!({"on": true})),
        ("call_screen", serde_json::json!({"on": true})),
    ] {
        assert!(
            get_ipc_response(&window, request(cmd, app_origin(), args.clone())).is_ok(),
            "{cmd} is allowed from the app"
        );
        let calls = core.calls.load(Ordering::SeqCst);
        for origin in ["https://example.org", "http://localhost:1420"] {
            assert!(
                get_ipc_response(&window, request(cmd, origin, args.clone())).is_err(),
                "{cmd} from {origin}"
            );
        }
        assert_eq!(core.calls.load(Ordering::SeqCst), calls, "{cmd}");
    }
}

/// GIVEN an unreachable core WHEN a video picture is asked for THEN the page
/// gets the generic error.
#[test]
fn given_unreachable_core_then_a_call_video_frame_is_a_generic_error() {
    let app = app_with(FakeCore::new(false));
    let window = create_main_window(&app).unwrap();
    assert_eq!(
        invoke_on(
            &window,
            "call_video_frame",
            app_origin(),
            serde_json::json!({"tile": 0, "after": 0, "stream": 0})
        )
        .unwrap_err(),
        "core_unavailable"
    );
}

/// GIVEN malformed handles or oversized room or event IDs WHEN any image
/// command is called THEN the UI refuses it without calling the core.
#[test]
fn given_bad_image_arguments_then_the_ui_refuses_without_calling_the_core() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    for (cmd, args) in bad_image_arguments() {
        assert_eq!(
            invoke_on(&window, cmd, app_origin(), args.clone()).unwrap_err(),
            "invalid_argument",
            "{cmd} {args}"
        );
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), 0);
}

/// GIVEN the image commands WHEN called from a remote origin THEN each is
/// denied by the capability before the core is reached.
#[test]
fn given_image_commands_from_a_remote_origin_then_they_are_denied() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let room = "!r:example.invalid";
    for (cmd, args) in [
        (
            "load_image",
            serde_json::json!({"roomId": room, "eventId": "$image"}),
        ),
        (
            "load_room_picture",
            serde_json::json!({"roomId": "!pic:example.invalid"}),
        ),
        (
            "image_band",
            serde_json::json!({"handle": HANDLE, "band": 0}),
        ),
        ("release_image", serde_json::json!({"handle": HANDLE})),
        ("pick_image", serde_json::json!({"roomId": room})),
        (
            "send_image",
            serde_json::json!({"roomId": room, "handle": HANDLE}),
        ),
        ("discard_image", serde_json::json!({"handle": HANDLE})),
    ] {
        assert!(
            get_ipc_response(&window, request(cmd, app_origin(), args.clone())).is_ok(),
            "{cmd} is allowed from the app"
        );
        let calls = core.calls.load(Ordering::SeqCst);
        for origin in ["https://example.org", "http://localhost:1420"] {
            assert!(
                get_ipc_response(&window, request(cmd, origin, args.clone())).is_err(),
                "{cmd} from {origin}"
            );
        }
        assert_eq!(core.calls.load(Ordering::SeqCst), calls, "{cmd}");
    }
}

/// A `paste_image` request as the page sends it: the image as the raw body
/// and the room in its header.
fn paste_request(origin: &str, room: Option<&[u8]>, body: InvokeBody) -> InvokeRequest {
    let mut headers = tauri::http::HeaderMap::new();
    if let Some(room) = room {
        headers.insert(
            persephone_desktop::PASTE_ROOM_HEADER,
            tauri::http::HeaderValue::from_bytes(room).unwrap(),
        );
    }
    InvokeRequest {
        cmd: "paste_image".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: origin.parse().unwrap(),
        body,
        headers,
        invoke_key: INVOKE_KEY.to_string(),
    }
}

/// GIVEN a pasted image WHEN the page sends it as a raw body with its room
/// THEN exactly those bytes reach the core and the draft comes back, checked
/// against the contract; the core's codes reach the page as fixed strings.
#[test]
fn given_a_pasted_image_then_its_bytes_reach_the_core_and_a_draft_comes_back() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let image = b"\x89PNG\r\n\x1a\n".to_vec();
    let paste = |room: &[u8]| {
        get_ipc_response(
            &window,
            paste_request(app_origin(), Some(room), InvokeBody::Raw(image.clone())),
        )
        .map(|reply| reply.deserialize::<serde_json::Value>().unwrap())
    };
    let draft = paste(b"!r:example.invalid").unwrap();
    assert_eq!(draft["kind"], "draft");
    assert_eq!(draft["preview"]["handle"], HANDLE);
    assert_eq!(
        core.pasted.lock().unwrap().take(),
        Some(("!r:example.invalid".to_owned(), image.clone()))
    );
    assert_eq!(paste(b"!other:example.invalid").unwrap_err(), "not_found");
}

/// GIVEN a paste that is not a raw image of 1 byte to the limit with a room
/// header of at most the room ID limit WHEN sent THEN it is refused as
/// `invalid_argument` and nothing reaches the core; from a remote origin it
/// is denied.
#[test]
fn given_a_paste_outside_its_limits_then_it_never_reaches_the_core() {
    let core = FakeCore::new(true);
    let app = app_with(core.clone());
    let window = create_main_window(&app).unwrap();
    let room: &[u8] = b"!r:example.invalid";
    let long_room = vec![b'!'; 256];
    let image = || InvokeBody::Raw(b"\x89PNG".to_vec());
    let too_long = vec![0; persephone_core_protocol::MAX_PASTED_IMAGE_BYTES as usize + 1];
    for (case, request) in [
        (
            "JSON body",
            paste_request(
                app_origin(),
                Some(room),
                InvokeBody::Json(serde_json::json!({"roomId": "!r:example.invalid"})),
            ),
        ),
        ("no room", paste_request(app_origin(), None, image())),
        (
            "empty room",
            paste_request(app_origin(), Some(b""), image()),
        ),
        (
            "long room",
            paste_request(app_origin(), Some(&long_room), image()),
        ),
        (
            "room not visible ASCII",
            paste_request(app_origin(), Some(b"!r\xe9:example.invalid"), image()),
        ),
        (
            "empty image",
            paste_request(app_origin(), Some(room), InvokeBody::Raw(Vec::new())),
        ),
        (
            "image over the limit",
            paste_request(app_origin(), Some(room), InvokeBody::Raw(too_long.clone())),
        ),
    ] {
        assert_eq!(
            get_ipc_response(&window, request)
                .map(|reply| reply.deserialize::<serde_json::Value>().unwrap()),
            Err(serde_json::json!("invalid_argument")),
            "{case}"
        );
    }
    for origin in ["https://example.org", "http://localhost:1420"] {
        assert!(
            get_ipc_response(&window, paste_request(origin, Some(room), image())).is_err(),
            "from {origin}"
        );
    }
    assert_eq!(core.calls.load(Ordering::SeqCst), 0);
    assert!(core.pasted.lock().unwrap().is_none());
}

/// GIVEN an unreachable core WHEN a band is asked for THEN the page gets the
/// generic error and no bytes.
#[test]
fn given_unreachable_core_then_an_image_band_is_a_generic_error() {
    let app = app_with(FakeCore::new(false));
    let window = create_main_window(&app).unwrap();
    assert_eq!(
        invoke_on(
            &window,
            "image_band",
            app_origin(),
            serde_json::json!({"handle": HANDLE, "band": 0})
        )
        .unwrap_err(),
        "core_unavailable"
    );
}

/// GIVEN a notification with names and one without WHEN its text is made
/// THEN the first names the room and sender, and the second says only that a
/// message arrived.
#[test]
fn given_notifications_with_and_without_names_then_only_named_ones_show_names() {
    assert_eq!(
        persephone_desktop::toast_text("Plans", "@bob:example.invalid"),
        (
            "Plans".to_owned(),
            "New message from @bob:example.invalid".to_owned()
        )
    );
    assert_eq!(
        persephone_desktop::toast_text("", ""),
        ("Persephone".to_owned(), "New message".to_owned())
    );
}

/// GIVEN an incoming call notification WHEN its text is made THEN it says
/// "Incoming call" and names the caller only when the core sent a name; it
/// never carries the room or message text (ADR-0052).
#[test]
fn given_call_notifications_then_the_text_is_incoming_call_and_names_only_when_sent() {
    assert_eq!(
        persephone_desktop::call_toast_text("@bob:example.invalid"),
        (
            "Incoming call".to_owned(),
            "@bob:example.invalid".to_owned()
        )
    );
    assert_eq!(
        persephone_desktop::call_toast_text(""),
        ("Incoming call".to_owned(), "Persephone".to_owned())
    );
}

/// Check local personal commands succeed and remote origins or invalid replies fail.
#[test]
fn personal_commands_cross_only_with_bounded_input_and_typed_replies() {
    let cases = [
        ("room_drafts", serde_json::json!({})),
        (
            "room_draft",
            serde_json::json!({"roomId":"!r:example.invalid"}),
        ),
        (
            "save_room_draft",
            serde_json::json!({"roomId":"!r:example.invalid","draft":{"body":"draft","replyTo":null}}),
        ),
        ("room_preferences", serde_json::json!({})),
        (
            "set_room_preference",
            serde_json::json!({"preference":{"roomId":"!r:example.invalid","favourite":true,"section":"Work"}}),
        ),
        (
            "search_messages",
            serde_json::json!({"term":"<b>term</b>","roomId":null,"filters":{"sender":null,"fromMs":null,"beforeMs":null}}),
        ),
        ("ignored_users", serde_json::json!({})),
        (
            "set_ignored_user",
            serde_json::json!({"userId":"@bob:example.invalid","ignored":true}),
        ),
        (
            "report_abuse",
            serde_json::json!({"roomId":"!r:example.invalid","eventId":"$event","reason":"Spam"}),
        ),
    ];
    for (cmd, body) in cases {
        assert!(call(cmd, app_origin(), body.clone()).is_ok(), "{cmd}");
        assert!(call(cmd, "https://evil.invalid", body).is_err(), "{cmd}");
    }
    assert_eq!(
        call(
            "room_draft",
            app_origin(),
            serde_json::json!({"roomId":"!bad:example.invalid"})
        ),
        Err(serde_json::json!("core_unavailable"))
    );
}

/// GIVEN the module report commands WHEN they run from the app origin and from
/// a foreign origin THEN only the app origin reaches the core, the report keeps
/// its typed shape, and out-of-limit ids are refused before the core.
#[test]
fn module_commands_work_from_the_app_origin_and_refuse_bad_ids_before_relay() {
    let fake = FakeCore::new(true);
    let app = app_with(fake.clone());
    let window = create_main_window(&app).unwrap();
    for (cmd, body) in [
        ("third_party_modules", serde_json::json!({})),
        (
            "acknowledge_modules",
            serde_json::json!({"ids": ["hook.dll\u{1f}unsigned"]}),
        ),
    ] {
        let reply = invoke_on(&window, cmd, app_origin(), body.clone()).unwrap();
        assert_eq!(reply["state"], "ready", "{cmd}");
        assert_eq!(reply["modules"][0]["processes"][1], "web_view", "{cmd}");
        let calls = fake.calls.load(Ordering::SeqCst);
        assert!(
            invoke_on(&window, cmd, "https://evil.invalid", body).is_err(),
            "{cmd}"
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), calls, "{cmd}");
    }
    let calls = fake.calls.load(Ordering::SeqCst);
    let too_many: Vec<String> = (0..129).map(|n| format!("id{n}")).collect();
    for ids in [
        Vec::new(),
        too_many,
        vec!["x".repeat(513)],
        vec![String::new()],
    ] {
        assert_eq!(
            invoke_on(
                &window,
                "acknowledge_modules",
                app_origin(),
                serde_json::json!({ "ids": ids })
            )
            .unwrap_err(),
            "invalid_argument"
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    let largest = vec!["x".repeat(512); 128];
    assert!(
        invoke_on(
            &window,
            "acknowledge_modules",
            app_origin(),
            serde_json::json!({ "ids": largest })
        )
        .is_ok()
    );
}

/// GIVEN the display session command WHEN it runs from the app origin THEN
/// the host answers one of the three sessions without calling the core;
/// WHEN it runs from a foreign origin THEN it is refused.
#[test]
fn display_session_is_answered_by_the_host_from_the_app_origin_only() {
    let fake = FakeCore::new(true);
    let app = app_with(fake.clone());
    let window = create_main_window(&app).unwrap();
    let calls = fake.calls.load(Ordering::SeqCst);
    let reply = invoke_on(
        &window,
        "display_session",
        app_origin(),
        serde_json::json!({}),
    )
    .unwrap();
    assert!(
        ["wayland", "x11", "unknown"].contains(&reply.as_str().unwrap()),
        "{reply}"
    );
    if !cfg!(target_os = "linux") {
        assert_eq!(reply, "unknown");
    }
    assert!(
        invoke_on(
            &window,
            "display_session",
            "https://evil.invalid",
            serde_json::json!({})
        )
        .is_err()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
}

/// Reject invalid personal arguments before they reach the core transport.
#[test]
fn personal_limits_reject_before_relay_and_nested_extra_fields_are_refused() {
    let fake = FakeCore::new(true);
    let app = app_with(fake.clone());
    let window = create_main_window(&app).unwrap();
    let cases = [
        (
            "save_room_draft",
            serde_json::json!({"roomId":"!r:s","draft":{"body":"x".repeat(16385),"replyTo":null}}),
        ),
        (
            "save_room_draft",
            serde_json::json!({"roomId":"!r:s","draft":{"body":"x","replyTo":null,"path":"unsafe"}}),
        ),
        (
            "set_room_preference",
            serde_json::json!({"preference":{"roomId":"!r:s","favourite":true,"section":"é".repeat(33)}}),
        ),
        (
            "search_messages",
            serde_json::json!({"term":"é".repeat(65),"roomId":null,"filters":{"sender":null,"fromMs":null,"beforeMs":null}}),
        ),
        (
            "search_messages",
            serde_json::json!({"term":"  ","roomId":null,"filters":{"sender":null,"fromMs":null,"beforeMs":null}}),
        ),
        (
            "set_ignored_user",
            serde_json::json!({"userId":"x".repeat(256),"ignored":true}),
        ),
        (
            "report_abuse",
            serde_json::json!({"roomId":"!r:s","eventId":"$e","reason":"x".repeat(1001)}),
        ),
        (
            "report_abuse",
            serde_json::json!({"roomId":"!r:s","eventId":"$e","reason":" "}),
        ),
    ];
    for (cmd, body) in cases {
        assert!(
            get_ipc_response(&window, request(cmd, app_origin(), body)).is_err(),
            "{cmd}"
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

/// Check navigation replies and reject remote origins or invalid input before relay.
#[test]
fn navigation_commands_allow_local_views_and_deny_remote_origins_before_relay() {
    let fake = FakeCore::new(true);
    let app = app_with(fake.clone());
    let window = create_main_window(&app).unwrap();
    let filters = serde_json::json!({"sender":null,"fromMs":null,"beforeMs":null});
    for (cmd, body) in [
        (
            "jump_to_date",
            serde_json::json!({"roomId":"!r:s","timestampMs":0}),
        ),
        (
            "event_context",
            serde_json::json!({"roomId":"!r:s","eventId":"$e"}),
        ),
        ("pinned_messages", serde_json::json!({"roomId":"!r:s"})),
        (
            "set_pinned",
            serde_json::json!({"roomId":"!r:s","eventId":"$e","pinned":true}),
        ),
        (
            "attachments",
            serde_json::json!({"roomId":"!r:s","cursor":null,"kind":"all","filters":filters}),
        ),
        ("connection_health", serde_json::json!({})),
    ] {
        assert!(
            invoke_on(&window, cmd, app_origin(), body.clone()).is_ok(),
            "{cmd}"
        );
        let calls = fake.calls.load(Ordering::SeqCst);
        assert!(
            invoke_on(&window, cmd, "https://evil.invalid", body.clone()).is_err(),
            "{cmd}"
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
        for (field, invalid) in [
            ("roomId", serde_json::json!("x".repeat(256))),
            ("eventId", serde_json::json!("x".repeat(1025))),
            ("cursor", serde_json::json!("x".repeat(4097))),
            ("timestampMs", serde_json::json!(9_007_199_254_740_992_u64)),
        ] {
            if body.get(field).is_none() {
                continue;
            }
            let mut bad = body.clone();
            bad[field] = invalid;
            assert!(
                invoke_on(&window, cmd, app_origin(), bad).is_err(),
                "{cmd} {field}"
            );
            assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
        }
    }
    for cmd in ["search_messages", "attachments"] {
        for filters in [
            serde_json::json!({"sender":null,"fromMs":1,"beforeMs":1}),
            serde_json::json!({"sender":"@a:\ns","fromMs":null,"beforeMs":null}),
            serde_json::json!({"sender":null,"fromMs":null,"beforeMs":null,"url":"unsafe"}),
            serde_json::json!({"sender":null,"fromMs":9_007_199_254_740_992_u64,"beforeMs":null}),
            serde_json::json!({"sender":null,"fromMs":null}),
        ] {
            let body = if cmd == "search_messages" {
                serde_json::json!({"term":"text","roomId":null,"filters":filters})
            } else {
                serde_json::json!({"roomId":"!r:s","cursor":null,"kind":"all","filters":filters})
            };
            let calls = fake.calls.load(Ordering::SeqCst);
            assert!(
                invoke_on(&window, cmd, app_origin(), body).is_err(),
                "{cmd}"
            );
            assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
        }
    }
}

/// GIVEN a new app WHEN the page reads or saves settings, or a session
/// changes THEN capture protection starts on, follows only a successful
/// reply or save, and turns on again with the session.
#[test]
fn given_account_settings_then_capture_protection_follows_only_successful_calls() {
    let core = FakeCore::new(true);
    let subscriptions = Arc::new(Subscriptions::default());
    let app = configure(mock_builder(), core.clone(), subscriptions.clone())
        .build(context())
        .unwrap();
    let window = create_main_window(&app).unwrap();
    let invoke = |cmd: &str, args: serde_json::Value| invoke_on(&window, cmd, app_origin(), args);
    let capture = subscriptions.capture();
    assert!(capture.wanted(), "on when the window is created");

    // A reply that turns it off.
    core.capture_protection.store(false, Ordering::SeqCst);
    assert_eq!(
        invoke("account_settings", serde_json::json!({})).unwrap()["captureProtection"],
        false
    );
    assert!(!capture.wanted());

    // A new session turns it on again, before the settings are read.
    subscriptions.dispatch(Event::SessionChanged {});
    assert!(capture.wanted());

    // A saved setting turns it off; the sent value counts.
    let saved = |enter_sends: bool, capture_protection: bool| {
        serde_json::json!({"settings": {"enterSends": enter_sends, "chatTextPercent": 100,
            "interfaceTextPercent": 100, "desktopNotifications": false,
            "sendTyping": false, "sendReadReceipts": false, "idleLockMinutes": 0,
            "notificationNames": true, "loadRoomPictures": false,
            "captureProtection": capture_protection,
            "timelineEvents": all_timeline_events(),
            "theme": "orange", "sharePresence": true, "presence": "online"}})
    };
    assert!(invoke("set_account_settings", saved(true, false)).is_ok());
    assert!(!capture.wanted());

    // A refused save changes nothing, either way.
    assert_eq!(
        invoke("set_account_settings", saved(false, true)).unwrap_err(),
        "locked"
    );
    assert!(!capture.wanted(), "a refused save leaves it off");
    assert!(invoke("set_account_settings", saved(true, true)).is_ok());
    assert!(capture.wanted());
    assert_eq!(
        invoke("set_account_settings", saved(false, false)).unwrap_err(),
        "locked"
    );
    assert!(capture.wanted(), "a refused save leaves it on");

    // A call the page cannot make (a setting missing) changes nothing.
    assert!(invoke("set_account_settings", serde_json::json!({"settings": {}})).is_err());
    assert!(capture.wanted());
}
