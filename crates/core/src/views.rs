//! Bounded room and timeline views, served from synthetic data until the
//! Matrix integration replaces it (P1 "mock bounded views").
//!
//! Projection policy for text leaving the core:
//! * Display names identify people, so they are normalized: bidi formatting and
//!   control characters become U+FFFD, whitespace runs collapse to one space,
//!   and the result is cut to [`MAX_DISPLAY_NAME_CHARS`].
//! * Message bodies keep their text, including bidi characters, which the UI
//!   isolates per message; C0/C1 controls other than tab and newline become
//!   U+FFFD, and bodies are cut to [`MAX_BODY_BYTES`] on a character boundary.
//!
//! Every reply must fit one control frame, so pages are bounded by item count
//! and by serialized size.

use persephone_contracts::{
    RoomEncryption, RoomList, RoomSummary, TimelineItem, TimelinePage, UnixMillis,
};
use persephone_core_protocol::{ErrorCode, MAX_CURSOR_BYTES, MAX_ROOM_ID_BYTES};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_ROOMS: usize = 200;
pub const MAX_PAGE_ITEMS: usize = 100;
/// Serialized page budget, under the 128 KiB control frame with room for the
/// reply envelope.
pub const MAX_PAGE_BYTES: usize = 112 * 1024;
pub const MAX_DISPLAY_NAME_CHARS: usize = 128;
/// The media plan's 16 KiB text limit.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
const CURSOR_PREFIX: &str = "t1.";
/// When the synthetic history starts: 2026-01-01 09:00 UTC.
const FIXTURE_START: u64 = 1_767_258_000_000;
const MINUTE: u64 = 60_000;

/// This computer's clock, or the epoch if the clock is before it.
pub fn now_millis() -> UnixMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    UnixMillis::saturating(millis)
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Normalizes a display name for safe, unambiguous display.
pub fn project_name(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(if c.is_control() || is_bidi_control(c) || c == '\u{FEFF}' {
            '\u{FFFD}'
        } else {
            c
        });
        if out.chars().count() >= MAX_DISPLAY_NAME_CHARS {
            out.push('…');
            break;
        }
    }
    out
}

/// Bounds a message body; see the module documentation.
pub fn project_body(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_BODY_BYTES + 4));
    for c in raw.chars() {
        let c = if c.is_control() && c != '\n' && c != '\t' {
            '\u{FFFD}'
        } else {
            c
        };
        if out.len() + c.len_utf8() > MAX_BODY_BYTES {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

struct Message {
    event_id: String,
    sender: String,
    body: String,
    origin_server_ts: Option<UnixMillis>,
}

struct Room {
    room_id: String,
    raw_name: Option<String>,
    unread: u32,
    messages: Vec<Message>,
}

/// Synthetic rooms and history: ordinary content plus the hostile cases the
/// UI must render inertly. No real accounts or servers.
pub struct Fixtures {
    rooms: Vec<Room>,
}

impl Default for Fixtures {
    fn default() -> Self {
        // Synthetic times: one message every few minutes from FIXTURE_START.
        let message = |room: &str, n: usize, sender: &str, body: String| Message {
            event_id: format!("${room}-{n}"),
            sender: sender.into(),
            body,
            origin_server_ts: UnixMillis::new(FIXTURE_START + 7 * MINUTE * n as u64),
        };
        let planning: Vec<Message> = (0..250)
            .map(|n| {
                message(
                    "planning",
                    n,
                    if n % 2 == 0 {
                        "@alice:example.invalid"
                    } else {
                        "@bob:example.invalid"
                    },
                    format!("Planning note {n}"),
                )
            })
            .collect();
        let hostile_bodies = [
            "<script>alert(1)</script><img src=x onerror=alert(2)>".to_string(),
            "[click here](javascript:alert(3)) and https://example.invalid/phish".to_string(),
            "Bidi: \u{202E}gnp.exe and isolate \u{2067}rtl\u{2069} end".to_string(),
            "Control: bell\u{0007} escape\u{001B}[31m red nul\u{0000}".to_string(),
            "Zero\u{200B}width and \u{FEFF}BOM".to_string(),
            "x".repeat(MAX_BODY_BYTES * 2),
            "🧪 emoji, 日本語, العربية mixed".to_string(),
        ];
        // Hostile times too: none, the epoch, and the largest JavaScript holds
        // exactly, which is past the end of JavaScript's dates.
        let hostile_times = [None, UnixMillis::new(0), UnixMillis::new(UnixMillis::MAX)];
        let hostile: Vec<Message> = hostile_bodies
            .into_iter()
            .enumerate()
            .map(|(n, body)| {
                let mut message = message("hostile", n, "@mallory:example.invalid", body);
                if let Some(time) = hostile_times.get(n) {
                    message.origin_server_ts = *time;
                }
                message
            })
            .collect();
        // Many large bodies: one page must stop at the byte budget, not the count.
        let large: Vec<Message> = (0..60)
            .map(|n| {
                message(
                    "large",
                    n,
                    "@carol:example.invalid",
                    format!("{n}:{}", "y".repeat(8 * 1024)),
                )
            })
            .collect();
        Self {
            rooms: vec![
                Room {
                    room_id: "!planning:example.invalid".into(),
                    raw_name: Some("Project planning".into()),
                    unread: 3,
                    messages: planning,
                },
                Room {
                    room_id: "!hostile:example.invalid".into(),
                    raw_name: Some("<img src=x onerror=alert(1)>".into()),
                    unread: u32::MAX,
                    messages: hostile,
                },
                Room {
                    room_id: "!bidi:example.invalid".into(),
                    raw_name: Some("Admin\u{202E}gnp.exe".into()),
                    unread: 0,
                    messages: vec![],
                },
                Room {
                    room_id: "!long:example.invalid".into(),
                    raw_name: Some("L".repeat(500)),
                    unread: 1,
                    messages: large,
                },
                Room {
                    room_id: "!control:example.invalid".into(),
                    raw_name: Some("Line one\nLine two\u{0007}\t  spaced".into()),
                    unread: 0,
                    messages: vec![],
                },
                Room {
                    room_id: "!unnamed:example.invalid".into(),
                    raw_name: None,
                    unread: 0,
                    messages: vec![],
                },
            ],
        }
    }
}

impl Fixtures {
    /// Adds `count` empty, unnamed rooms, for tests of room limits.
    pub fn with_empty_rooms(mut self, count: usize) -> Self {
        self.rooms.extend((0..count).map(|n| Room {
            room_id: format!("!empty-{n}:example.invalid"),
            raw_name: None,
            unread: 0,
            messages: vec![],
        }));
        self
    }

    pub fn has_room(&self, room_id: &str) -> bool {
        self.rooms.iter().any(|room| room.room_id == room_id)
    }

    /// Appends a synthetic message; returns false for an unknown room.
    pub fn append_message(&mut self, room_id: &str, sender: &str, body: String) -> bool {
        let Some(room) = self.rooms.iter_mut().find(|room| room.room_id == room_id) else {
            return false;
        };
        let n = room.messages.len();
        room.messages.push(Message {
            event_id: format!("$live-{n}"),
            sender: sender.into(),
            body,
            origin_server_ts: Some(now_millis()),
        });
        true
    }

    pub fn room_list(&self) -> RoomList {
        let rooms = self
            .rooms
            .iter()
            .take(MAX_ROOMS)
            .map(|room| RoomSummary {
                room_id: room.room_id.clone(),
                display_name: room.raw_name.as_deref().map(project_name),
                unread_count: room.unread,
                // Synthetic rooms: nothing can be sent to them.
                encryption: RoomEncryption::Unknown,
                contact: None,
            })
            .collect();
        RoomList {
            rooms,
            truncated: self.rooms.len() > MAX_ROOMS,
        }
    }

    /// One page of history older than `cursor`, oldest first. Without a
    /// cursor, the newest page.
    pub fn timeline_page(
        &self,
        room_id: &str,
        cursor: Option<&str>,
    ) -> Result<TimelinePage, ErrorCode> {
        if room_id.len() > MAX_ROOM_ID_BYTES || !room_id.starts_with('!') {
            return Err(ErrorCode::InvalidArgument);
        }
        let room = self
            .rooms
            .iter()
            .find(|room| room.room_id == room_id)
            .ok_or(ErrorCode::NotFound)?;
        let end = match cursor {
            None => room.messages.len(),
            Some(cursor) => parse_cursor(cursor, room.messages.len())?,
        };
        let mut items = Vec::new();
        let mut bytes = 0usize;
        let mut start = end;
        while start > 0 && items.len() < MAX_PAGE_ITEMS {
            let message = &room.messages[start - 1];
            let item = TimelineItem::Message {
                event_id: message.event_id.clone(),
                sender: message.sender.clone(),
                body: project_body(&message.body),
                origin_server_ts: message.origin_server_ts,
                reply_to: None,
            };
            let size = serde_json::to_vec(&item)
                .map_err(|_| ErrorCode::Internal)?
                .len()
                + 1;
            if bytes + size > MAX_PAGE_BYTES {
                break;
            }
            bytes += size;
            items.push(item);
            start -= 1;
        }
        items.reverse();
        Ok(TimelinePage {
            room_id: room_id.into(),
            items,
            next_cursor: (start > 0).then(|| format!("{CURSOR_PREFIX}{start}")),
        })
    }
}

fn parse_cursor(cursor: &str, len: usize) -> Result<usize, ErrorCode> {
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err(ErrorCode::InvalidArgument);
    }
    let digits = cursor
        .strip_prefix(CURSOR_PREFIX)
        .ok_or(ErrorCode::InvalidArgument)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.starts_with('0') {
        return Err(ErrorCode::InvalidArgument);
    }
    let position: usize = digits.parse().map_err(|_| ErrorCode::InvalidArgument)?;
    if position > len {
        return Err(ErrorCode::NotFound);
    }
    Ok(position)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_hostile_names_then_controls_bidi_and_length_are_neutralized() {
        assert_eq!(project_name("Admin\u{202E}gnp.exe"), "Admin\u{FFFD}gnp.exe");
        assert_eq!(
            project_name("  Line one\nLine two\u{0007}\t  spaced "),
            "Line one Line two\u{FFFD} spaced"
        );
        assert_eq!(project_name("\u{FEFF}x\u{2066}y"), "\u{FFFD}x\u{FFFD}y");
        let long = project_name(&"L".repeat(500));
        assert_eq!(long.chars().count(), MAX_DISPLAY_NAME_CHARS + 1);
        assert!(long.ends_with('…'));
        assert_eq!(
            project_name("<img src=x>"),
            "<img src=x>",
            "markup is left for inert rendering"
        );
    }

    #[test]
    fn given_hostile_bodies_then_controls_and_size_are_bounded_but_text_kept() {
        assert_eq!(
            project_body("a\u{0007}b\nc\td\u{001B}"),
            "a\u{FFFD}b\nc\td\u{FFFD}"
        );
        assert_eq!(project_body("keep \u{202E}bidi"), "keep \u{202E}bidi");
        let long = project_body(&"é".repeat(MAX_BODY_BYTES));
        assert!(long.len() <= MAX_BODY_BYTES + '…'.len_utf8());
        assert!(long.ends_with('…'));
    }

    #[test]
    fn given_long_history_then_pages_walk_back_without_gaps_or_overlap() {
        let fixtures = Fixtures::default();
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = fixtures
                .timeline_page("!planning:example.invalid", cursor.as_deref())
                .unwrap();
            assert!(page.items.len() <= MAX_PAGE_ITEMS);
            let ids: Vec<String> = page
                .items
                .iter()
                .map(|item| match item {
                    TimelineItem::Message { event_id, .. } => event_id.clone(),
                    TimelineItem::Gap { .. }
                    | TimelineItem::Notice { .. }
                    | TimelineItem::Image { .. }
                    | TimelineItem::Edit { .. }
                    | TimelineItem::Redaction { .. }
                    | TimelineItem::State { .. } => unreachable!(),
                })
                .collect();
            seen.splice(0..0, ids);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let expected: Vec<String> = (0..250).map(|n| format!("$planning-{n}")).collect();
        assert_eq!(seen, expected);
    }

    /// GIVEN synthetic history WHEN a page is read THEN each message carries
    /// its time, including hostile ones (none, the epoch, the largest safe),
    /// and a live message carries this computer's time.
    #[test]
    fn given_history_then_messages_carry_their_times() {
        let times = |fixtures: &Fixtures, room: &str| -> Vec<Option<u64>> {
            fixtures
                .timeline_page(room, None)
                .unwrap()
                .items
                .iter()
                .map(|item| match item {
                    TimelineItem::Message {
                        origin_server_ts, ..
                    } => origin_server_ts.map(UnixMillis::get),
                    TimelineItem::Gap { .. }
                    | TimelineItem::Notice { .. }
                    | TimelineItem::Image { .. }
                    | TimelineItem::Edit { .. }
                    | TimelineItem::Redaction { .. }
                    | TimelineItem::State { .. } => unreachable!(),
                })
                .collect()
        };
        let mut fixtures = Fixtures::default();
        let planning = times(&fixtures, "!planning:example.invalid");
        assert_eq!(
            planning.last(),
            Some(&Some(FIXTURE_START + 7 * MINUTE * 249))
        );
        assert!(planning.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            times(&fixtures, "!hostile:example.invalid")[..3],
            [None, Some(0), Some(UnixMillis::MAX)]
        );

        let before = now_millis();
        assert!(fixtures.append_message(
            "!planning:example.invalid",
            "@bot:example.invalid",
            "x".into()
        ));
        let live = times(&fixtures, "!planning:example.invalid");
        let live = live.last().unwrap().unwrap();
        assert!(live >= before.get() && live <= now_millis().get());
    }

    #[test]
    fn given_large_bodies_then_a_page_stops_at_the_byte_budget() {
        let fixtures = Fixtures::default();
        let page = fixtures
            .timeline_page("!long:example.invalid", None)
            .unwrap();
        let size = serde_json::to_vec(&page).unwrap().len();
        assert!(
            page.items.len() < 60,
            "count limit would have allowed all 60"
        );
        assert!(size <= MAX_PAGE_BYTES + 512, "{size} bytes");
        assert!(page.next_cursor.is_some());
    }

    #[test]
    fn given_bad_room_or_cursor_then_errors_are_specific() {
        let fixtures = Fixtures::default();
        let page = |room: &str, cursor: Option<&str>| fixtures.timeline_page(room, cursor).err();
        assert_eq!(
            page("!missing:example.invalid", None),
            Some(ErrorCode::NotFound)
        );
        assert_eq!(page("no-bang", None), Some(ErrorCode::InvalidArgument));
        assert_eq!(
            page(&format!("!{}", "r".repeat(300)), None),
            Some(ErrorCode::InvalidArgument)
        );
        for bad in [
            "", "t1.", "t1.0", "t1.01", "t1.-1", "t1.1e3", "t2.5", "t1.５",
        ] {
            assert_eq!(
                page("!planning:example.invalid", Some(bad)),
                Some(ErrorCode::InvalidArgument),
                "{bad:?}"
            );
        }
        assert_eq!(
            page("!planning:example.invalid", Some("t1.251")),
            Some(ErrorCode::NotFound)
        );
        assert_eq!(
            page(
                "!planning:example.invalid",
                Some(&format!("t1.{}", "9".repeat(80)))
            ),
            Some(ErrorCode::InvalidArgument)
        );
    }

    #[test]
    fn given_room_list_then_names_are_projected_and_counts_kept() {
        let list = Fixtures::default().room_list();
        assert!(!list.truncated);
        let bidi = list
            .rooms
            .iter()
            .find(|r| r.room_id == "!bidi:example.invalid")
            .unwrap();
        assert_eq!(bidi.display_name.as_deref(), Some("Admin\u{FFFD}gnp.exe"));
        let unnamed = list
            .rooms
            .iter()
            .find(|r| r.room_id == "!unnamed:example.invalid")
            .unwrap();
        assert_eq!(unnamed.display_name, None);
        assert!(list.rooms.iter().any(|r| r.unread_count == u32::MAX));
    }
}
