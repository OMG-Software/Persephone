//! Bounded control protocol between the core and a contained call worker.
//!
//! Each message is the body of one media-protocol Control frame:
//! `[u8 version][u8 tag][fields]`. Integers are big-endian. A string is a
//! `u16` length and UTF-8 bytes. A byte field is a `u8` length and bytes.
//! Decoding rejects anything outside the limits below. Errors carry no text.

use std::io::{self, Read, Write};

use persephone_media_protocol::{
    FrameError, FrameHeader, FrameKind, ResourceBudgets, read_frame, write_frame,
};
use zeroize::Zeroizing;

pub const VERSION: u8 = 1;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_URL_BYTES: usize = 2048;
pub const MAX_JWT_BYTES: usize = 8192;
pub const MAX_IDENTITY_BYTES: usize = 512;
pub const MAX_TURN_URIS: usize = 8;
pub const MAX_TURN_URI_BYTES: usize = 512;
pub const MAX_TURN_CREDENTIAL_BYTES: usize = 512;
pub const MIN_KEY_BYTES: usize = 16;
pub const MAX_KEY_BYTES: usize = 64;
pub const MAX_PARTICIPANTS: usize = 32;

/// Why a message body was refused. There is no free text on purpose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    UnknownVersion,
    UnknownTag,
    Truncated,
    TrailingBytes,
    /// The message or one field is over its limit.
    TooLarge,
    /// A required field is empty or under its minimum.
    TooShort,
    InvalidUtf8,
    /// A control character, or a non-graphic character in an ASCII field.
    InvalidCharacter,
    /// A bad boolean, failure code, URL scheme or URI prefix.
    InvalidValue,
}

/// A frame key with its key index.
#[derive(Clone, Eq, PartialEq)]
pub struct FrameKeyMsg {
    pub index: u8,
    pub bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for FrameKeyMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameKeyMsg")
            .field("index", &self.index)
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TurnServer {
    pub uri: String,
    pub username: Zeroizing<String>,
    pub credential: Zeroizing<String>,
}

impl std::fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServer")
            .field("uri", &self.uri)
            .field("username_len", &self.username.len())
            .field("credential_len", &self.credential.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ToWorker {
    Join {
        sfu_url: String,
        jwt: Zeroizing<String>,
        turn: Vec<TurnServer>,
        relay_only: bool,
        own_identity: String,
        own_key: FrameKeyMsg,
    },
    SetKey {
        identity: String,
        key: FrameKeyMsg,
    },
    SetOwnKey {
        key: FrameKeyMsg,
    },
    RemoveParticipant {
        identity: String,
    },
    SetMuted {
        muted: bool,
    },
    Leave,
}

impl std::fmt::Debug for ToWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Join {
                sfu_url,
                jwt,
                turn,
                relay_only,
                own_identity,
                own_key,
            } => f
                .debug_struct("Join")
                .field("sfu_url", sfu_url)
                .field("jwt_len", &jwt.len())
                .field("turn", turn)
                .field("relay_only", relay_only)
                .field("own_identity", own_identity)
                .field("own_key", own_key)
                .finish(),
            Self::SetKey { identity, key } => f
                .debug_struct("SetKey")
                .field("identity", identity)
                .field("key", key)
                .finish(),
            Self::SetOwnKey { key } => f.debug_struct("SetOwnKey").field("key", key).finish(),
            Self::RemoveParticipant { identity } => f
                .debug_struct("RemoveParticipant")
                .field("identity", identity)
                .finish(),
            Self::SetMuted { muted } => f.debug_struct("SetMuted").field("muted", muted).finish(),
            Self::Leave => f.write_str("Leave"),
        }
    }
}

/// Why a call worker failed. A closed set with no text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FailureCode {
    Containment = 1,
    SignallingRefused = 2,
    Tls = 3,
    Network = 4,
    Decryption = 5,
    AudioDevice = 6,
    Protocol = 7,
    Internal = 8,
}

impl TryFrom<u8> for FailureCode {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        Ok(match value {
            1 => Self::Containment,
            2 => Self::SignallingRefused,
            3 => Self::Tls,
            4 => Self::Network,
            5 => Self::Decryption,
            6 => Self::AudioDevice,
            7 => Self::Protocol,
            8 => Self::Internal,
            _ => return Err(DecodeError::InvalidValue),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FromWorker {
    /// Containment self-check result. Zero means all controls are present,
    /// as for the media worker.
    Ready {
        containment_bits: u32,
    },
    Connected,
    ParticipantJoined {
        identity: String,
    },
    ParticipantLeft {
        identity: String,
    },
    Speaking {
        identity: String,
        speaking: bool,
    },
    Muted {
        muted: bool,
    },
    Failed {
        code: FailureCode,
    },
    Left,
}

// Tags of core to worker messages.
const TO_JOIN: u8 = 1;
const TO_SET_KEY: u8 = 2;
const TO_SET_OWN_KEY: u8 = 3;
const TO_REMOVE_PARTICIPANT: u8 = 4;
const TO_SET_MUTED: u8 = 5;
const TO_LEAVE: u8 = 6;

// Tags of worker to core messages.
const FROM_READY: u8 = 1;
const FROM_CONNECTED: u8 = 2;
const FROM_PARTICIPANT_JOINED: u8 = 3;
const FROM_PARTICIPANT_LEFT: u8 = 4;
const FROM_SPEAKING: u8 = 5;
const FROM_MUTED: u8 = 6;
const FROM_FAILED: u8 = 7;
const FROM_LEFT: u8 = 8;

#[derive(Clone, Copy)]
enum CharRule {
    /// Any text without control characters.
    NoControl,
    /// ASCII graphic characters only.
    AsciiGraphic,
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        if self.rest.len() < count {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.rest.split_at(count);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidValue),
        }
    }

    /// Reads a string. The length is checked against `max` before the bytes
    /// are touched, and an empty string is refused unless `allow_empty`.
    fn str(
        &mut self,
        max: usize,
        rule: CharRule,
        allow_empty: bool,
    ) -> Result<&'a str, DecodeError> {
        let length = self.take(2)?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        if length > max {
            return Err(DecodeError::TooLarge);
        }
        if length == 0 && !allow_empty {
            return Err(DecodeError::TooShort);
        }
        let text = std::str::from_utf8(self.take(length)?).map_err(|_| DecodeError::InvalidUtf8)?;
        let valid = match rule {
            CharRule::NoControl => !text.chars().any(char::is_control),
            CharRule::AsciiGraphic => text.bytes().all(|byte| byte.is_ascii_graphic()),
        };
        if valid {
            Ok(text)
        } else {
            Err(DecodeError::InvalidCharacter)
        }
    }

    fn identity(&mut self) -> Result<String, DecodeError> {
        Ok(self
            .str(MAX_IDENTITY_BYTES, CharRule::NoControl, false)?
            .to_owned())
    }

    fn secret(&mut self, max: usize, rule: CharRule) -> Result<Zeroizing<String>, DecodeError> {
        Ok(Zeroizing::new(self.str(max, rule, false)?.to_owned()))
    }

    fn key(&mut self) -> Result<FrameKeyMsg, DecodeError> {
        let index = self.u8()?;
        let length = usize::from(self.u8()?);
        if length > MAX_KEY_BYTES {
            return Err(DecodeError::TooLarge);
        }
        if length < MIN_KEY_BYTES {
            return Err(DecodeError::TooShort);
        }
        Ok(FrameKeyMsg {
            index,
            bytes: Zeroizing::new(self.take(length)?.to_vec()),
        })
    }

    fn finish(self) -> Result<(), DecodeError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

/// Starts a reader after the version byte and returns the tag.
fn open(body: &[u8]) -> Result<(u8, Reader<'_>), DecodeError> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(DecodeError::TooLarge);
    }
    let mut reader = Reader { rest: body };
    if reader.u8()? != VERSION {
        return Err(DecodeError::UnknownVersion);
    }
    let tag = reader.u8()?;
    Ok((tag, reader))
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    // A field over u16::MAX is cut here, and the decoder refuses it anyway.
    let bytes = &text.as_bytes()[..text.len().min(usize::from(u16::MAX))];
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_key(out: &mut Vec<u8>, key: &FrameKeyMsg) {
    let bytes = &key.bytes[..key.bytes.len().min(usize::from(u8::MAX))];
    out.push(key.index);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
}

fn validate_sfu_url(url: &str) -> Result<(), DecodeError> {
    if url
        .strip_prefix("wss://")
        .is_some_and(|rest| !rest.is_empty())
    {
        return Ok(());
    }
    #[cfg(feature = "dev-disposable-servers")]
    for prefix in ["ws://127.0.0.1", "ws://[::1]"] {
        if let Some(rest) = url.strip_prefix(prefix)
            && matches!(rest.bytes().next(), None | Some(b':' | b'/'))
        {
            return Ok(());
        }
    }
    Err(DecodeError::InvalidValue)
}

impl ToWorker {
    /// Encodes the message. The buffer holds the JWT, TURN credentials and
    /// frame keys, so it is cleared when dropped.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(vec![VERSION]);
        match self {
            Self::Join {
                sfu_url,
                jwt,
                turn,
                relay_only,
                own_identity,
                own_key,
            } => {
                out.push(TO_JOIN);
                put_str(&mut out, sfu_url);
                put_str(&mut out, jwt);
                out.push(turn.len().min(usize::from(u8::MAX)) as u8);
                for server in turn.iter().take(usize::from(u8::MAX)) {
                    put_str(&mut out, &server.uri);
                    put_str(&mut out, &server.username);
                    put_str(&mut out, &server.credential);
                }
                out.push(u8::from(*relay_only));
                put_str(&mut out, own_identity);
                put_key(&mut out, own_key);
            }
            Self::SetKey { identity, key } => {
                out.push(TO_SET_KEY);
                put_str(&mut out, identity);
                put_key(&mut out, key);
            }
            Self::SetOwnKey { key } => {
                out.push(TO_SET_OWN_KEY);
                put_key(&mut out, key);
            }
            Self::RemoveParticipant { identity } => {
                out.push(TO_REMOVE_PARTICIPANT);
                put_str(&mut out, identity);
            }
            Self::SetMuted { muted } => {
                out.push(TO_SET_MUTED);
                out.push(u8::from(*muted));
            }
            Self::Leave => out.push(TO_LEAVE),
        }
        out
    }

    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let (tag, mut reader) = open(body)?;
        let message = match tag {
            TO_JOIN => {
                let sfu_url = reader.str(MAX_URL_BYTES, CharRule::AsciiGraphic, false)?;
                validate_sfu_url(sfu_url)?;
                let sfu_url = sfu_url.to_owned();
                let jwt = reader.secret(MAX_JWT_BYTES, CharRule::AsciiGraphic)?;
                let count = usize::from(reader.u8()?);
                if count > MAX_TURN_URIS {
                    return Err(DecodeError::TooLarge);
                }
                let mut turn = Vec::with_capacity(count);
                for _ in 0..count {
                    let uri = reader.str(MAX_TURN_URI_BYTES, CharRule::AsciiGraphic, false)?;
                    if !(uri.starts_with("turn:") || uri.starts_with("turns:")) {
                        return Err(DecodeError::InvalidValue);
                    }
                    turn.push(TurnServer {
                        uri: uri.to_owned(),
                        username: reader.secret(MAX_TURN_CREDENTIAL_BYTES, CharRule::NoControl)?,
                        credential: reader
                            .secret(MAX_TURN_CREDENTIAL_BYTES, CharRule::NoControl)?,
                    });
                }
                Self::Join {
                    sfu_url,
                    jwt,
                    turn,
                    relay_only: reader.bool()?,
                    own_identity: reader.identity()?,
                    own_key: reader.key()?,
                }
            }
            TO_SET_KEY => Self::SetKey {
                identity: reader.identity()?,
                key: reader.key()?,
            },
            TO_SET_OWN_KEY => Self::SetOwnKey { key: reader.key()? },
            TO_REMOVE_PARTICIPANT => Self::RemoveParticipant {
                identity: reader.identity()?,
            },
            TO_SET_MUTED => Self::SetMuted {
                muted: reader.bool()?,
            },
            TO_LEAVE => Self::Leave,
            _ => return Err(DecodeError::UnknownTag),
        };
        reader.finish()?;
        Ok(message)
    }
}

impl FromWorker {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Self::Ready { containment_bits } => {
                out.push(FROM_READY);
                out.extend_from_slice(&containment_bits.to_be_bytes());
            }
            Self::Connected => out.push(FROM_CONNECTED),
            Self::ParticipantJoined { identity } => {
                out.push(FROM_PARTICIPANT_JOINED);
                put_str(&mut out, identity);
            }
            Self::ParticipantLeft { identity } => {
                out.push(FROM_PARTICIPANT_LEFT);
                put_str(&mut out, identity);
            }
            Self::Speaking { identity, speaking } => {
                out.push(FROM_SPEAKING);
                put_str(&mut out, identity);
                out.push(u8::from(*speaking));
            }
            Self::Muted { muted } => {
                out.push(FROM_MUTED);
                out.push(u8::from(*muted));
            }
            Self::Failed { code } => {
                out.push(FROM_FAILED);
                out.push(*code as u8);
            }
            Self::Left => out.push(FROM_LEFT),
        }
        out
    }

    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let (tag, mut reader) = open(body)?;
        let message = match tag {
            FROM_READY => Self::Ready {
                containment_bits: reader.u32()?,
            },
            FROM_CONNECTED => Self::Connected,
            FROM_PARTICIPANT_JOINED => Self::ParticipantJoined {
                identity: reader.identity()?,
            },
            FROM_PARTICIPANT_LEFT => Self::ParticipantLeft {
                identity: reader.identity()?,
            },
            FROM_SPEAKING => Self::Speaking {
                identity: reader.identity()?,
                speaking: reader.bool()?,
            },
            FROM_MUTED => Self::Muted {
                muted: reader.bool()?,
            },
            FROM_FAILED => Self::Failed {
                code: FailureCode::try_from(reader.u8()?)?,
            },
            FROM_LEFT => Self::Left,
            _ => return Err(DecodeError::UnknownTag),
        };
        reader.finish()?;
        Ok(message)
    }
}

/// A message type that travels in a Control frame.
///
/// The encoded body is cleared when dropped. On the reading side the
/// media-protocol frame keeps its own copy of the body, which is not cleared;
/// for `ToWorker` that copy exists only in the worker, which holds the same
/// secrets for the call anyway.
pub trait Message: Sized {
    fn to_body(&self) -> Zeroizing<Vec<u8>>;
    fn from_body(body: &[u8]) -> Result<Self, DecodeError>;
}

impl Message for ToWorker {
    fn to_body(&self) -> Zeroizing<Vec<u8>> {
        self.encode()
    }
    fn from_body(body: &[u8]) -> Result<Self, DecodeError> {
        Self::decode(body)
    }
}

impl Message for FromWorker {
    fn to_body(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.encode())
    }
    fn from_body(body: &[u8]) -> Result<Self, DecodeError> {
        Self::decode(body)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReadError {
    Frame(FrameError),
    /// The frame was not a Control frame.
    NotControl,
    Decode(DecodeError),
}

/// Writes one message as a Control frame and flushes it.
pub fn write_message<W: Write, M: Message>(writer: &mut W, message: &M) -> io::Result<()> {
    let body = message.to_body();
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "call message is too large",
        ));
    }
    let header = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, header, &body)
}

/// Reads one Control frame under the control budget and decodes it. The
/// budget permit is held only while the body is decoded.
pub fn read_message<R: Read, M: Message>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<M, ReadError> {
    let frame = read_frame(reader, budgets).map_err(ReadError::Frame)?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReadError::NotControl);
    }
    M::from_body(frame.body()).map_err(ReadError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn key(index: u8, len: usize) -> FrameKeyMsg {
        FrameKeyMsg {
            index,
            bytes: Zeroizing::new(vec![0xA5; len]),
        }
    }

    fn turn(uri: &str) -> TurnServer {
        TurnServer {
            uri: uri.to_owned(),
            username: Zeroizing::new("SECRETUSER".to_owned()),
            credential: Zeroizing::new("SECRETCRED".to_owned()),
        }
    }

    fn join() -> ToWorker {
        ToWorker::Join {
            sfu_url: "wss://sfu.example.org/livekit".to_owned(),
            jwt: Zeroizing::new("SECRETJWT.payload.sig".to_owned()),
            turn: vec![
                turn("turn:turn.example.org:3478"),
                turn("turns:t.example.org"),
            ],
            relay_only: true,
            own_identity: "@alice:example.org:DEVICE".to_owned(),
            own_key: key(3, 16),
        }
    }

    /// Join with one field replaced, to test a limit.
    fn join_with(
        url: &str,
        jwt: &str,
        turns: Vec<TurnServer>,
        identity: &str,
        own_key: FrameKeyMsg,
    ) -> Vec<u8> {
        ToWorker::Join {
            sfu_url: url.to_owned(),
            jwt: Zeroizing::new(jwt.to_owned()),
            turn: turns,
            relay_only: false,
            own_identity: identity.to_owned(),
            own_key,
        }
        .encode()
        .to_vec()
    }

    fn all_to_worker() -> Vec<ToWorker> {
        vec![
            join(),
            ToWorker::SetKey {
                identity: "@bob:example.org:D".to_owned(),
                key: key(255, 64),
            },
            ToWorker::SetOwnKey { key: key(0, 32) },
            ToWorker::RemoveParticipant {
                identity: "@bob:example.org:D".to_owned(),
            },
            ToWorker::SetMuted { muted: true },
            ToWorker::Leave,
        ]
    }

    fn all_from_worker() -> Vec<FromWorker> {
        let mut all = vec![
            FromWorker::Ready {
                containment_bits: 0x8000_0021,
            },
            FromWorker::Connected,
            FromWorker::ParticipantJoined {
                identity: "@bob:example.org:D".to_owned(),
            },
            FromWorker::ParticipantLeft {
                identity: "ünï".to_owned(),
            },
            FromWorker::Speaking {
                identity: "@bob:example.org:D".to_owned(),
                speaking: true,
            },
            FromWorker::Muted { muted: false },
            FromWorker::Left,
        ];
        for code in 1..=8 {
            all.push(FromWorker::Failed {
                code: FailureCode::try_from(code).unwrap(),
            });
        }
        all
    }

    /// GIVEN every core-to-worker and worker-to-core message
    /// WHEN each is encoded and decoded
    /// THEN the original message returns.
    #[test]
    fn given_every_message_when_round_tripped_then_it_is_unchanged() {
        for message in all_to_worker() {
            assert_eq!(ToWorker::decode(&message.encode()), Ok(message));
        }
        for message in all_from_worker() {
            assert_eq!(FromWorker::decode(&message.encode()), Ok(message));
        }
    }

    /// GIVEN a message written as a frame
    /// WHEN it is read back with the control budget
    /// THEN it decodes and the budget is released.
    #[test]
    fn given_framed_message_when_read_then_it_decodes_and_budget_is_released() {
        let mut wire = Vec::new();
        write_message(&mut wire, &join()).unwrap();
        let budgets = ResourceBudgets::default();
        let read: ToWorker = read_message(&mut Cursor::new(wire), &budgets).unwrap();
        assert_eq!(read, join());
        assert_eq!(budgets.aggregate_used(), 0);
    }

    /// GIVEN a frame that is not a Control frame
    /// WHEN it is read as a call message
    /// THEN the read fails with `NotControl`.
    #[test]
    fn given_encoded_image_frame_when_read_then_not_control_is_returned() {
        let mut wire = Vec::new();
        let header = FrameHeader {
            kind: FrameKind::EncodedImage,
            length: 2,
            width: 0,
            height: 0,
        };
        write_frame(&mut wire, header, &[1, 1]).unwrap();
        let result: Result<FromWorker, _> =
            read_message(&mut Cursor::new(wire), &ResourceBudgets::default());
        assert_eq!(result, Err(ReadError::NotControl));
    }

    /// GIVEN a frame whose body is not a valid message
    /// WHEN it is read
    /// THEN the decode error is returned.
    #[test]
    fn given_garbage_control_frame_when_read_then_decode_error_is_returned() {
        let mut wire = Vec::new();
        let header = FrameHeader {
            kind: FrameKind::Control,
            length: 2,
            width: 0,
            height: 0,
        };
        write_frame(&mut wire, header, &[9, 1]).unwrap();
        let result: Result<FromWorker, _> =
            read_message(&mut Cursor::new(wire), &ResourceBudgets::default());
        assert_eq!(result, Err(ReadError::Decode(DecodeError::UnknownVersion)));
    }

    /// GIVEN each field one byte over its limit
    /// WHEN the Join message is decoded
    /// THEN it is rejected as too large.
    #[test]
    fn given_field_over_limit_when_decoded_then_it_is_rejected() {
        let url = format!("wss://{}", "a".repeat(MAX_URL_BYTES + 1 - 6));
        let ok_url = format!("wss://{}", "a".repeat(MAX_URL_BYTES - 6));
        let jwt = "j".repeat(MAX_JWT_BYTES + 1);
        let ok_jwt = "j".repeat(MAX_JWT_BYTES);
        let identity = "i".repeat(MAX_IDENTITY_BYTES + 1);
        let ok_identity = "i".repeat(MAX_IDENTITY_BYTES);
        let uri = format!("turn:{}", "u".repeat(MAX_TURN_URI_BYTES + 1 - 5));
        let ok_uri = format!("turn:{}", "u".repeat(MAX_TURN_URI_BYTES - 5));
        let long_secret = Zeroizing::new("s".repeat(MAX_TURN_CREDENTIAL_BYTES + 1));
        let short = "x";

        let good = join_with(
            &ok_url,
            &ok_jwt,
            vec![turn(&ok_uri)],
            &ok_identity,
            key(0, 64),
        );
        assert!(ToWorker::decode(&good).is_ok());

        let cases = [
            join_with(&url, short, vec![], short, key(0, 16)),
            join_with("wss://h", &jwt, vec![], short, key(0, 16)),
            join_with("wss://h", short, vec![], &identity, key(0, 16)),
            join_with("wss://h", short, vec![turn(&uri)], short, key(0, 16)),
            join_with(
                "wss://h",
                short,
                vec![TurnServer {
                    username: long_secret.clone(),
                    ..turn("turn:a")
                }],
                short,
                key(0, 16),
            ),
            join_with(
                "wss://h",
                short,
                vec![TurnServer {
                    credential: long_secret,
                    ..turn("turn:a")
                }],
                short,
                key(0, 16),
            ),
            join_with(
                "wss://h",
                short,
                (0..=MAX_TURN_URIS).map(|_| turn("turn:a")).collect(),
                short,
                key(0, 16),
            ),
            join_with("wss://h", short, vec![], short, key(0, 65)),
        ];
        for case in cases {
            assert_eq!(ToWorker::decode(&case), Err(DecodeError::TooLarge));
        }
        let long_identity = FromWorker::ParticipantJoined {
            identity: identity.clone(),
        };
        assert_eq!(
            FromWorker::decode(&long_identity.encode()),
            Err(DecodeError::TooLarge)
        );
    }

    /// GIVEN key lengths of 15 and 65 bytes, and an empty identity
    /// WHEN the messages are decoded
    /// THEN each is rejected.
    #[test]
    fn given_bad_key_length_or_empty_identity_when_decoded_then_it_is_rejected() {
        let short = ToWorker::SetOwnKey { key: key(0, 15) }.encode();
        assert_eq!(ToWorker::decode(&short), Err(DecodeError::TooShort));
        let long = ToWorker::SetOwnKey { key: key(0, 65) }.encode();
        assert_eq!(ToWorker::decode(&long), Err(DecodeError::TooLarge));
        let empty = ToWorker::RemoveParticipant {
            identity: String::new(),
        };
        assert_eq!(
            ToWorker::decode(&empty.encode()),
            Err(DecodeError::TooShort)
        );
    }

    /// GIVEN a valid Join message
    /// WHEN it is cut at every byte offset
    /// THEN each shorter input is rejected and none panics.
    #[test]
    fn given_truncated_join_at_every_offset_when_decoded_then_it_is_rejected() {
        let full = join().encode();
        assert!(ToWorker::decode(&full).is_ok());
        for cut in 0..full.len() {
            assert!(ToWorker::decode(&full[..cut]).is_err(), "cut at {cut}");
        }
    }

    /// GIVEN a valid message with one extra byte
    /// WHEN it is decoded
    /// THEN it is rejected for trailing bytes.
    #[test]
    fn given_trailing_byte_when_decoded_then_it_is_rejected() {
        for message in all_to_worker() {
            let mut body = message.encode();
            body.push(0);
            assert_eq!(ToWorker::decode(&body), Err(DecodeError::TrailingBytes));
        }
        for message in all_from_worker() {
            let mut body = message.encode();
            body.push(0);
            assert_eq!(FromWorker::decode(&body), Err(DecodeError::TrailingBytes));
        }
    }

    /// GIVEN an unknown version, an unknown tag, or an oversize body
    /// WHEN it is decoded
    /// THEN it is rejected.
    #[test]
    fn given_unknown_version_tag_or_oversize_body_when_decoded_then_it_is_rejected() {
        assert_eq!(
            ToWorker::decode(&[2, TO_LEAVE]),
            Err(DecodeError::UnknownVersion)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, 0]),
            Err(DecodeError::UnknownTag)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, 7]),
            Err(DecodeError::UnknownTag)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, 9]),
            Err(DecodeError::UnknownTag)
        );
        assert_eq!(FromWorker::decode(&[]), Err(DecodeError::Truncated));
        let oversize = vec![VERSION; MAX_MESSAGE_BYTES + 1];
        assert_eq!(FromWorker::decode(&oversize), Err(DecodeError::TooLarge));
    }

    /// GIVEN a failure code outside the closed set, or a boolean other than 0 or 1
    /// WHEN the message is decoded
    /// THEN it is rejected.
    #[test]
    fn given_invalid_code_or_bool_when_decoded_then_it_is_rejected() {
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_FAILED, 0]),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_FAILED, 9]),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_MUTED, 2]),
            Err(DecodeError::InvalidValue)
        );
    }

    /// GIVEN an identity that is not valid UTF-8
    /// WHEN the message is decoded
    /// THEN it is rejected.
    #[test]
    fn given_invalid_utf8_identity_when_decoded_then_it_is_rejected() {
        let body = [VERSION, FROM_PARTICIPANT_LEFT, 0, 2, 0xC3, 0x28];
        assert_eq!(FromWorker::decode(&body), Err(DecodeError::InvalidUtf8));
    }

    /// GIVEN identities with control characters
    /// WHEN the messages are decoded
    /// THEN they are rejected.
    #[test]
    fn given_control_character_in_identity_when_decoded_then_it_is_rejected() {
        for bad in ["a\nb", "a\0b", "a\u{7f}b", "a\u{85}b"] {
            let message = FromWorker::ParticipantJoined {
                identity: bad.to_owned(),
            };
            assert_eq!(
                FromWorker::decode(&message.encode()),
                Err(DecodeError::InvalidCharacter),
                "{bad:?}"
            );
        }
    }

    /// GIVEN a JWT with a space
    /// WHEN the Join message is decoded
    /// THEN it is rejected as not ASCII graphic.
    #[test]
    fn given_jwt_with_space_when_decoded_then_it_is_rejected() {
        let body = join_with("wss://h", "a b", vec![], "id", key(0, 16));
        assert_eq!(ToWorker::decode(&body), Err(DecodeError::InvalidCharacter));
    }

    /// GIVEN SFU URLs with a plain or lookalike scheme
    /// WHEN the Join message is decoded
    /// THEN every URL other than `wss://` host is rejected.
    #[test]
    fn given_non_wss_sfu_url_when_decoded_then_it_is_rejected() {
        let mut bad = vec![
            "https://sfu.example.org",
            "ws://sfu.example.org",
            "wss://",
            "WSS://sfu.example.org",
            "ws://127.0.0.1.evil.example",
            "ws://[::1]x",
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            bad.extend(["ws://127.0.0.1:7880", "ws://[::1]:7880"]);
        }
        for url in bad {
            let body = join_with(url, "t", vec![], "id", key(0, 16));
            assert_eq!(
                ToWorker::decode(&body),
                Err(DecodeError::InvalidValue),
                "{url}"
            );
        }
    }

    /// GIVEN loopback `ws://` URLs and the dev feature
    /// WHEN the Join message is decoded
    /// THEN it is accepted.
    #[cfg(feature = "dev-disposable-servers")]
    #[test]
    fn given_dev_feature_and_loopback_ws_url_when_decoded_then_it_is_accepted() {
        for url in ["ws://127.0.0.1:7880", "ws://127.0.0.1", "ws://[::1]:7880/x"] {
            let body = join_with(url, "t", vec![], "id", key(0, 16));
            assert!(ToWorker::decode(&body).is_ok(), "{url}");
        }
    }

    /// GIVEN a TURN URI without a turn or turns prefix
    /// WHEN the Join message is decoded
    /// THEN it is rejected.
    #[test]
    fn given_bad_turn_prefix_when_decoded_then_it_is_rejected() {
        for uri in ["stun:s.example.org", "http://x", "turnx:y", "TURN:y"] {
            let body = join_with("wss://h", "t", vec![turn(uri)], "id", key(0, 16));
            assert_eq!(
                ToWorker::decode(&body),
                Err(DecodeError::InvalidValue),
                "{uri}"
            );
        }
    }

    /// GIVEN messages that hold secrets
    /// WHEN they are formatted with Debug
    /// THEN no secret text or key byte value appears.
    #[test]
    fn given_secret_messages_when_debug_formatted_then_no_secret_is_printed() {
        let set_key = ToWorker::SetKey {
            identity: "bob".to_owned(),
            key: FrameKeyMsg {
                index: 1,
                bytes: Zeroizing::new(vec![0xAB; 16]),
            },
        };
        let shown = format!("{:?}\n{:#?}\n{:?}", join(), join(), set_key);
        for secret in [
            "SECRETJWT",
            "SECRETUSER",
            "SECRETCRED",
            "165",
            "171",
            "0xa5",
            "0xab",
        ] {
            assert!(!shown.contains(secret), "leaked {secret}");
        }
        let server = format!("{:?}", turn("turn:a"));
        assert!(!server.contains("SECRET"));
        assert!(server.contains("turn:a"));
    }
}
