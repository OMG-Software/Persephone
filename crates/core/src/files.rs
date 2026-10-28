//! Files other than images (ADR-0029): sent from the core's own file dialog
//! and saved through its own Save dialog. Nothing is opened, previewed,
//! played or decoded.
//!
//! * A picked file is read like a picked image (`pick::read_image_file`: no
//!   link, no directory, at most
//!   [`MAX_SENT_BYTES`](crate::media::MAX_SENT_BYTES), checked before it is
//!   read) and queued at once: the dialog, titled for the room, is the user's choice to send it. It is
//!   sent as `application/octet-stream` with its file name, encrypted in an
//!   encrypted room by the SDK.
//! * `m.file`, `m.audio` and `m.video` events are `file` items: a name
//!   projected like a display name, the declared size, and whether the file
//!   is encrypted. Nothing is fetched until the user saves one.
//! * Saving downloads and decrypts like an image, then asks where to save in
//!   the core's own dialog, suggesting the file's name made safe for a file
//!   system, and writes it. The page never names a path.

use crate::media::{Encoded, MAX_ENCODED_BYTES};
use crate::views::project_name;
use matrix_sdk::{
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    ruma::{
        UInt,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            room::{MediaSource, message::MessageType},
        },
    },
};
use persephone_contracts::{TimelineItem, UnixMillis};
use persephone_core_protocol::ErrorCode;
use persephone_media_protocol::{BudgetClass, BudgetPermit, ResourceBudgets};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Longest file name sent, suggested or shown, in characters.
pub const MAX_FILE_NAME_CHARS: usize = 120;

/// A picked file waiting to be sent: its bytes, reserved in the encoded
/// budget, and its name.
pub struct FileUpload {
    bytes: Encoded,
    name: String,
    budgets: Arc<ResourceBudgets>,
}

impl FileUpload {
    /// The upload of `bytes` read from `path`, named after the path's final
    /// part made safe ([`safe_name`]).
    pub fn new(bytes: Encoded, path: &Path, budgets: Arc<ResourceBudgets>) -> Self {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map_or_else(|| "file".to_owned(), safe_name);
        Self {
            bytes,
            name,
            budgets,
        }
    }

    /// A copy of the bytes for the SDK, reserved in `OutgoingSend` before it
    /// is allocated, together with the copies the SDK makes while it uploads
    /// (all but the outbox's of [`SENT_COPIES`](crate::media::SENT_COPIES)).
    /// Keep the permit until the send ends.
    pub fn copy_for_send(&self) -> Result<(Vec<u8>, BudgetPermit), ErrorCode> {
        let length = self.bytes.bytes().len();
        let permit = self
            .budgets
            .try_acquire(
                BudgetClass::OutgoingSend,
                (crate::media::SENT_COPIES - 1) * length as u64,
            )
            .map_err(|_| ErrorCode::LimitExceeded)?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(length)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        copy.extend_from_slice(self.bytes.bytes());
        Ok((copy, permit))
    }

    pub fn bytes(&self) -> &[u8] {
        self.bytes.bytes()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Windows device names, which no file may be called, with or without an
/// extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A file name safe to send and to suggest when saving: projected like a
/// name, with path separators and characters file systems refuse replaced,
/// no leading or trailing dots or spaces, not a Windows device name, at most
/// [`MAX_FILE_NAME_CHARS`]; `file` when nothing is left.
pub fn safe_name(raw: &str) -> String {
    let projected = project_name(raw);
    let replaced: String = projected
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\u{FFFD}' | '…' => '_',
            c => c,
        })
        .take(MAX_FILE_NAME_CHARS)
        .collect();
    let trimmed = replaced.trim_matches(|c: char| c == '.' || c == ' ');
    if trimmed.is_empty() {
        return "file".to_owned();
    }
    let stem = trimmed.split('.').next().unwrap_or_default();
    if RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        return format!("_{trimmed}");
    }
    trimmed.to_owned()
}

/// What a file event holds: its source, name and declared size.
pub struct FileContent {
    pub source: MediaSource,
    pub name: String,
    pub size: Option<UInt>,
}

/// The file content of a decrypted (or plain) `m.file`, `m.audio` or
/// `m.video` event; `None` for anything else, including an event that could
/// not be decrypted.
pub fn file_content(event: &TimelineEvent) -> Option<FileContent> {
    if matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }) {
        return None;
    }
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(message),
    ))) = event.raw().deserialize()
    else {
        return None;
    };
    file_parts(&message.content.msgtype)
}

/// The parts of a file message type; see [`file_content`].
pub fn file_parts(msgtype: &MessageType) -> Option<FileContent> {
    let (source, name, size) = match msgtype {
        MessageType::File(file) => (
            file.source.clone(),
            file.filename.as_deref().unwrap_or(&file.body),
            file.info.as_ref().and_then(|info| info.size),
        ),
        MessageType::Audio(audio) => (
            audio.source.clone(),
            audio.filename.as_deref().unwrap_or(&audio.body),
            audio.info.as_ref().and_then(|info| info.size),
        ),
        MessageType::Video(video) => (
            video.source.clone(),
            video.filename.as_deref().unwrap_or(&video.body),
            video.info.as_ref().and_then(|info| info.size),
        ),
        _ => return None,
    };
    Some(FileContent {
        source,
        name: safe_name(name),
        size,
    })
}

/// The timeline item for a file event. Nothing is fetched.
pub fn timeline_item(
    event_id: String,
    sender: String,
    origin_server_ts: Option<UnixMillis>,
    file: &FileContent,
) -> TimelineItem {
    TimelineItem::File {
        event_id,
        sender,
        origin_server_ts,
        name: file.name.clone(),
        size: file
            .size
            .and_then(|size| u32::try_from(u64::from(size)).ok())
            .filter(|size| *size > 0),
        encrypted: matches!(file.source, MediaSource::Encrypted(_)),
    }
}

/// Whether a declared size is over the limit a file may have.
pub fn too_large(size: Option<UInt>) -> bool {
    size.is_some_and(|size| u64::from(size) > MAX_ENCODED_BYTES)
}

/// Writes a saved file where the user chose in the core's Save dialog. The
/// dialog asked before replacing a file. The bytes go to a new file with a
/// random name beside it, which only its owner can read from the start
/// (`platform::os::create_private_file`), flushed to disk, which then takes
/// the destination's place in one step (`platform::os::replace_file`): on
/// Windows a file already there keeps its DACL; otherwise the saved file
/// stays readable by its owner only. A failed write (a full disk, no
/// permission, a file in use) leaves a file already there unchanged and
/// removes the partial one, with one exception on Windows: should
/// `ReplaceFileW` move the old file to its backup name and moving it back
/// fail too, the old content stays whole at that backup name
/// (`.persephone-<random>.old` beside it). A backup that cannot be deleted
/// after a save stays there the same way. A failure is `unavailable`, not
/// `rejected`, which means the file itself was refused.
pub fn write(path: &Path, bytes: &[u8]) -> Result<(), ErrorCode> {
    let dir = path.parent().ok_or(ErrorCode::Unavailable)?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| ErrorCode::Unavailable)?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let partial = dir.join(format!(".persephone-{suffix}.part"));
    let written =
        write_new(&partial, bytes).and_then(|()| crate::platform::os::replace_file(&partial, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.map_err(|_| ErrorCode::Unavailable)
}

/// Creates `path`, which must not exist and only its owner can read, with
/// `bytes`, flushed to disk.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = crate::platform::os::create_private_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// GIVEN a new name and a file already there WHEN saved THEN each holds
    /// exactly the new bytes, and no partial file is left beside them.
    #[test]
    fn given_a_save_then_the_file_holds_the_bytes_and_nothing_else_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new.pdf");
        let old = dir.path().join("old.pdf");
        std::fs::write(&old, b"the old file, longer than the new one").unwrap();
        write(&new, b"new").unwrap();
        write(&old, b"replaced").unwrap();
        assert_eq!(std::fs::read(&new).unwrap(), b"new");
        assert_eq!(std::fs::read(&old).unwrap(), b"replaced");
        assert_eq!(entries(dir.path()), ["new.pdf", "old.pdf"]);
    }

    /// GIVEN a destination that cannot be replaced (a folder with a file in
    /// it) or a folder that does not exist WHEN saved THEN it is
    /// `unavailable`, what was there is unchanged, and the partial file,
    /// fully written before the rename failed, is removed.
    #[test]
    fn given_a_save_that_fails_then_what_was_there_is_kept_and_nothing_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir.path().join("taken.pdf");
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("inside.txt"), b"kept").unwrap();
        assert_eq!(write(&taken, b"new"), Err(ErrorCode::Unavailable));
        assert_eq!(std::fs::read(taken.join("inside.txt")).unwrap(), b"kept");
        assert_eq!(entries(dir.path()), ["taken.pdf"]);
        assert_eq!(
            write(&dir.path().join("missing").join("x.pdf"), b"new"),
            Err(ErrorCode::Unavailable)
        );
        assert_eq!(entries(dir.path()), ["taken.pdf"]);
    }

    /// GIVEN hostile and odd file names WHEN made safe THEN separators,
    /// reserved characters, controls, bidirectional controls, leading and
    /// trailing dots and device names cannot reach a file system, and an
    /// empty name becomes `file`.
    #[test]
    fn given_file_names_then_they_are_safe_to_save() {
        for (raw, safe) in [
            ("report.pdf", "report.pdf"),
            ("../../Windows/System32/x.dll", "_.._Windows_System32_x.dll"),
            ("C:\\boot.ini", "C__boot.ini"),
            ("evil\u{202E}fdp.exe", "evil_fdp.exe"),
            ("a<b>c:d\"e|f?g*h", "a_b_c_d_e_f_g_h"),
            ("  .hidden. ", "hidden"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("", "file"),
            ("...", "file"),
            ("line\nbreak", "line break"),
        ] {
            assert_eq!(safe_name(raw), safe, "{raw:?}");
        }
        assert!(safe_name(&"x".repeat(500)).chars().count() <= MAX_FILE_NAME_CHARS);
    }

    fn event(content: serde_json::Value) -> TimelineEvent {
        TimelineEvent::from_plaintext(
            matrix_sdk::ruma::serde::Raw::from_json_string(
                serde_json::json!({
                    "type": "m.room.message", "event_id": "$f", "sender": "@a:example.org",
                    "origin_server_ts": 1, "content": content,
                })
                .to_string(),
            )
            .unwrap(),
        )
    }

    /// GIVEN file, audio, video and text events WHEN read THEN the first
    /// three give a safe name, size and source, and text gives none.
    #[test]
    fn given_file_events_then_their_parts_are_read() {
        for msgtype in ["m.file", "m.audio", "m.video"] {
            let content = file_content(&event(serde_json::json!({
                "msgtype": msgtype, "body": "notes/../x.txt",
                "url": "mxc://example.org/abc", "info": { "size": 1234 },
            })))
            .unwrap();
            assert_eq!(content.name, "notes_.._x.txt", "{msgtype}");
            assert_eq!(content.size.map(u64::from), Some(1234));
            let TimelineItem::File {
                name,
                size,
                encrypted,
                ..
            } = timeline_item("$f".into(), "@a:example.org".into(), None, &content)
            else {
                panic!("a file item");
            };
            assert_eq!(
                (name.as_str(), size, encrypted),
                ("notes_.._x.txt", Some(1234), false)
            );
        }
        assert!(
            file_content(&event(
                serde_json::json!({ "msgtype": "m.text", "body": "hi" })
            ))
            .is_none()
        );
        assert!(too_large(UInt::new(MAX_ENCODED_BYTES + 1)));
        assert!(!too_large(UInt::new(MAX_ENCODED_BYTES)));
    }
}
