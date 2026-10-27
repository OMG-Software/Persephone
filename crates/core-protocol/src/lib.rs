//! Messages between the keyless UI process and the key-holding core
//! (ADR-0004).
//!
//! Each call is one control frame from the UI and one control frame back,
//! using the bounded frame protocol from `persephone-media-protocol`. Control
//! frames are at most 128 KiB, and the receiver rejects an oversized or
//! malformed header before allocating the body. This crate deliberately does
//! not depend on the core, so the UI's dependency graph contains no SDK, store
//! or key code.
//!
//! One call has a second kind of reply: `image_band` is answered by a
//! [`PixelReply`] control frame followed at once by exactly one `RgbaPixels`
//! frame of at most [`MAX_BAND_BYTES`] ([`send_pixel_reply`]). The UI reads
//! the core's frames with [`receive_from_core`], which checks the pixel
//! frame's header against its reply before it allocates the body.

pub use persephone_contracts::RoomChange;
use persephone_media_protocol::{
    FrameError, FrameHeader, FrameKind, MAX_DIMENSION, MIB, RGBA_BYTES_PER_PIXEL, ReceivedFrame,
    ResourceBudgets, read_frame, write_frame,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

/// A command the UI may ask the core to run. Unknown commands and unknown
/// fields fail to decode.
///
/// Every variant is a struct variant, even without arguments: serde does not
/// apply `deny_unknown_fields` to unit variants of an internally tagged enum,
/// so `AppStatus` as a unit variant silently accepted extra fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    AppStatus {},
    RoomList {},
    /// A page of a room's history, older than `cursor` (or the newest page).
    TimelinePage {
        room_id: String,
        /// Required, though it may be null: serde would otherwise treat a
        /// missing field as `None`.
        #[serde(deserialize_with = "Option::deserialize")]
        cursor: Option<String>,
    },
    /// Start sending [`Event::RoomChanged`] for this room.
    Subscribe {
        room_id: String,
    },
    Unsubscribe {
        room_id: String,
    },
    /// Close the protected session: refuse new work, wait for admitted work,
    /// then close the stores. Replies with the resulting store state.
    LockSession {},
    /// Reopen the protected session. The core asks for the profile passphrase
    /// in its own window; nothing secret crosses this channel. Replies at once
    /// with the store state (`unlocking` while the prompt is open); the
    /// outcome arrives as [`Event::SessionChanged`].
    UnlockSession {},
    /// Delete this computer's profile after a forgotten passphrase. Only asks:
    /// the core shows its own warning, and deletes nothing without an explicit
    /// yes there. Replies like `UnlockSession`.
    ResetProfile {},
    /// Sign in to this homeserver. The core asks for the username and password
    /// itself, in a native prompt; nothing secret crosses this channel. Replies
    /// at once with the account status; the outcome arrives as
    /// [`Event::SessionChanged`].
    SignIn {
        homeserver: String,
    },
    /// Register a new account named `username` (a localpart) on this
    /// homeserver, then sign in to it. The core checks that the name is free
    /// first, then asks for the password (twice) and, when the homeserver
    /// needs one, the registration token, in its own windows; nothing secret
    /// crosses this channel. Replies at once like `SignIn`; the outcome, or
    /// why it failed (such as `username_taken`), arrives as
    /// [`Event::SessionChanged`].
    Register {
        homeserver: String,
        username: String,
    },
    /// Sign out: revoke the session on the homeserver if it can be reached,
    /// then delete this device's local profile (keys first). Replies with the
    /// resulting store state.
    SignOut {},
    /// The account's encryption state (codes only).
    SecurityStatus {},
    /// Set up cross-signing and recovery for an account without an identity.
    /// The core asks for the password in its own window and shows the
    /// recovery key in its own window; neither crosses this channel. Replies
    /// at once with the status; progress arrives as [`Event::SecurityChanged`].
    SetUpEncryption {},
    /// Verify this device with the recovery key, asked in the core's own
    /// window. Replies at once with the status, like `SetUpEncryption`.
    RecoverWithKey {},
    /// Queue a text message. Replies at once with the queued message; its
    /// progress arrives as [`Event::RoomChanged`] for the room.
    SendMessage {
        room_id: String,
        body: String,
    },
    /// The room's messages sent from this device in this session.
    PendingMessages {
        room_id: String,
    },
    /// Send a failed or unknown message again, with the same transaction ID.
    RetryMessage {
        local_id: u32,
    },
    /// Drop a message that is not being sent.
    DiscardMessage {
        local_id: u32,
    },
    /// Record the user's consent to send unencrypted messages in this room.
    AllowUnencrypted {
        room_id: String,
    },
    /// Set up recovery alone for a verified identity without it (an earlier
    /// attempt failed). The core shows the new recovery key in its own window.
    SetUpRecovery {},
    /// Ask the account's other devices to verify this one by comparing
    /// emoji. Progress arrives as [`Event::SecurityChanged`].
    VerifyThisDevice {},
    /// Accept a verification another of the account's devices asked for.
    AcceptVerification {
        flow_id: String,
    },
    /// Confirm that the emoji match those on the other device.
    ConfirmVerification {
        flow_id: String,
    },
    /// Cancel a verification; during the comparison, say the emoji differ.
    CancelVerification {
        flow_id: String,
    },
    /// The other person in a direct-message room, and how far this account
    /// trusts them.
    ContactStatus {
        room_id: String,
    },
    /// Ask the other person in this direct-message room to verify each other
    /// by comparing emoji. The core finds the person itself; the request goes
    /// to an existing direct-message room with them, never a new one.
    /// Progress arrives as [`Event::SecurityChanged`].
    VerifyContact {
        room_id: String,
    },
    /// The members of this room whose identity changed (ADR-0007).
    IdentityChanges {
        room_id: String,
    },
    /// Accept the new identity of a member of this room whose identity
    /// changed: pin it, or withdraw the verification of the old one. Only
    /// asks: the core confirms in its own window and changes nothing without
    /// an explicit yes there. Replies at once with the status; the outcome
    /// arrives as [`Event::SecurityChanged`].
    AcceptIdentity {
        room_id: String,
        user_id: String,
    },
    /// Download, decrypt and decode the image of this `m.image` event in the
    /// contained worker (ADR-0009). Replies with an image view: an opaque
    /// handle, the shown size and the number of pixel bands. Nothing is
    /// fetched until the user asks.
    LoadImage {
        room_id: String,
        event_id: String,
    },
    /// One band of rows of a loaded or picked image. Answered by a
    /// [`PixelReply`] and one pixel frame, or by an ordinary error reply.
    ImageBand {
        handle: String,
        band: u32,
    },
    /// Drop a loaded image and its pixels.
    ReleaseImage {
        handle: String,
    },
    /// Ask the user for a PNG or JPEG file to send in this room, in the
    /// core's own file dialog; the page never names a path. Replies with a
    /// draft (a stripped copy and its preview) or `cancelled`.
    PickImage {
        room_id: String,
    },
    /// Queue a picked image (by its draft handle) for this room. Replies with
    /// the queued message, like `SendMessage`.
    SendImage {
        room_id: String,
        handle: String,
    },
    /// Drop a picked image that was not sent.
    DiscardImage {
        handle: String,
    },
    /// Room invitations this account has not answered, direct-message or
    /// not, from this homeserver or another one.
    InviteList {},
    /// Join the room of this invitation.
    AcceptInvite {
        room_id: String,
    },
    /// Refuse this invitation (leave the room).
    DeclineInvite {
        room_id: String,
    },
    /// Create an encrypted, invite-only room with this name and invite these
    /// people (at most [`MAX_INVITEES`]). Refused when one of them has no
    /// cross-signing identity (ADR-0005); nothing is created then.
    CreateRoom {
        name: String,
        invite: Vec<String>,
    },
    /// Invite this person to a joined room. In an encrypted room, refused
    /// when they have no cross-signing identity.
    InviteUser {
        room_id: String,
        user_id: String,
    },
    /// This account's display name, as the homeserver has it.
    DisplayName {},
    /// Set this account's display name; an empty name removes it.
    SetDisplayName {
        name: String,
    },
    /// The devices (sessions) signed in to this account.
    DeviceList {},
    /// This account's settings, kept in its encrypted profile.
    AccountSettings {},
    /// Replace this account's settings. Every setting is a required field,
    /// so the page always sends the whole set.
    SetAccountSettings {
        /// Enter sends a message; otherwise it starts a new line.
        enter_sends: bool,
    },
    /// The local profiles (accounts) on this computer, by slot number. No
    /// path or account name crosses.
    ProfileList {},
    /// Make a new empty profile and switch to it. Replies at once; the core
    /// then closes the window, locks the stores and starts again with the new
    /// profile (ADR-0015).
    AddProfile {},
    /// Switch to the profile in this slot, as `AddProfile` does.
    SwitchProfile {
        slot: u32,
    },
    /// Open a direct-message conversation with this user: an existing room
    /// with just them, or a new encrypted room they are invited to. Refused
    /// when they have no cross-signing identity (ADR-0005).
    StartConversation {
        user_id: String,
    },
    /// Search the homeserver's user directory. The page sends this only when
    /// the user has turned directory search on: the server sees every term.
    SearchUsers {
        term: String,
    },
    /// A joined room's settings: names, addresses, access, encryption,
    /// notifications and power levels, with what this account may change.
    RoomSettings {
        room_id: String,
    },
    /// A joined room's members (joined, invited, asking or banned) whose ID
    /// or display name contains `filter` (empty: all), at most
    /// [`MAX_LISTED_MEMBERS`].
    RoomMembers {
        room_id: String,
        filter: String,
    },
    /// Change one setting of a joined room, or act on one member. The
    /// homeserver decides whether this account may.
    ChangeRoom {
        room_id: String,
        change: RoomChange,
    },
}

/// Longest room topic either side accepts, in bytes.
pub const MAX_TOPIC_BYTES: usize = 4096;

/// Longest room address either side accepts, in bytes (the Matrix limit).
pub const MAX_ALIAS_BYTES: usize = 255;

/// Most published addresses besides the main one.
pub const MAX_ALT_ALIASES: usize = 10;

/// Longest member filter either side accepts, in bytes.
pub const MAX_MEMBER_FILTER_BYTES: usize = 256;

/// Most members a `room_members` reply lists.
pub const MAX_LISTED_MEMBERS: usize = 100;

/// Whether every text in `change` is within its limit. The receiver still
/// validates the content; this only keeps oversized input off the channel.
pub fn room_change_within_limits(change: &RoomChange) -> bool {
    match change {
        RoomChange::Name { name } => name.len() <= MAX_ROOM_NAME_BYTES,
        RoomChange::Topic { topic } => topic.len() <= MAX_TOPIC_BYTES,
        RoomChange::AddAlias { alias } | RoomChange::RemoveAlias { alias } => {
            alias.len() <= MAX_ALIAS_BYTES
        }
        RoomChange::CanonicalAlias { alias, alt_aliases } => {
            alias
                .as_ref()
                .is_none_or(|alias| alias.len() <= MAX_ALIAS_BYTES)
                && alt_aliases.len() <= MAX_ALT_ALIASES
                && alt_aliases
                    .iter()
                    .all(|alias| alias.len() <= MAX_ALIAS_BYTES)
        }
        RoomChange::MemberLevel { user_id, .. }
        | RoomChange::Kick { user_id }
        | RoomChange::Ban { user_id }
        | RoomChange::Unban { user_id } => user_id.len() <= MAX_USER_ID_BYTES,
        RoomChange::JoinRule { .. }
        | RoomChange::HistoryVisibility { .. }
        | RoomChange::GuestAccess { .. }
        | RoomChange::Encryption {}
        | RoomChange::Published { .. }
        | RoomChange::Level { .. }
        | RoomChange::Notifications { .. }
        | RoomChange::Leave {} => true,
    }
}

/// Longest user-directory search term either side accepts, in bytes.
pub const MAX_SEARCH_TERM_BYTES: usize = 256;

/// Most matches a user-directory search returns.
pub const MAX_SEARCH_RESULTS: usize = 10;

/// Longest room name either side accepts, in bytes.
pub const MAX_ROOM_NAME_BYTES: usize = 255;

/// Most people a `create_room` call may invite.
pub const MAX_INVITEES: usize = 10;

/// Longest display name either side accepts, in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 256;

/// Most local profiles (slots 1 to this number) on one computer.
pub const MAX_PROFILES: u32 = 8;

/// Longest event ID either side accepts in a call (the Matrix limit).
pub const MAX_EVENT_ID_BYTES: usize = 255;

/// The exact length of an image handle: 128 random bits as lowercase hex.
pub const IMAGE_HANDLE_BYTES: usize = 32;

/// Most pixel bytes in the one `RgbaPixels` frame after a [`PixelReply`].
pub const MAX_BAND_BYTES: u64 = MIB;

/// Longest user ID either side accepts in a call (the Matrix limit).
pub const MAX_USER_ID_BYTES: usize = 255;

/// Longest verification flow ID either side accepts.
pub const MAX_FLOW_ID_BYTES: usize = 128;

/// Longest message body either side accepts, in bytes (the display limit).
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// Longest username (localpart) either side accepts when registering.
pub const MAX_USERNAME_BYTES: usize = 255;

/// Longest homeserver URL either side accepts in a call.
pub const MAX_HOMESERVER_URL_BYTES: usize = 2048;

/// Most rooms the UI may subscribe to at once.
pub const MAX_SUBSCRIPTIONS: usize = 16;

/// A notification from the core. Events carry no message content: the UI
/// fetches content through bounded calls. A burst of changes to one room
/// coalesces into one pending event, so the core's queue is bounded by the
/// number of subscriptions.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    RoomChanged {
        room_id: String,
    },
    /// The session locked, unlocked or changed state: drop everything shown
    /// from it and ask for `app_status`. Sent before the lock waits for
    /// admitted work, so the UI clears first.
    SessionChanged {},
    /// Rooms were added, removed or changed: fetch the room list again.
    /// Coalesced like room events.
    RoomListChanged {},
    /// The account's encryption state changed: ask for `security_status`.
    /// Coalesced; it does not clear what the UI shows.
    SecurityChanged {},
}

/// Every frame from the core to the UI: a reply to a call, or an event.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreMessage {
    Reply(Reply),
    Event(Event),
}

/// Longest room ID either side accepts in a call (Matrix limits IDs to 255 bytes).
pub const MAX_ROOM_ID_BYTES: usize = 255;
/// Longest opaque timeline cursor either side accepts in a call.
pub const MAX_CURSOR_BYTES: usize = 64;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u32,
    pub call: Call,
}

/// Errors the core reports to the UI. Codes only; no text from the core's
/// internals crosses the boundary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unavailable,
    Internal,
    /// An argument failed validation.
    InvalidArgument,
    /// The named room or cursor does not exist.
    NotFound,
    /// A limit such as [`MAX_SUBSCRIPTIONS`] would be exceeded.
    LimitExceeded,
    /// The session is locked (or locking); protected views are unavailable.
    Locked,
    /// The image was refused: too large, malformed, not PNG or JPEG, it
    /// failed its integrity check, or the server refused the file (a 4xx
    /// status other than 404 or 429). Trying again gives the same answer.
    Rejected,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Ok(serde_json::Value),
    Error(ErrorCode),
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub id: u32,
    pub outcome: Outcome,
}

/// Announces the pixel frame that follows it: `rows` rows of `width` RGBA
/// pixels, band number `band` of the image, for request `id`. On the wire it
/// is `{"pixel_reply":{...}}` in a control frame.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PixelReply {
    pub id: u32,
    pub width: u32,
    pub rows: u32,
    pub band: u32,
}

impl PixelReply {
    /// The pixel frame header this reply announces, or `None` when the band
    /// is empty, too wide or over [`MAX_BAND_BYTES`].
    pub fn frame_header(&self) -> Option<FrameHeader> {
        if self.width == 0 || self.rows == 0 || self.width > MAX_DIMENSION {
            return None;
        }
        let length = u64::from(self.width)
            .checked_mul(u64::from(self.rows))?
            .checked_mul(RGBA_BYTES_PER_PIXEL)
            .filter(|bytes| *bytes <= MAX_BAND_BYTES)?;
        Some(FrameHeader {
            kind: FrameKind::RgbaPixels,
            length,
            width: self.width,
            height: self.rows,
        })
    }
}

/// A [`PixelReply`] and its pixel frame. The frame keeps its budget
/// reservation until it is dropped.
pub struct PixelBand {
    pub reply: PixelReply,
    pub pixels: ReceivedFrame,
}

/// What the UI reads from the core: a message, or a band of pixels.
pub enum FromCore {
    Message(CoreMessage),
    Pixels(PixelBand),
}

/// Every control message from the core: [`CoreMessage`]'s two forms and the
/// pixel reply, in the same externally tagged form.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Wire<R, P> {
    Reply(R),
    Event(Event),
    PixelReply(P),
}

/// The fixed frame header length of `persephone-media-protocol`.
const FRAME_HEADER_LEN: usize = 17;

#[derive(Debug)]
pub enum ReceiveError {
    /// The peer closed the stream, at or within a frame.
    Closed,
    /// The frame header failed validation; its body was not read.
    Frame(FrameError),
    /// The frame was not a control frame.
    UnexpectedKind,
    /// The body was not a valid message.
    Malformed,
}

/// Writes one message as a control frame. Fails without writing anything if
/// the encoded message exceeds the control frame limit.
pub fn send<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    let header = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, header, &body)
}

/// Reads one message. The frame header is validated and the budget reserved
/// before the body is allocated.
pub fn receive<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<T, ReceiveError> {
    let frame = read_frame(reader, budgets).map_err(|error| match error {
        FrameError::Io(_) => ReceiveError::Closed,
        other => ReceiveError::Frame(other),
    })?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReceiveError::UnexpectedKind);
    }
    serde_json::from_slice(frame.body()).map_err(|_| ReceiveError::Malformed)
}

fn frame_error(error: FrameError) -> ReceiveError {
    match error {
        FrameError::Io(_) => ReceiveError::Closed,
        other => ReceiveError::Frame(other),
    }
}

/// Writes a [`PixelReply`] and then its pixel frame. Everything is checked
/// before anything is written: `pixels` must be exactly `width * rows` RGBA
/// pixels and at most [`MAX_BAND_BYTES`]. The caller holds its output lock
/// across this call, so no other frame comes between the two.
pub fn send_pixel_reply<W: Write>(
    writer: &mut W,
    reply: &PixelReply,
    pixels: &[u8],
) -> io::Result<()> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    let header = reply
        .frame_header()
        .ok_or_else(|| invalid("pixel band outside the band limit"))?;
    if u64::try_from(pixels.len()).ok() != Some(header.length) {
        return Err(invalid("pixel band length does not match its reply"));
    }
    FrameHeader::decode(header.encode())
        .map_err(|_| invalid("pixel frame header fails receiver validation"))?;
    let body = serde_json::to_vec(&Wire::<(), &PixelReply>::PixelReply(reply))
        .map_err(io::Error::other)?;
    let control = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, control, &body)?;
    write_frame(writer, header, pixels)
}

/// Reads one message or pixel band from the core (the UI side). After a
/// [`PixelReply`], the next frame must be an `RgbaPixels` frame whose width,
/// height and length match it. That header is checked before the body is
/// allocated, and the body is read with the budgeted reader. A reply over the
/// band limit is refused without reading the frame that follows.
pub fn receive_from_core<R: Read>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<FromCore, ReceiveError> {
    let frame = read_frame(reader, budgets).map_err(frame_error)?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReceiveError::UnexpectedKind);
    }
    let reply = match serde_json::from_slice::<Wire<Reply, PixelReply>>(frame.body()) {
        Ok(Wire::Reply(reply)) => return Ok(FromCore::Message(CoreMessage::Reply(reply))),
        Ok(Wire::Event(event)) => return Ok(FromCore::Message(CoreMessage::Event(event))),
        Ok(Wire::PixelReply(reply)) => reply,
        Err(_) => return Err(ReceiveError::Malformed),
    };
    drop(frame);
    let expected = reply.frame_header().ok_or(ReceiveError::Malformed)?;
    let mut wire = [0; FRAME_HEADER_LEN];
    reader
        .read_exact(&mut wire)
        .map_err(|_| ReceiveError::Closed)?;
    let header = FrameHeader::decode(wire).map_err(ReceiveError::Frame)?;
    if header.kind != FrameKind::RgbaPixels {
        return Err(ReceiveError::UnexpectedKind);
    }
    if header != expected {
        return Err(ReceiveError::Malformed);
    }
    // The header was checked; give it back to the budgeted reader together
    // with the body that follows it.
    let pixels = read_frame(&mut (&wire[..]).chain(reader), budgets).map_err(frame_error)?;
    Ok(FromCore::Pixels(PixelBand { reply, pixels }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: FrameKind, body: &[u8]) -> Vec<u8> {
        let mut wire = FrameHeader {
            kind,
            length: body.len() as u64,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(body);
        wire
    }

    #[test]
    fn given_request_and_reply_when_sent_then_they_round_trip() {
        let mut wire = Vec::new();
        let request = Request {
            id: 7,
            call: Call::AppStatus {},
        };
        send(&mut wire, &request).unwrap();
        assert_eq!(
            String::from_utf8(wire[17..].to_vec()).unwrap(),
            r#"{"id":7,"call":{"command":"app_status"}}"#
        );
        let reply = Reply {
            id: 7,
            outcome: Outcome::Error(ErrorCode::Unavailable),
        };
        send(&mut wire, &reply).unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        assert_eq!(
            receive::<_, Request>(&mut reader, &budgets).unwrap(),
            request
        );
        assert_eq!(receive::<_, Reply>(&mut reader, &budgets).unwrap(), reply);
        assert!(matches!(
            receive::<_, Reply>(&mut reader, &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    #[test]
    fn given_core_messages_when_sent_then_replies_and_events_are_distinguished() {
        let mut wire = Vec::new();
        let reply = CoreMessage::Reply(Reply {
            id: 3,
            outcome: Outcome::Error(ErrorCode::LimitExceeded),
        });
        let event = CoreMessage::Event(Event::RoomChanged {
            room_id: "!r:example.invalid".into(),
        });
        let session = CoreMessage::Event(Event::SessionChanged {});
        let list = CoreMessage::Event(Event::RoomListChanged {});
        let security = CoreMessage::Event(Event::SecurityChanged {});
        send(&mut wire, &reply).unwrap();
        send(&mut wire, &event).unwrap();
        send(&mut wire, &session).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"session_changed":{}}}"#),
            "wire form of the session event"
        );
        send(&mut wire, &list).unwrap();
        send(&mut wire, &security).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"security_changed":{}}}"#),
            "wire form of the security event"
        );
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        for expected in [reply, event, session, list, security] {
            assert_eq!(
                receive::<_, CoreMessage>(&mut reader, &budgets).unwrap(),
                expected
            );
        }
        for body in [
            &br#"{"event":{"room_changed":{"room_id":"!r","body":"x"}}}"#[..],
            br#"{"event":{"message_added":{"room_id":"!r"}}}"#,
            br#"{"reply":{"id":1,"outcome":{"ok":null}},"event":null}"#,
            br#"{"event":{"session_changed":{"state":"locked"}}}"#,
            br#"{"event":"session_changed"}"#,
            br#"{"event":{"room_list_changed":{"rooms":[]}}}"#,
            br#"{"event":{"security_changed":{"identity":"verified"}}}"#,
            br#"{"event":{"security_changed":{"recovery_key":"x"}}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, CoreMessage>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// GIVEN the contact calls WHEN sent THEN only the room ID crosses, in the
    /// documented wire form.
    #[test]
    fn given_contact_calls_then_only_the_room_id_crosses() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::ContactStatus {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"contact_status","room_id":"!r:s"}}"#,
            ),
            (
                Call::VerifyContact {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"verify_contact","room_id":"!r:s"}}"#,
            ),
            (
                Call::IdentityChanges {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"identity_changes","room_id":"!r:s"}}"#,
            ),
            (
                Call::AcceptIdentity {
                    room_id: "!r:s".into(),
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"accept_identity","room_id":"!r:s","user_id":"@b:s"}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
    }

    /// GIVEN the conversation calls WHEN sent THEN only the room ID, user ID
    /// or search term crosses, in the documented wire form.
    #[test]
    fn given_conversation_calls_then_only_their_one_argument_crosses() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::InviteList {},
                r#"{"id":1,"call":{"command":"invite_list"}}"#,
            ),
            (
                Call::AcceptInvite {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"accept_invite","room_id":"!r:s"}}"#,
            ),
            (
                Call::DeclineInvite {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"decline_invite","room_id":"!r:s"}}"#,
            ),
            (
                Call::StartConversation {
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"start_conversation","user_id":"@b:s"}}"#,
            ),
            (
                Call::SearchUsers { term: "bob".into() },
                r#"{"id":1,"call":{"command":"search_users","term":"bob"}}"#,
            ),
            (
                Call::Register {
                    homeserver: "https://h".into(),
                    username: "al".into(),
                },
                r#"{"id":1,"call":{"command":"register","homeserver":"https://h","username":"al"}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
    }

    /// GIVEN the room, account and profile calls WHEN sent THEN only their
    /// documented arguments cross: names, IDs and a slot number, never a path.
    #[test]
    fn given_room_account_and_profile_calls_then_they_have_a_strict_wire_form() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::CreateRoom {
                    name: "Plans".into(),
                    invite: vec!["@b:s".into()],
                },
                r#"{"id":1,"call":{"command":"create_room","name":"Plans","invite":["@b:s"]}}"#,
            ),
            (
                Call::InviteUser {
                    room_id: "!r:s".into(),
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"invite_user","room_id":"!r:s","user_id":"@b:s"}}"#,
            ),
            (
                Call::DisplayName {},
                r#"{"id":1,"call":{"command":"display_name"}}"#,
            ),
            (
                Call::SetDisplayName { name: "Al".into() },
                r#"{"id":1,"call":{"command":"set_display_name","name":"Al"}}"#,
            ),
            (
                Call::DeviceList {},
                r#"{"id":1,"call":{"command":"device_list"}}"#,
            ),
            (
                Call::AccountSettings {},
                r#"{"id":1,"call":{"command":"account_settings"}}"#,
            ),
            (
                Call::SetAccountSettings { enter_sends: false },
                r#"{"id":1,"call":{"command":"set_account_settings","enter_sends":false}}"#,
            ),
            (
                Call::ProfileList {},
                r#"{"id":1,"call":{"command":"profile_list"}}"#,
            ),
            (
                Call::AddProfile {},
                r#"{"id":1,"call":{"command":"add_profile"}}"#,
            ),
            (
                Call::SwitchProfile { slot: 2 },
                r#"{"id":1,"call":{"command":"switch_profile","slot":2}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"create_room","invite":[]}}"#[..],
            br#"{"id":1,"call":{"command":"create_room","name":"x","invite":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"create_room","name":"x","invite":[],"encrypted":false}}"#,
            br#"{"id":1,"call":{"command":"create_room","name":"x","invite":[],"preset":"public_chat"}}"#,
            br#"{"id":1,"call":{"command":"invite_user","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"invite_user","room_id":"!r:s","user_id":"@b:s","reason":"x"}}"#,
            br#"{"id":1,"call":{"command":"display_name","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"set_display_name"}}"#,
            br#"{"id":1,"call":{"command":"set_display_name","name":null}}"#,
            br#"{"id":1,"call":{"command":"device_list","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"delete_device","device_id":"D"}}"#,
            br#"{"id":1,"call":{"command":"account_settings","slot":2}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings"}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":"false"}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":null}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":true,"theme":"x"}}"#,
            br#"{"id":1,"call":{"command":"profile_list","all":true}}"#,
            br#"{"id":1,"call":{"command":"add_profile","dir":"C:\\x"}}"#,
            br#"{"id":1,"call":{"command":"switch_profile"}}"#,
            br#"{"id":1,"call":{"command":"switch_profile","slot":-1}}"#,
            br#"{"id":1,"call":{"command":"switch_profile","slot":"2"}}"#,
            br#"{"id":1,"call":{"command":"switch_profile","path":"C:\\x"}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// GIVEN the room settings calls WHEN sent THEN they cross in the
    /// documented wire form; missing, extra, null, unknown or out-of-range
    /// fields are refused, inside the change as well as around it.
    #[test]
    fn given_room_settings_calls_then_they_have_a_strict_wire_form() {
        use persephone_contracts::{JoinRule, LevelKey, PowerLevel};
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::RoomSettings {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"room_settings","room_id":"!r:s"}}"#,
            ),
            (
                Call::RoomMembers {
                    room_id: "!r:s".into(),
                    filter: "bo".into(),
                },
                r#"{"id":1,"call":{"command":"room_members","room_id":"!r:s","filter":"bo"}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::JoinRule {
                        rule: JoinRule::Knock,
                    },
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"join_rule","rule":"knock"}}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::CanonicalAlias {
                        alias: None,
                        alt_aliases: vec!["#a:s".into()],
                    },
                },
                r##"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"canonical_alias","alias":null,"altAliases":["#a:s"]}}}"##,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::Level {
                        key: LevelKey::NotifyRoom,
                        level: PowerLevel::new(-5).unwrap(),
                    },
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"notify_room","level":-5}}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::MemberLevel {
                        user_id: "@b:s".into(),
                        level: PowerLevel::new(PowerLevel::MAX).unwrap(),
                    },
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"member_level","userId":"@b:s","level":9007199254740991}}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::Leave {},
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"leave"}}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"room_settings"}}"#[..],
            br#"{"id":1,"call":{"command":"room_settings","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"room_members","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"room_members","room_id":"!r:s","filter":null}}"#,
            br#"{"id":1,"call":{"command":"room_members","room_id":"!r:s","filter":"","limit":500}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"name"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"name","name":"x","topic":"y"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"encryption","enabled":false}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"leave","forget":true}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"join_rule","rule":"open"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"canonical_alias","altAliases":[]}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"canonical_alias","alias":null,"alt_aliases":[]}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"ban","level":9007199254740992}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"ban","level":-9007199254740992}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"ban","level":1.5}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"m.room.name","level":1}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"member_level","user_id":"@b:s","level":1}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"kick","userId":"@b:s","reason":"x"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"redact","eventId":"$e"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"notifications","mode":"loud"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"state","type":"m.room.server_acl","content":{}}}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// GIVEN changes with text at and over each limit THEN only those within
    /// every limit pass the length check.
    #[test]
    fn given_room_changes_then_texts_over_their_limit_are_caught() {
        let at = |n: usize| "a".repeat(n);
        for (change, within) in [
            (
                RoomChange::Name {
                    name: at(MAX_ROOM_NAME_BYTES),
                },
                true,
            ),
            (
                RoomChange::Name {
                    name: at(MAX_ROOM_NAME_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::Topic {
                    topic: at(MAX_TOPIC_BYTES),
                },
                true,
            ),
            (
                RoomChange::Topic {
                    topic: at(MAX_TOPIC_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::AddAlias {
                    alias: at(MAX_ALIAS_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::RemoveAlias {
                    alias: at(MAX_ALIAS_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::CanonicalAlias {
                    alias: Some(at(MAX_ALIAS_BYTES)),
                    alt_aliases: vec![at(MAX_ALIAS_BYTES); MAX_ALT_ALIASES],
                },
                true,
            ),
            (
                RoomChange::CanonicalAlias {
                    alias: None,
                    alt_aliases: vec![String::new(); MAX_ALT_ALIASES + 1],
                },
                false,
            ),
            (
                RoomChange::CanonicalAlias {
                    alias: None,
                    alt_aliases: vec![at(MAX_ALIAS_BYTES + 1)],
                },
                false,
            ),
            (
                RoomChange::Ban {
                    user_id: at(MAX_USER_ID_BYTES + 1),
                },
                false,
            ),
            (RoomChange::Leave {}, true),
        ] {
            assert_eq!(room_change_within_limits(&change), within, "{change:?}");
        }
    }

    #[test]
    fn given_unknown_command_field_or_kind_then_receive_refuses() {
        let budgets = ResourceBudgets::default();
        for body in [
            &br#"{"id":1,"call":{"command":"export_keys"}}"#[..],
            br#"{"id":1,"call":{"command":"app_status"},"extra":true}"#,
            br#"{"id":1,"call":{"command":"app_status","arg":1}}"#,
            br#"{"id":-1,"call":{"command":"app_status"}}"#,
            br#"{"id":1,"call":{"command":"room_list","limit":5}}"#,
            br#"{"id":1,"call":{"command":"timeline_page","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"timeline_page","room_id":"!r:s","cursor":null,"x":1}}"#,
            br#"{"id":1,"call":{"command":"lock_session","force":true}}"#,
            br#"{"id":1,"call":{"command":"unlock_session","key":"00"}}"#,
            br#"{"id":1,"call":{"command":"unlock_session","passphrase":"x"}}"#,
            br#"{"id":1,"call":{"command":"reset_profile","confirm":true}}"#,
            br#"{"id":1,"call":{"command":"sign_in"}}"#,
            br#"{"id":1,"call":{"command":"sign_in","homeserver":"https://h","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"sign_in","homeserver":null}}"#,
            br#"{"id":1,"call":{"command":"register","homeserver":"https://h"}}"#,
            br#"{"id":1,"call":{"command":"register","homeserver":"https://h","username":"al","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"register","homeserver":"https://h","username":"al","token":"x"}}"#,
            br#"{"id":1,"call":{"command":"register","username":"al"}}"#,
            br#"{"id":1,"call":{"command":"sign_out","keep_data":true}}"#,
            br#"{"id":1,"call":{"command":"security_status","user":"@a:s"}}"#,
            br#"{"id":1,"call":{"command":"set_up_encryption","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"recover_with_key","recovery_key":"x"}}"#,
            br#"{"id":1,"call":{"command":"export_room_keys"}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x","encrypted":false}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x","txn_id":"t"}}"#,
            br#"{"id":1,"call":{"command":"retry_message","local_id":-1}}"#,
            br#"{"id":1,"call":{"command":"retry_message","local_id":4294967296}}"#,
            br#"{"id":1,"call":{"command":"discard_message","local_id":"1"}}"#,
            br#"{"id":1,"call":{"command":"allow_unencrypted","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"pending_messages"}}"#,
            br#"{"id":1,"call":{"command":"set_up_recovery","passphrase":"x"}}"#,
            br#"{"id":1,"call":{"command":"verify_this_device","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"accept_verification"}}"#,
            br#"{"id":1,"call":{"command":"confirm_verification","flow_id":"f","emoji":[]}}"#,
            br#"{"id":1,"call":{"command":"cancel_verification","flow_id":null}}"#,
            br#"{"id":1,"call":{"command":"trust_device","device_id":"D"}}"#,
            br#"{"id":1,"call":{"command":"contact_status"}}"#,
            br#"{"id":1,"call":{"command":"contact_status","room_id":"!r:s","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_contact"}}"#,
            br#"{"id":1,"call":{"command":"verify_contact","room_id":null}}"#,
            br#"{"id":1,"call":{"command":"verify_contact","room_id":"!r:s","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_contact","room_id":"!r:s","create_room":true}}"#,
            br#"{"id":1,"call":{"command":"verify_user","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"identity_changes"}}"#,
            br#"{"id":1,"call":{"command":"identity_changes","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"accept_identity","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"accept_identity","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"accept_identity","room_id":"!r:s","user_id":"@b:s","confirmed":true}}"#,
            br#"{"id":1,"call":{"command":"pin_identity","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"invite_list","all":true}}"#,
            br#"{"id":1,"call":{"command":"accept_invite"}}"#,
            br#"{"id":1,"call":{"command":"decline_invite","room_id":"!r:s","ban":true}}"#,
            br#"{"id":1,"call":{"command":"start_conversation"}}"#,
            br#"{"id":1,"call":{"command":"start_conversation","user_id":"@b:s","encrypted":false}}"#,
            br#"{"id":1,"call":{"command":"start_conversation","user_id":"@b:s","invite":["@c:s"]}}"#,
            br#"{"id":1,"call":{"command":"search_users"}}"#,
            br#"{"id":1,"call":{"command":"search_users","term":"b","limit":500}}"#,
            br#"{"id":1,"call":{"command":"create_room","name":"x"}}"#,
            b"not json",
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let mut reader = Cursor::new(frame(FrameKind::EncodedImage, b"{}"));
        assert!(matches!(
            receive::<_, Request>(&mut reader, &budgets),
            Err(ReceiveError::UnexpectedKind)
        ));
    }

    /// A header announcing more than the control limit is refused before the
    /// body is read; the stream position proves nothing past the header was
    /// consumed.
    #[test]
    fn given_oversized_control_header_then_body_is_never_read() {
        let mut wire = FrameHeader {
            kind: FrameKind::Control,
            length: 64 * 1024 * 1024,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(&[b'x'; 4096]);
        let mut reader = Cursor::new(wire);
        let result = receive::<_, Request>(&mut reader, &ResourceBudgets::default());
        assert!(matches!(
            result,
            Err(ReceiveError::Frame(FrameError::TooLarge))
        ));
        assert_eq!(reader.position(), 17);
    }

    /// GIVEN the image calls WHEN sent THEN they cross in the documented wire
    /// form and round trip; missing, extra, null or wrongly typed fields are
    /// refused.
    #[test]
    fn given_image_calls_then_they_have_a_strict_wire_form() {
        let budgets = ResourceBudgets::default();
        let handle = "0123456789abcdef0123456789abcdef";
        for (call, wire) in [
            (
                Call::LoadImage {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                },
                r#"{"id":1,"call":{"command":"load_image","room_id":"!r:s","event_id":"$e"}}"#
                    .to_owned(),
            ),
            (
                Call::ImageBand {
                    handle: handle.into(),
                    band: 3,
                },
                format!(
                    r#"{{"id":1,"call":{{"command":"image_band","handle":"{handle}","band":3}}}}"#
                ),
            ),
            (
                Call::ReleaseImage {
                    handle: handle.into(),
                },
                format!(r#"{{"id":1,"call":{{"command":"release_image","handle":"{handle}"}}}}"#),
            ),
            (
                Call::PickImage {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"pick_image","room_id":"!r:s"}}"#.to_owned(),
            ),
            (
                Call::SendImage {
                    room_id: "!r:s".into(),
                    handle: handle.into(),
                },
                format!(
                    r#"{{"id":1,"call":{{"command":"send_image","room_id":"!r:s","handle":"{handle}"}}}}"#
                ),
            ),
            (
                Call::DiscardImage {
                    handle: handle.into(),
                },
                format!(r#"{{"id":1,"call":{{"command":"discard_image","handle":"{handle}"}}}}"#),
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"load_image","room_id":"!r:s"}}"#[..],
            br#"{"id":1,"call":{"command":"load_image","room_id":"!r:s","event_id":null}}"#,
            br#"{"id":1,"call":{"command":"load_image","room_id":"!r:s","event_id":"$e","url":"mxc://s/m"}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h"}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":-1}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":4294967296}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":"0"}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":0,"rows":9}}"#,
            br#"{"id":1,"call":{"command":"release_image"}}"#,
            br#"{"id":1,"call":{"command":"pick_image"}}"#,
            br#"{"id":1,"call":{"command":"pick_image","room_id":"!r:s","path":"C:\\x.png"}}"#,
            br#"{"id":1,"call":{"command":"send_image","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"send_image","handle":"h"}}"#,
            br#"{"id":1,"call":{"command":"send_image","room_id":"!r:s","handle":"h","filename":"a.png"}}"#,
            br#"{"id":1,"call":{"command":"discard_image","handle":1}}"#,
            br#"{"id":1,"call":{"command":"decode_image","bytes":"AAAA"}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        // The new error code's wire form.
        assert_eq!(
            serde_json::to_string(&ErrorCode::Rejected).unwrap(),
            r#""rejected""#
        );
    }

    fn pixels_frame(width: u32, height: u32, body: &[u8]) -> Vec<u8> {
        let mut wire = FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: body.len() as u64,
            width,
            height,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(body);
        wire
    }

    fn pixel_reply_frame(reply: &str) -> Vec<u8> {
        frame(FrameKind::Control, reply.as_bytes())
    }

    /// GIVEN a pixel reply between an event and a reply WHEN the UI reads THEN
    /// it gets the event, the band with its pixels and the reply, in order;
    /// the documented wire form is used, and the old reader refuses it.
    #[test]
    fn given_a_pixel_reply_then_the_band_arrives_whole_between_messages() {
        let mut wire = Vec::new();
        send(&mut wire, &CoreMessage::Event(Event::SecurityChanged {})).unwrap();
        let reply = PixelReply {
            id: 9,
            width: 2,
            rows: 3,
            band: 1,
        };
        let pixels: Vec<u8> = (0..24).collect();
        let start = wire.len();
        send_pixel_reply(&mut wire, &reply, &pixels).unwrap();
        let documented = r#"{"pixel_reply":{"id":9,"width":2,"rows":3,"band":1}}"#;
        let body = start + 17;
        assert_eq!(
            String::from_utf8_lossy(&wire[body..body + documented.len()]),
            documented
        );
        assert_eq!(
            wire[body + documented.len()],
            FrameKind::RgbaPixels as u8,
            "the pixel frame follows at once"
        );
        send(
            &mut wire,
            &CoreMessage::Reply(Reply {
                id: 10,
                outcome: Outcome::Ok(serde_json::Value::Null),
            }),
        )
        .unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire.clone());
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Ok(FromCore::Message(CoreMessage::Event(
                Event::SecurityChanged {}
            )))
        ));
        match receive_from_core(&mut reader, &budgets) {
            Ok(FromCore::Pixels(band)) => {
                assert_eq!(band.reply, reply);
                assert_eq!(band.pixels.body(), &pixels[..]);
                assert_eq!(band.pixels.header().width, 2);
                assert_eq!(band.pixels.header().height, 3);
            }
            _ => panic!("expected a pixel band"),
        }
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Ok(FromCore::Message(CoreMessage::Reply(Reply { id: 10, .. })))
        ));
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Err(ReceiveError::Closed)
        ));
        // A reader that knows only messages refuses the pixel reply.
        let mut reader = Cursor::new(wire[start..].to_vec());
        assert!(matches!(
            receive::<_, CoreMessage>(&mut reader, &budgets),
            Err(ReceiveError::Malformed)
        ));
    }

    /// GIVEN pixel frames that do not match their reply WHEN read THEN each is
    /// refused, and the pixel body is never read.
    #[test]
    fn given_a_mismatched_or_missing_pixel_frame_then_receive_refuses() {
        let budgets = ResourceBudgets::default();
        let reply = r#"{"pixel_reply":{"id":1,"width":2,"rows":2,"band":0}}"#;
        let body = [7u8; 16];
        for (follower, expected) in [
            // Another width, height, or both.
            (pixels_frame(4, 1, &body), "malformed"),
            (pixels_frame(1, 4, &body), "malformed"),
            // A control or encoded frame instead of pixels.
            (frame(FrameKind::Control, b"{}"), "kind"),
            (frame(FrameKind::EncodedImage, &body), "kind"),
            // Nothing at all, or a cut-off header.
            (Vec::new(), "closed"),
            (pixels_frame(2, 2, &body)[..9].to_vec(), "closed"),
        ] {
            let mut wire = pixel_reply_frame(reply);
            let header_end = wire.len() + 17;
            wire.extend_from_slice(&follower);
            let mut reader = Cursor::new(wire);
            let result = receive_from_core(&mut reader, &budgets);
            let matched = match (expected, &result) {
                ("malformed", Err(ReceiveError::Malformed)) => {
                    reader.position() == header_end as u64
                }
                ("kind", Err(ReceiveError::UnexpectedKind)) => {
                    reader.position() == header_end as u64
                }
                ("closed", Err(ReceiveError::Closed)) => true,
                _ => false,
            };
            assert!(matched, "{expected}: {:?}", result.err());
        }
        // A truncated body is the core closing mid-frame.
        let mut wire = pixel_reply_frame(reply);
        wire.extend_from_slice(&pixels_frame(2, 2, &body)[..25]);
        assert!(matches!(
            receive_from_core(&mut Cursor::new(wire), &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    /// GIVEN a pixel reply over the band limit, empty, too wide or with extra
    /// fields WHEN read THEN it is refused before the next frame is touched;
    /// WHEN the core tries to send one THEN nothing is written.
    #[test]
    fn given_an_oversized_or_malformed_pixel_reply_then_nothing_crosses() {
        let budgets = ResourceBudgets::default();
        for reply in [
            // 1024 x 257 x 4 is just over 1 MiB.
            r#"{"pixel_reply":{"id":1,"width":1024,"rows":257,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":0,"rows":1,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":1,"rows":0,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":8193,"rows":1,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":1,"rows":1,"band":0,"x":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":1,"rows":1}}"#,
        ] {
            let mut wire = pixel_reply_frame(reply);
            let end = wire.len() as u64;
            wire.extend_from_slice(&pixels_frame(1, 1, &[0; 4]));
            let mut reader = Cursor::new(wire);
            assert!(
                matches!(
                    receive_from_core(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{reply}"
            );
            assert_eq!(reader.position(), end, "{reply}");
        }
        // The largest band that fits: 1024 x 256.
        let largest = PixelReply {
            id: 1,
            width: 1024,
            rows: 256,
            band: 0,
        };
        let mut wire = Vec::new();
        send_pixel_reply(&mut wire, &largest, &vec![1; 1024 * 256 * 4]).unwrap();
        assert!(matches!(
            receive_from_core(&mut Cursor::new(wire), &budgets),
            Ok(FromCore::Pixels(_))
        ));
        for (reply, length) in [
            (
                PixelReply {
                    rows: 257,
                    ..largest
                },
                1024 * 257 * 4,
            ),
            (PixelReply { rows: 2, ..largest }, 1024 * 4),
            (
                PixelReply {
                    width: 0,
                    ..largest
                },
                0,
            ),
        ] {
            let mut wire = Vec::new();
            assert!(send_pixel_reply(&mut wire, &reply, &vec![0; length]).is_err());
            assert!(wire.is_empty(), "{reply:?}");
        }
    }

    #[test]
    fn given_message_over_control_limit_then_send_writes_nothing() {
        let mut wire = Vec::new();
        let huge = serde_json::Value::String("x".repeat(200 * 1024));
        assert!(send(&mut wire, &huge).is_err());
        assert!(wire.is_empty());
    }
}
