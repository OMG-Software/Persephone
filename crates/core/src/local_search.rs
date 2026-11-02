//! Bounded substring search of the SDK's encrypted local event cache.
//!
//! No method in this module makes an HTTP request, starts pagination, or
//! requests a room key. Only text already decrypted by the SDK is searched.
//! Gaps and undecrypted events make coverage partial. No search index is kept.
//! Sync can change the cache during a read. Results describe the bounded
//! cached events observed, and can become stale before they are displayed.

use std::collections::{BTreeMap, BTreeSet};

use crate::{abuse, matrix_views, store::Stores};
use matrix_sdk::{
    Client, RoomState,
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    linked_chunk::{ChunkContent, LinkedChunkId},
    ruma::{
        OwnedEventId, OwnedUserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            room::message::{MessageType, Relation, sanitize::remove_plain_reply_fallback},
        },
    },
};
use matrix_sdk_base::event_cache::store::EventCacheStore;
use persephone_contracts::{LocalSearchFilters, LocalSearchHit, LocalSearchResults};
use persephone_core_protocol::ErrorCode;

pub const MAX_SEARCH_ROOMS: usize = 100;
pub const MAX_SCANNED_EVENTS: u32 = 2000;
pub const MAX_SEARCH_RESULTS: usize = 50;
pub const MAX_SNIPPET_BYTES: usize = 512;
pub const MAX_SEARCH_BYTES: usize = 112 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHUNKS: usize = 2000;

/// Validate the byte and control limits before case folding the query.
fn checked_term(term: &str) -> Result<String, ErrorCode> {
    if term.trim().is_empty() || term.len() > 128 || term.chars().any(char::is_control) {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(term.to_lowercase())
}

/// Reject malformed filters before any local cache work.
pub(crate) fn checked_filters(filters: &LocalSearchFilters) -> Result<(), ErrorCode> {
    const MAX_TIMESTAMP: u64 = (1u64 << 53) - 1;
    if filters.sender.as_ref().is_some_and(|sender| {
        sender.len() > 255
            || sender.chars().any(char::is_control)
            || matrix_sdk::ruma::UserId::parse(sender.as_str()).is_err()
    }) || filters
        .from_ms
        .is_some_and(|value| value.get() > MAX_TIMESTAMP)
        || filters
            .before_ms
            .is_some_and(|value| value.get() > MAX_TIMESTAMP)
        || matches!((filters.from_ms, filters.before_ms), (Some(from), Some(before)) if from.get() >= before.get())
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}

/// Dates refer to the original message, with an inclusive start and exclusive end.
/// An unknown timestamp cannot satisfy a date filter.
pub(crate) fn matches_filters(
    filters: &LocalSearchFilters,
    sender: &str,
    timestamp: Option<u64>,
) -> bool {
    if filters
        .sender
        .as_deref()
        .is_some_and(|expected| expected != sender)
    {
        return false;
    }
    if filters.from_ms.is_none() && filters.before_ms.is_none() {
        return true;
    }
    timestamp.is_some_and(|timestamp| {
        filters.from_ms.is_none_or(|from| timestamp >= from.get())
            && filters
                .before_ms
                .is_none_or(|before| timestamp < before.get())
    })
}

/// Search plain text message kinds only; omit media and custom message types.
fn body(msgtype: &MessageType) -> Option<&str> {
    match msgtype {
        MessageType::Text(text) => Some(&text.body),
        MessageType::Notice(text) => Some(&text.body),
        MessageType::Emote(text) => Some(&text.body),
        _ => None,
    }
}

/// Project the complete bounded source before finding a match. The snippet
/// starts near that match, on UTF-8 boundaries, so it can explain the hit.
fn matching_snippet(text: &str, term: &str) -> Option<String> {
    // Input was already bounded by MAX_EVENT_BYTES. Match all retained text,
    // rather than silently searching only a timeline body's shorter prefix.
    let projected: String = text
        .chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                '\u{FFFD}'
            } else {
                ch
            }
        })
        .collect();
    let folded = projected.to_lowercase();
    let at = folded.find(term)?;
    // Lowercase can change byte lengths. Map the folded offset back to the
    // original text instead of using it directly as a UTF-8 byte offset.
    let mut folded_bytes = 0;
    let mut start = 0;
    for (offset, ch) in projected.char_indices() {
        let width = ch.to_lowercase().map(char::len_utf8).sum::<usize>();
        if folded_bytes + width > at {
            start = offset;
            break;
        }
        folded_bytes += width;
    }
    let remaining = &projected[start..];
    let mut end = remaining.len().min(MAX_SNIPPET_BYTES);
    while !remaining.is_char_boundary(end) {
        end -= 1;
    }
    Some(remaining[..end].to_owned())
}

/// Search already retained unthreaded room chunks, newest first per room.
/// The cache is subscribed by client setup before sync begins.
pub async fn search(
    client: &Client,
    stores: &Stores,
    term: &str,
    room_id: Option<&str>,
    filters: &LocalSearchFilters,
) -> Result<LocalSearchResults, ErrorCode> {
    search_cache(client, &stores.sdk().event_cache, term, room_id, filters).await
}

/// Apply room, event, chunk, and byte budgets to local cache reads.
/// Fail on unreadable ignore state and recheck it before publishing results.
async fn search_cache(
    client: &Client,
    cache: &impl EventCacheStore,
    term: &str,
    room_id: Option<&str>,
    filters: &LocalSearchFilters,
) -> Result<LocalSearchResults, ErrorCode> {
    let term = checked_term(term)?;
    checked_filters(filters)?;
    let ignored = abuse::ignored_set(client).await?;
    let mut rooms = if let Some(id) = room_id {
        vec![matrix_views::joined_room(client, id)?]
    } else {
        client.joined_rooms()
    };
    rooms.sort_by(|left, right| left.room_id().cmp(right.room_id()));
    let mut output = LocalSearchResults {
        results: Vec::new(),
        truncated: rooms.len() > MAX_SEARCH_ROOMS,
        scanned: 0,
    };
    let mut scan_bytes = 0usize;
    let mut chunk_count = 0usize;
    let mut exhausted = false;
    for room in rooms.iter().take(MAX_SEARCH_ROOMS) {
        if room.state() != RoomState::Joined {
            continue;
        }
        if output.scanned == MAX_SCANNED_EVENTS || exhausted {
            output.truncated = true;
            break;
        }
        let id = LinkedChunkId::Room(room.room_id());
        let (mut chunk, _) = cache
            .load_last_chunk(id)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        let mut seen = BTreeSet::new();
        let mut events = Vec::new();
        let mut room_unsearchable = false;
        while let Some(current) = chunk {
            chunk_count += 1;
            if chunk_count > MAX_CHUNKS || !seen.insert(current.identifier.index()) {
                output.truncated = true;
                exhausted = true;
                break;
            }
            let previous = current.previous;
            match current.content {
                ChunkContent::Gap(_) => output.truncated = true,
                ChunkContent::Items(items) => {
                    for event in items.into_iter().rev() {
                        if output.scanned == MAX_SCANNED_EVENTS {
                            output.truncated = true;
                            break;
                        }
                        output.scanned += 1;
                        let bytes = event.raw().json().get().len();
                        if bytes > MAX_EVENT_BYTES {
                            output.truncated = true;
                            // This may be an edit or deletion. Omitting it
                            // while returning earlier text could disclose
                            // superseded content. Omit this room's hits.
                            room_unsearchable = true;
                            break;
                        }
                        if scan_bytes.saturating_add(bytes) > MAX_SCAN_BYTES {
                            output.truncated = true;
                            exhausted = true;
                            break;
                        }
                        scan_bytes += bytes;
                        if matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. })
                            || event
                                .raw()
                                .get_field::<String>("type")
                                .ok()
                                .flatten()
                                .as_deref()
                                == Some("m.room.encrypted")
                        {
                            output.truncated = true;
                            continue;
                        }
                        events.push(event);
                    }
                }
            }
            if output.scanned == MAX_SCANNED_EVENTS
                || exhausted
                || room_unsearchable
                || scan_bytes >= MAX_SCAN_BYTES
            {
                output.truncated |= previous.is_some();
                break;
            }
            chunk = if previous.is_some() {
                let loaded = cache
                    .load_previous_chunk(id, current.identifier)
                    .await
                    .map_err(|_| ErrorCode::Unavailable)?;
                output.truncated |= loaded.is_none();
                loaded
            } else {
                None
            };
        }
        if !room_unsearchable {
            search_events(
                room.room_id().as_str(),
                &events,
                &ignored,
                &term,
                filters,
                &mut output,
            );
        }
    }
    // An ignore write or server account-data update can complete during
    // the scan. Apply the complete current set again before publication.
    let ignored = abuse::ignored_set(client).await?;
    output
        .results
        .retain(|hit| !ignored.contains(hit.sender.as_str()));
    Ok(output)
}

/// `events` are newest first. Consider all retained redactions before edits,
/// including deletion of an edit. Never accept another sender's replacement.
fn search_events(
    room_id: &str,
    events: &[TimelineEvent],
    ignored: &BTreeSet<OwnedUserId>,
    term: &str,
    filters: &LocalSearchFilters,
    output: &mut LocalSearchResults,
) {
    let redacted: BTreeSet<OwnedEventId> = events
        .iter()
        .filter_map(|event| {
            let value: serde_json::Value = event.raw().deserialize_as().ok()?;
            if value.get("type")?.as_str()? != "m.room.redaction" {
                return None;
            }
            let target = value
                .get("content")
                .and_then(|c| c.get("redacts"))
                .or_else(|| value.get("redacts"))?
                .as_str()?;
            matrix_sdk::ruma::EventId::parse(target).ok()
        })
        .collect();
    let mut edits: BTreeMap<(OwnedEventId, OwnedUserId), (u64, OwnedEventId, String)> =
        BTreeMap::new();
    for event in events {
        let Some(event_id) = event.event_id() else {
            continue;
        };
        if redacted.contains(event_id) {
            continue;
        }
        let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(message),
        ))) = event.raw().deserialize()
        else {
            continue;
        };
        if ignored.contains(&message.sender) {
            continue;
        }
        if let Some(Relation::Replacement(replacement)) = message.content.relates_to
            && let MessageType::Text(text) = &replacement.new_content.msgtype
        {
            let target = (replacement.event_id, message.sender);
            let timestamp = u64::from(message.origin_server_ts.get());
            let newer = edits.get(&target).is_none_or(|current| {
                timestamp > current.0 || (timestamp == current.0 && message.event_id > current.1)
            });
            if newer {
                edits.insert(target, (timestamp, message.event_id, text.body.clone()));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for event in events {
        let Some(event_id) = event.event_id() else {
            continue;
        };
        if !seen.insert(event_id.to_owned()) {
            continue;
        }
        if redacted.contains(event_id) {
            continue;
        }
        let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(message),
        ))) = event.raw().deserialize()
        else {
            continue;
        };
        if ignored.contains(&message.sender)
            || matches!(message.content.relates_to, Some(Relation::Replacement(_)))
        {
            continue;
        }
        if !matches_filters(
            filters,
            message.sender.as_str(),
            Some(u64::from(message.origin_server_ts.get())),
        ) {
            continue;
        }
        let Some(original) = body(&message.content.msgtype) else {
            continue;
        };
        let text = edits
            .get(&(event_id.to_owned(), message.sender.clone()))
            .filter(|_| matches!(message.content.msgtype, MessageType::Text(_)))
            .map(|(_, _, body)| body.as_str())
            .unwrap_or(original);
        let text = if matches!(message.content.relates_to, Some(Relation::Reply(_))) {
            remove_plain_reply_fallback(text)
        } else {
            text
        };
        let Some(body) = matching_snippet(text, term) else {
            continue;
        };
        if output.results.len() == MAX_SEARCH_RESULTS {
            output.truncated = true;
            continue;
        }
        let hit = LocalSearchHit {
            room_id: room_id.to_owned(),
            event_id: event_id.to_string(),
            sender: message.sender.to_string(),
            body,
        };
        output.results.push(hit);
        if serde_json::to_vec(output).map_or(true, |bytes| bytes.len() > MAX_SEARCH_BYTES) {
            output.results.pop();
            output.truncated = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::serde::Raw;
    use persephone_contracts::UnixMillis;
    use serde_json::json;

    fn event(id: &str, sender: &str, content: serde_json::Value) -> TimelineEvent {
        event_at(id, sender, content, 1)
    }

    fn event_at(
        id: &str,
        sender: &str,
        content: serde_json::Value,
        timestamp: u64,
    ) -> TimelineEvent {
        TimelineEvent::from_plaintext(
            Raw::from_json_string(
                json!({
                    "type": "m.room.message", "event_id": id, "sender": sender,
                    "origin_server_ts": timestamp, "content": content,
                })
                .to_string(),
            )
            .unwrap(),
        )
    }

    fn results(events: &[TimelineEvent], term: &str, ignored: &[&str]) -> LocalSearchResults {
        let mut output = LocalSearchResults {
            results: Vec::new(),
            truncated: false,
            scanned: events.len() as u32,
        };
        search_events(
            "!room:example.org",
            events,
            &ignored
                .iter()
                .map(|id| matrix_sdk::ruma::UserId::parse(*id).unwrap())
                .collect(),
            &checked_term(term).unwrap(),
            &LocalSearchFilters::default(),
            &mut output,
        );
        output
    }

    #[test]
    fn given_sender_and_date_filters_then_boundaries_and_unknown_dates_are_explicit() {
        let filters = LocalSearchFilters {
            sender: Some("@alice:s".into()),
            from_ms: UnixMillis::new(10),
            before_ms: UnixMillis::new(20),
        };
        assert!(checked_filters(&filters).is_ok());
        assert!(UnixMillis::new(1 << 53).is_none());
        assert!(matches_filters(&filters, "@alice:s", Some(10)));
        assert!(matches_filters(&filters, "@alice:s", Some(19)));
        for timestamp in [None, Some(9), Some(20)] {
            assert!(!matches_filters(&filters, "@alice:s", timestamp));
        }
        assert!(!matches_filters(&filters, "@Alice:s", Some(10)));
        assert!(matches_filters(
            &LocalSearchFilters::default(),
            "@alice:s",
            None
        ));
        for invalid in [
            LocalSearchFilters {
                sender: Some("alice".into()),
                ..Default::default()
            },
            LocalSearchFilters {
                sender: Some("@alice:s\n".into()),
                ..Default::default()
            },
            LocalSearchFilters {
                sender: Some(format!("@{}:s", "a".repeat(256))),
                ..Default::default()
            },
            LocalSearchFilters {
                from_ms: UnixMillis::new(20),
                before_ms: UnixMillis::new(20),
                ..Default::default()
            },
            LocalSearchFilters {
                from_ms: UnixMillis::new(21),
                before_ms: UnixMillis::new(20),
                ..Default::default()
            },
        ] {
            assert_eq!(checked_filters(&invalid), Err(ErrorCode::InvalidArgument));
        }
    }

    #[test]
    fn given_relations_outside_date_filter_then_current_original_text_is_searched() {
        let filters = LocalSearchFilters {
            sender: Some("@alice:s".into()),
            from_ms: UnixMillis::new(10),
            before_ms: UnixMillis::new(20),
        };
        let events = vec![
            event_at(
                "$edit",
                "@alice:s",
                json!({
                    "msgtype": "m.text", "body": "* changed",
                    "m.new_content": {"msgtype": "m.text", "body": "updated words"},
                    "m.relates_to": {"rel_type": "m.replace", "event_id": "$original"},
                }),
                30,
            ),
            TimelineEvent::from_plaintext(
                Raw::from_json_string(
                    json!({
                        "type":"m.room.redaction", "event_id":"$redaction", "sender":"@mod:s",
                        "origin_server_ts":30, "content":{"redacts":"$deleted"},
                    })
                    .to_string(),
                )
                .unwrap(),
            ),
            event_at(
                "$original",
                "@alice:s",
                json!({"msgtype":"m.text", "body":"old words"}),
                10,
            ),
            event_at(
                "$deleted",
                "@alice:s",
                json!({"msgtype":"m.text", "body":"deleted words"}),
                11,
            ),
            event_at(
                "$before",
                "@alice:s",
                json!({"msgtype":"m.text", "body":"outside words"}),
                9,
            ),
            event_at(
                "$end",
                "@alice:s",
                json!({"msgtype":"m.text", "body":"outside words"}),
                20,
            ),
            event_at(
                "$other",
                "@bob:s",
                json!({"msgtype":"m.text", "body":"other words"}),
                10,
            ),
        ];
        let mut output = LocalSearchResults {
            results: Vec::new(),
            truncated: false,
            scanned: events.len() as u32,
        };
        search_events(
            "!room:s",
            &events,
            &BTreeSet::new(),
            "words",
            &filters,
            &mut output,
        );
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0].event_id, "$original");
        assert_eq!(output.results[0].body, "words");
        output.results.clear();
        search_events(
            "!room:s",
            &events,
            &BTreeSet::new(),
            "old",
            &filters,
            &mut output,
        );
        assert!(output.results.is_empty());
    }

    #[test]
    fn given_edits_then_only_same_sender_latest_text_matches() {
        let edit = |id, sender, text, timestamp| {
            event_at(
                id,
                sender,
                json!({
                    "msgtype": "m.text", "body": "* fallback",
                    "m.new_content": { "msgtype": "m.text", "body": text },
                    "m.relates_to": { "rel_type": "m.replace", "event_id": "$original" },
                }),
                timestamp,
            )
        };
        let events = vec![
            edit("$forged", "@mallory:s", "forged", 99),
            // Arrival order differs from timestamp order. Equal timestamps
            // use the lexicographically largest event ID.
            edit("$older", "@alice:s", "older words", 2),
            edit("$a", "@alice:s", "tie loser", 3),
            edit("$z", "@alice:s", "Latest words", 3),
            event(
                "$original",
                "@alice:s",
                json!({ "msgtype": "m.text", "body": "original words" }),
            ),
        ];
        assert_eq!(results(&events, "LATEST", &[]).results.len(), 1);
        for term in ["forged", "older", "original", "fallback", "tie loser"] {
            assert!(results(&events, term, &[]).results.is_empty(), "{term}");
        }
    }

    #[test]
    fn given_deletions_ignored_senders_and_reply_quotes_then_they_do_not_match() {
        let redaction = TimelineEvent::from_plaintext(
            Raw::from_json_string(
                json!({
                    "type": "m.room.redaction", "event_id": "$redaction", "sender": "@mod:s",
                    "origin_server_ts": 2, "content": { "redacts": "$deleted" },
                })
                .to_string(),
            )
            .unwrap(),
        );
        let events = vec![
            redaction,
            event(
                "$deleted",
                "@alice:s",
                json!({ "msgtype": "m.text", "body": "secret" }),
            ),
            event(
                "$ignored",
                "@bob:s",
                json!({ "msgtype": "m.text", "body": "secret" }),
            ),
            event(
                "$reply",
                "@alice:s",
                json!({ "msgtype": "m.text", "body": "> <@bob:s> secret\n\nvisible reply", "m.relates_to": { "m.in_reply_to": { "event_id": "$ignored" } } }),
            ),
        ];
        assert!(results(&events, "secret", &["@bob:s"]).results.is_empty());
        assert_eq!(results(&events, "VISIBLE", &["@bob:s"]).results.len(), 1);
    }

    #[test]
    fn given_many_hits_and_unicode_then_output_is_bounded_on_character_boundaries() {
        let events: Vec<_> = (0..100)
            .map(|n| {
                event(
                    &format!("$e{n}"),
                    "@alice:s",
                    json!({ "msgtype": "m.text", "body": format!("İ{}", "世".repeat(1000)) }),
                )
            })
            .collect();
        let output = results(&events, "世", &[]);
        assert_eq!(output.results.len(), MAX_SEARCH_RESULTS);
        assert!(output.truncated);
        assert!(
            output
                .results
                .iter()
                .all(|hit| hit.body.len() <= MAX_SNIPPET_BYTES && hit.body.starts_with('世'))
        );
        assert!(serde_json::to_vec(&output).unwrap().len() <= MAX_SEARCH_BYTES);
        for term in ["", " ", "term\n", &"x".repeat(129)] {
            assert!(checked_term(term).is_err());
        }
    }

    #[tokio::test]
    async fn given_encrypted_sqlite_cache_then_search_traverses_bounded_history_without_http() {
        use matrix_sdk::linked_chunk::{ChunkIdentifier, Position, Update};
        use matrix_sdk_sqlite::SqliteEventCacheStore;
        let server = wiremock::MockServer::start().await;
        let client =
            crate::abuse::test_client(&server.uri(), &[("!room:example.org", RoomState::Joined)])
                .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = SqliteEventCacheStore::open_with_key(dir.path(), Some(&[7u8; 32]))
            .await
            .unwrap();
        let room = matrix_sdk::ruma::room_id!("!room:example.org");
        for chunk in 0..17u64 {
            let id = ChunkIdentifier::new(chunk);
            let items = (0..128)
                .map(|n| {
                    let index = chunk * 128 + n;
                    // These newest events are inside the scan budget and match
                    // the term, but each fails one active filter.
                    event_at(
                        &format!("$e{index}"),
                        if index == 2175 { "@bob:s" } else { "@alice:s" },
                        json!({"msgtype":"m.text", "body":"locally retained message"}),
                        match index {
                            2174 => 2,
                            2173 => 0,
                            _ => 1,
                        },
                    )
                })
                .collect();
            cache
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room),
                    vec![
                        Update::NewItemsChunk {
                            previous: chunk.checked_sub(1).map(ChunkIdentifier::new),
                            new: id,
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(id, 0),
                            items,
                        },
                    ],
                )
                .await
                .unwrap();
        }
        // Positive control: prove the recorder observes an SDK request.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                ".*/account_data/m.ignored_user_list",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({"ignored_users":{}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let before_control = server.received_requests().await.unwrap().len();
        client.account().fetch_account_data_static::<matrix_sdk::ruma::events::ignored_user_list::IgnoredUserListEventContent>().await.unwrap();
        let before = server.received_requests().await.unwrap().len();
        assert_eq!(
            before,
            before_control + 1,
            "the HTTP recorder must detect the control request"
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request
                    .url
                    .path()
                    .ends_with("/account_data/m.ignored_user_list"))
        );
        let output = search_cache(
            &client,
            &cache,
            "RETAINED",
            Some(room.as_str()),
            &LocalSearchFilters {
                sender: Some("@alice:s".into()),
                from_ms: UnixMillis::new(1),
                before_ms: UnixMillis::new(2),
            },
        )
        .await
        .unwrap();
        assert_eq!(output.scanned, MAX_SCANNED_EVENTS);
        assert_eq!(output.results.len(), MAX_SEARCH_RESULTS);
        assert_eq!(output.results[0].event_id, "$e2172");
        assert!(
            output.results.iter().all(|hit| {
                hit.sender == "@alice:s"
                    && !["$e2175", "$e2174", "$e2173"].contains(&hit.event_id.as_str())
            }),
            "sender, inclusive start, and exclusive end must filter cached hits"
        );
        assert!(output.truncated);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            before,
            "search must make no HTTP request"
        );
        assert_eq!(
            search_cache(
                &client,
                &cache,
                "retained",
                Some("!missing:s"),
                &LocalSearchFilters::default()
            )
            .await
            .err(),
            Some(ErrorCode::NotFound)
        );
        // A valid edit larger than the source budget must not make the
        // original text searchable. Omit all hits from that room.
        let id = ChunkIdentifier::new(17);
        cache.handle_linked_chunk_updates(LinkedChunkId::Room(room), vec![
            Update::NewItemsChunk { previous: Some(ChunkIdentifier::new(16)), new: id, next: None },
            Update::PushItems { at: Position::new(id, 0), items: vec![event("$large-edit", "@alice:s", json!({
                "msgtype":"m.text", "body":"* changed",
                "m.new_content":{"msgtype":"m.text","body":"x".repeat(MAX_EVENT_BYTES)},
                "m.relates_to":{"rel_type":"m.replace","event_id":"$e2172"},
            }))] },
        ]).await.unwrap();
        let output = search_cache(
            &client,
            &cache,
            "retained",
            Some(room.as_str()),
            &LocalSearchFilters::default(),
        )
        .await
        .unwrap();
        assert!(output.results.is_empty());
        assert!(output.truncated);
        cache.close().await.unwrap();
    }
}
