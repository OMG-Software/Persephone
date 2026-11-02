//! Bounded room navigation and metadata views. Matrix authority stays in core.
use crate::matrix_views::{MatrixViews, joined_room, server_time};
use crate::views::MAX_PAGE_BYTES;
use matrix_sdk::{
    Client, Room,
    room::MessagesOptions,
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch, UInt,
        api::client::room::get_event_by_timestamp,
        events::{StateEventType, room::pinned_events::RoomPinnedEventsEventContent},
    },
};
use persephone_contracts::{
    AttachmentKind, AttachmentPage, FocusedTimeline, LocalSearchFilters, PinnedMessages,
    TimelineItem, TimelinePage,
};
use persephone_core_protocol::{ErrorCode, MAX_EVENT_ID_BYTES};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

const MAX_PINS: usize = 100;
/// Removal can preserve a larger existing state without permitting new pins.
const MAX_PIN_STATE_IDS: usize = 512;
const MAX_SCAN: usize = 100;
const MAX_ATTACHMENTS: usize = 50;
const PIN_READ_BUDGET: Duration = Duration::from_secs(10);

fn event_id(raw: &str) -> Result<&EventId, ErrorCode> {
    if raw.len() > MAX_EVENT_ID_BYTES {
        return Err(ErrorCode::InvalidArgument);
    }
    <&EventId>::try_from(raw).map_err(|_| ErrorCode::InvalidArgument)
}

fn fits(value: &impl serde::Serialize) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= MAX_PAGE_BYTES)
}

/// Get the first visible event at or after the supplied server time.
/// Only a missing forward result permits a backward lookup.
pub async fn jump_to_date(
    client: &Client,
    views: &MatrixViews,
    room: &str,
    timestamp: u64,
) -> Result<FocusedTimeline, ErrorCode> {
    let joined = joined_room(client, room)?;
    let ts = MilliSecondsSinceUnixEpoch(
        UInt::try_from(timestamp).map_err(|_| ErrorCode::InvalidArgument)?,
    );
    let request = get_event_by_timestamp::v1::Request::since(joined.room_id().to_owned(), ts);
    let (found, backward) = match client.send(request).await {
        Ok(found) => (found, false),
        Err(error)
            if error
                .as_client_api_error()
                .is_some_and(|e| e.status_code.as_u16() == 404) =>
        {
            (
                client
                    .send(get_event_by_timestamp::v1::Request::until(
                        joined.room_id().to_owned(),
                        ts,
                    ))
                    .await
                    .map_err(|_| ErrorCode::Unavailable)?,
                true,
            )
        }
        Err(_) => return Err(ErrorCode::Unavailable),
    };
    focused_context(
        client,
        views,
        room,
        found.event_id.as_str(),
        Some((timestamp, backward)),
    )
    .await
}

/// Return a focused page. Retry smaller complete contexts instead of cutting
/// the older range, so the returned backward cursor cannot skip history.
pub async fn event_context(
    client: &Client,
    views: &MatrixViews,
    room: &str,
    raw: &str,
) -> Result<FocusedTimeline, ErrorCode> {
    focused_context(client, views, room, raw, None).await
}

/// Select a visible date target from the same bounded, validated context.
async fn focused_context(
    client: &Client,
    views: &MatrixViews,
    room: &str,
    raw: &str,
    date: Option<(u64, bool)>,
) -> Result<FocusedTimeline, ErrorCode> {
    let joined = joined_room(client, room)?;
    let id = event_id(raw)?;
    let mut limit = 98u64;
    loop {
        let context = joined
            .event_with_context(id, true, UInt::try_from(limit).unwrap(), None)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        let target = context.event.ok_or(ErrorCode::NotFound)?;
        if target.event_id() != Some(id) {
            return Err(ErrorCode::Rejected);
        }
        if context.events_before.len() + context.events_after.len() + 1 > 99 {
            return Err(ErrorCode::LimitExceeded);
        }
        let mut chunk = context.events_before;
        chunk.reverse();
        chunk.push(target);
        chunk.extend(context.events_after);
        let mut items = views.project_chunk(client, &joined, &chunk).await?;
        let selected = match date {
            Some((timestamp, backward)) => date_target(&items, timestamp, backward)
                .ok_or(ErrorCode::NotFound)?
                .to_owned(),
            None if items.iter().any(|item| item_id(item) == Some(raw)) => raw.to_owned(),
            None => return Err(ErrorCode::NotFound),
        };
        items.push(TimelineItem::Gap { next_cursor: None });
        // Register only after the budget check to avoid evicting useful cursors
        // for responses that will be retried.
        let mut result = FocusedTimeline {
            page: TimelinePage {
                room_id: room.to_owned(),
                items,
                next_cursor: None,
            },
            event_id: selected,
        };
        if fits(&result) {
            result.page.next_cursor = context
                .prev_batch_token
                .map(|token| views.remember(room, token));
            if !fits(&result) {
                return Err(ErrorCode::LimitExceeded);
            }
            return Ok(result);
        }
        if limit == 0 {
            return Err(ErrorCode::LimitExceeded);
        }
        limit /= 2;
    }
}

/// Skip hidden messages and relation-only events that have no timeline row.
fn date_target(items: &[TimelineItem], timestamp: u64, backward: bool) -> Option<&str> {
    let redacted: std::collections::BTreeSet<&str> = items
        .iter()
        .filter_map(|item| {
            if let TimelineItem::Redaction { redacts, .. } = item {
                Some(redacts.as_str())
            } else {
                None
            }
        })
        .collect();
    let candidates = items.iter().filter_map(|item| {
        let (id, time) = match item {
            TimelineItem::Message {
                event_id,
                origin_server_ts,
                ..
            }
            | TimelineItem::Image {
                event_id,
                origin_server_ts,
                ..
            }
            | TimelineItem::File {
                event_id,
                origin_server_ts,
                ..
            }
            | TimelineItem::Notice {
                event_id,
                origin_server_ts,
                ..
            }
            | TimelineItem::State {
                event_id,
                origin_server_ts,
                ..
            } => (event_id, origin_server_ts.as_ref()?.get()),
            _ => return None,
        };
        (!redacted.contains(id.as_str())
            && if backward {
                time <= timestamp
            } else {
                time >= timestamp
            })
        .then_some((id.as_str(), time))
    });
    if backward {
        candidates.max_by_key(|(_, time)| *time)
    } else {
        candidates.min_by_key(|(_, time)| *time)
    }
    .map(|(id, _)| id)
}

fn item_id(item: &TimelineItem) -> Option<&str> {
    match item {
        TimelineItem::Message { event_id, .. }
        | TimelineItem::Image { event_id, .. }
        | TimelineItem::File { event_id, .. }
        | TimelineItem::Notice { event_id, .. }
        | TimelineItem::Edit { event_id, .. }
        | TimelineItem::Reaction { event_id, .. }
        | TimelineItem::Redaction { event_id, .. }
        | TimelineItem::State { event_id, .. } => Some(event_id),
        TimelineItem::Gap { .. } => None,
    }
}

async fn can_manage(client: &Client, room: &Room) -> Result<bool, ErrorCode> {
    let own = client.user_id().ok_or(ErrorCode::Locked)?;
    Ok(crate::room_settings::power_levels(client, room)
        .await?
        .user_can_send_state(own, StateEventType::RoomPinnedEvents))
}

/// Load fresh pin state and bounded projections. Missing content retains its
/// event ID, so an unavailable or ignored pin does not become invented text.
pub async fn pinned_messages(
    client: &Client,
    views: &MatrixViews,
    room: &str,
    is_open: impl Fn() -> bool,
) -> Result<PinnedMessages, ErrorCode> {
    pinned_messages_until(
        client,
        views,
        room,
        is_open,
        Instant::now() + PIN_READ_BUDGET,
    )
    .await
}

/// Keep the whole pin read, including a slow current request, within one deadline.
async fn pinned_messages_until(
    client: &Client,
    views: &MatrixViews,
    room: &str,
    is_open: impl Fn() -> bool,
    deadline: Instant,
) -> Result<PinnedMessages, ErrorCode> {
    let joined = joined_room(client, room)?;
    if !is_open() {
        return Err(ErrorCode::Locked);
    }
    let ids = timeout_at(deadline, joined.load_pinned_events())
        .await
        .map_err(|_| ErrorCode::Unavailable)?
        .map_err(|_| ErrorCode::Unavailable)?
        .unwrap_or_default();
    let mut result = PinnedMessages {
        room_id: room.to_owned(),
        event_ids: Vec::new(),
        items: Vec::new(),
        can_manage: timeout_at(deadline, can_manage(client, &joined))
            .await
            .map_err(|_| ErrorCode::Unavailable)??,
        truncated: ids.len() > MAX_PINS,
    };
    for id in ids.into_iter().take(MAX_PINS) {
        if id.as_str().len() > MAX_EVENT_ID_BYTES {
            result.truncated = true;
            continue;
        }
        // Pin state is untrusted. Preserve the first reference only, so
        // duplicate IDs cannot trigger duplicate fetches or keyed UI rows.
        if result.event_ids.iter().any(|known| known == id.as_str()) {
            continue;
        }
        result.event_ids.push(id.to_string());
    }
    for id in result.event_ids.clone() {
        if !is_open() || Instant::now() >= deadline {
            result.truncated = true;
            break;
        }
        let id = event_id(&id)?;
        if let Ok(Ok(event)) = timeout_at(deadline, joined.event(id, None)).await {
            if event.event_id() != Some(id) {
                return Err(ErrorCode::Rejected);
            }
            // A preview must not show obsolete original text when later edits
            // cannot be checked. Hide it on incomplete/failed relation reads.
            if !is_open() || Instant::now() >= deadline {
                result.truncated = true;
                break;
            }
            let edits = timeout_at(
                deadline,
                crate::room_relations::replacements(client, &joined, id),
            )
            .await;
            let Ok(Ok(edits)) = edits else {
                result.truncated = true;
                continue;
            };
            let projected =
                timeout_at(deadline, views.project_pin(client, &joined, event, edits)).await;
            let projected = match projected {
                Ok(items) => items?,
                Err(_) => {
                    result.truncated = true;
                    break;
                }
            };
            if projected.is_empty() {
                result.truncated = true;
            }
            for item in projected {
                result.items.push(item);
                if !fits(&result) {
                    result.items.pop();
                    result.truncated = true;
                }
            }
        } else {
            result.truncated = true;
        }
    }
    if !fits(&result) {
        return Err(ErrorCode::LimitExceeded);
    }
    Ok(result)
}

/// Change fresh pin state only with state permission and a bounded list.
/// Matrix state replacement has no compare-and-swap guarantee.
pub async fn set_pinned(
    client: &Client,
    room: &str,
    raw: &str,
    pinned: bool,
) -> Result<(), ErrorCode> {
    let joined = joined_room(client, room)?;
    let id = event_id(raw)?;
    if !can_manage(client, &joined).await? {
        return Err(ErrorCode::Rejected);
    }
    let mut ids = joined
        .load_pinned_events()
        .await
        .map_err(|_| ErrorCode::Unavailable)?
        .unwrap_or_default();
    if ids.len() > MAX_PIN_STATE_IDS || ids.iter().any(|id| id.as_str().len() > MAX_EVENT_ID_BYTES)
    {
        return Err(ErrorCode::LimitExceeded);
    }
    if pinned && ids.iter().any(|old| old == id) {
        return Ok(());
    }
    if !pinned && !ids.iter().any(|old| old == id) {
        return Ok(());
    }
    if pinned {
        if ids.len() >= MAX_PINS {
            return Err(ErrorCode::LimitExceeded);
        }
        let event = joined
            .event(id, None)
            .await
            .map_err(|_| ErrorCode::NotFound)?;
        if event.event_id() != Some(id) {
            return Err(ErrorCode::Rejected);
        }
        let items = MatrixViews::default()
            .project_chunk(client, &joined, &[event])
            .await?;
        if !items.iter().any(|item| {
            matches!(
                item,
                TimelineItem::Message { .. }
                    | TimelineItem::Image { .. }
                    | TimelineItem::File { .. }
            )
        }) {
            return Err(ErrorCode::Rejected);
        }
        ids.push(id.to_owned());
    } else if !pinned {
        ids.retain(|old| old != id);
    }
    let content = RoomPinnedEventsEventContent::new(ids);
    if !fits(&content) {
        return Err(ErrorCode::LimitExceeded);
    }
    joined
        .send_state_event(content)
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    Ok(())
}

/// Scan one complete history chunk for attachment metadata. Empty pages can
/// have a continuation because encrypted events require local filtering.
pub async fn attachments(
    client: &Client,
    views: &MatrixViews,
    room: &str,
    cursor: Option<&str>,
    kind: AttachmentKind,
    filters: &LocalSearchFilters,
) -> Result<AttachmentPage, ErrorCode> {
    crate::local_search::checked_filters(filters)?;
    let joined = joined_room(client, room)?;
    let from = cursor.map(|cursor| views.token(room, cursor)).transpose()?;
    // At most 50 raw events ensures no matching attachments are discarded.
    let mut options = MessagesOptions::backward().from(from.as_deref());
    options.limit = UInt::try_from(MAX_ATTACHMENTS as u64).unwrap();
    let messages = joined
        .messages(options)
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    if messages.chunk.len() > MAX_SCAN || messages.chunk.len() > MAX_ATTACHMENTS {
        return Err(ErrorCode::LimitExceeded);
    }
    let scanned = messages.chunk.len() as u32;
    let matching: std::collections::BTreeSet<String> = messages
        .chunk
        .iter()
        .filter(|event| {
            event.sender().is_some_and(|sender| {
                crate::local_search::matches_filters(
                    filters,
                    sender.as_str(),
                    server_time(event).map(|time| time.get()),
                )
            })
        })
        .filter_map(|event| event.event_id().map(|id| id.to_string()))
        .collect();
    let projected = views
        .project_attachment_chunk(client, &joined, &messages.chunk)
        .await?;
    let redacted: std::collections::BTreeSet<&str> = projected
        .iter()
        .filter_map(|item| match item {
            TimelineItem::Redaction { redacts, .. } => Some(redacts.as_str()),
            _ => None,
        })
        .collect();
    let items = projected
        .iter()
        .filter(|item| match item {
            TimelineItem::Image { event_id, .. }
                if matches!(kind, AttachmentKind::All | AttachmentKind::Images) =>
            {
                !redacted.contains(event_id.as_str()) && matching.contains(event_id)
            }
            TimelineItem::File { event_id, .. }
                if matches!(kind, AttachmentKind::All | AttachmentKind::Files) =>
            {
                !redacted.contains(event_id.as_str()) && matching.contains(event_id)
            }
            _ => false,
        })
        .cloned()
        .collect();
    let result = AttachmentPage {
        room_id: room.to_owned(),
        items,
        next_cursor: messages
            .end
            .filter(|_| scanned > 0)
            .map(|token| views.remember(room, token)),
        scanned,
        partial: true,
    };
    if !fits(&result) {
        return Err(ErrorCode::LimitExceeded);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_invalid_or_oversized_event_id_then_reject_before_network() {
        assert_eq!(
            event_id("not an event").unwrap_err(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            event_id(&format!("${}", "a".repeat(MAX_EVENT_ID_BYTES))).unwrap_err(),
            ErrorCode::InvalidArgument
        );
        assert!(event_id("$valid:example.org").is_ok());
    }

    #[test]
    fn given_cross_room_or_expired_cursor_then_no_server_token_is_disclosed() {
        let views = MatrixViews::default();
        let cursor = views.remember("!a:example.org", "secret-server-token".to_owned());
        assert_eq!(
            views.token("!b:example.org", &cursor),
            Err(ErrorCode::NotFound)
        );
        assert!(!cursor.contains("secret"));
        for _ in 0..crate::matrix_views::MAX_CURSORS {
            views.remember("!a:example.org", "next".to_owned());
        }
        assert_eq!(
            views.token("!a:example.org", &cursor),
            Err(ErrorCode::NotFound)
        );
    }

    #[test]
    fn given_projection_budget_then_serialized_envelope_counts_toward_limit() {
        assert!(fits(&"x".repeat(MAX_PAGE_BYTES - 2)));
        assert!(!fits(&"x".repeat(MAX_PAGE_BYTES - 1)));
    }
    use matrix_sdk::{
        SessionMeta, SessionTokens,
        authentication::matrix::MatrixSession,
        config::{RequestConfig, SyncSettings},
        ruma::user_id,
    };
    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };
    const ROOM: &str = "!dm:synthetic.test";

    fn state_event(kind: &str, key: &str, sender: &str, content: Value) -> Value {
        json!({
            "type": kind, "state_key": key, "sender": sender, "content": content,
            "event_id": format!("${kind}-{key}"), "origin_server_ts": 1
        })
    }

    /// Join events for `members`.
    fn joined(members: &[&str]) -> Vec<Value> {
        members
            .iter()
            .map(|user| state_event("m.room.member", user, user, json!({ "membership": "join" })))
            .collect()
    }

    /// A sync with the direct-message room `ROOM`: `members` joined (two, as the
    /// summary counts), marked direct for Bob, with `m.room.encryption` in its
    /// state when `encrypted`.
    fn sync_reply(encrypted: bool, members: &[&str]) -> Value {
        let mut state = vec![state_event(
            "m.room.create",
            "",
            "@alice:synthetic.test",
            json!({ "room_version": "11" }),
        )];
        state.extend(joined(members));
        state.push(state_event(
            "m.room.power_levels",
            "",
            "@alice:synthetic.test",
            json!({"users":{"@alice:synthetic.test":0},"state_default":50}),
        ));
        if encrypted {
            state.push(state_event(
                "m.room.encryption",
                "",
                "@alice:synthetic.test",
                json!({ "algorithm": "m.megolm.v1.aes-sha2" }),
            ));
        }
        json!({
            "next_batch": "s1",
            "account_data": { "events": [{
                "type": "m.direct",
                "content": { "@bob:synthetic.test": [ROOM] }
            }]},
            "rooms": { "join": { ROOM: {
                "summary": { "m.joined_member_count": 2, "m.invited_member_count": 0 },
                "state": { "events": state },
                "timeline": { "events": [], "limited": false }
            }}}
        })
    }

    /// A signed-in client after one sync with the direct-message room.
    async fn synced(server: &MockServer, encrypted: bool, members: &[&str]) -> Client {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "versions": ["v1.19"] })),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sync_reply(encrypted, members)))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/keys/upload"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "one_time_key_counts": { "signed_curve25519": 50 } })),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/keys/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "device_keys": {} })))
            .mount(server)
            .await;
        // As in Persephone's own client (`account.rs`): no retries, so a failed
        // request is answered at once.
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
        client.sync_once(SyncSettings::default()).await.unwrap();
        client
    }

    #[tokio::test]
    async fn given_unsupported_timestamp_endpoint_then_no_history_scan() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v1/rooms/{ROOM}/timestamp_to_event"
            )))
            .and(query_param("dir", "f"))
            .respond_with(
                ResponseTemplate::new(501)
                    .set_body_json(json!({"errcode":"M_UNRECOGNIZED","error":"unsupported"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert!(matches!(
            jump_to_date(&client, &MatrixViews::default(), ROOM, 1).await,
            Err(ErrorCode::Unavailable)
        ));
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path().ends_with("/messages"))
        );
    }

    #[tokio::test]
    async fn given_no_state_permission_then_pin_write_is_rejected() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        assert_eq!(
            set_pinned(&client, ROOM, "$message", true).await,
            Err(ErrorCode::Rejected)
        );
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.method == "PUT")
        );
    }

    #[tokio::test]
    async fn given_non_attachment_history_then_empty_page_keeps_backward_cursor() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/v3/rooms/{ROOM}/messages")))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "start":"latest", "end":"opaque-older", "chunk":[{
                    "type":"m.room.message", "event_id":"$text", "sender":"@alice:synthetic.test",
                    "origin_server_ts":1, "content":{"msgtype":"m.text","body":"ordinary text"}
                }], "state":[]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let views = MatrixViews::default();
        let page = attachments(
            &client,
            &views,
            ROOM,
            None,
            AttachmentKind::All,
            &LocalSearchFilters::default(),
        )
        .await
        .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.scanned, 1);
        assert!(page.partial);
        assert_eq!(
            views
                .token(ROOM, page.next_cursor.as_deref().unwrap())
                .unwrap(),
            "opaque-older"
        );
        assert!(
            !serde_json::to_string(&page)
                .unwrap()
                .contains("opaque-older")
        );
    }

    #[tokio::test]
    async fn given_context_target_identity_mismatch_then_reject() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/v3/rooms/{ROOM}/context/$wanted")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event":{"type":"m.room.message","event_id":"$other","sender":"@alice:synthetic.test","origin_server_ts":1,"content":{"msgtype":"m.text","body":"wrong target"}},
                "events_before":[],"events_after":[],"state":[],"start":"older","end":"newer"
            }))).expect(1).mount(&server).await;
        assert!(matches!(
            event_context(&client, &MatrixViews::default(), ROOM, "$wanted").await,
            Err(ErrorCode::Rejected)
        ));
    }

    #[tokio::test]
    async fn given_moderator_redaction_outside_filters_then_attachment_is_hidden() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET")).and(path(format!("/_matrix/client/v3/rooms/{ROOM}/messages")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"start":"latest","end":"older","state":[],"chunk":[
                {"type":"m.room.redaction","event_id":"$delete","sender":"@mod:synthetic.test","origin_server_ts":999,"content":{"redacts":"$image"}},
                {"type":"m.room.message","event_id":"$image","sender":"@alice:synthetic.test","origin_server_ts":10,"content":{"msgtype":"m.image","body":"image.png","url":"mxc://synthetic.test/id"}}
            ]}))).expect(1).mount(&server).await;
        let views = MatrixViews::default();
        let filters = LocalSearchFilters {
            sender: Some("@alice:synthetic.test".into()),
            from_ms: Some(persephone_contracts::UnixMillis::new(5).unwrap()),
            before_ms: Some(persephone_contracts::UnixMillis::new(20).unwrap()),
        };
        let page = attachments(
            &client,
            &views,
            ROOM,
            None,
            AttachmentKind::Images,
            &filters,
        )
        .await
        .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.scanned, 2);
        assert_eq!(
            views
                .token(ROOM, page.next_cursor.as_deref().unwrap())
                .unwrap(),
            "older"
        );
    }

    async fn context_mock(server: &MockServer) {
        Mock::given(method("GET")).and(path(format!("/_matrix/client/v3/rooms/{ROOM}/context/$wanted")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event":{"type":"m.room.message","event_id":"$wanted","sender":"@alice:synthetic.test","origin_server_ts":10,"content":{"msgtype":"m.text","body":"target"}},"events_before":[],"events_after":[],"state":[],"start":"context-older","end":"context-newer"})))
            .expect(1).mount(server).await;
    }

    #[tokio::test]
    async fn given_timestamp_forward_match_then_focused_page_retains_cursor() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v1/rooms/{ROOM}/timestamp_to_event"
            )))
            .and(query_param("dir", "f"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"event_id":"$wanted","origin_server_ts":10})),
            )
            .expect(1)
            .mount(&server)
            .await;
        context_mock(&server).await;
        let views = MatrixViews::default();
        let result = jump_to_date(&client, &views, ROOM, 10).await.unwrap();
        assert_eq!(result.event_id, "$wanted");
        assert!(matches!(
            result.page.items.last(),
            Some(TimelineItem::Gap { .. })
        ));
        assert_eq!(
            views
                .token(ROOM, result.page.next_cursor.as_deref().unwrap())
                .unwrap(),
            "context-older"
        );
    }

    #[tokio::test]
    async fn given_missing_forward_timestamp_then_backward_match_is_used() {
        let server = MockServer::start().await;
        let client = synced(&server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v1/rooms/{ROOM}/timestamp_to_event"
            )))
            .and(query_param("dir", "f"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"errcode":"M_NOT_FOUND","error":"none"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v1/rooms/{ROOM}/timestamp_to_event"
            )))
            .and(query_param("dir", "b"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"event_id":"$wanted","origin_server_ts":10})),
            )
            .expect(1)
            .mount(&server)
            .await;
        context_mock(&server).await;
        assert_eq!(
            jump_to_date(&client, &MatrixViews::default(), ROOM, 20)
                .await
                .unwrap()
                .event_id,
            "$wanted"
        );
    }

    async fn manager(server: &MockServer) -> Client {
        let mut sync = sync_reply(false, &["@alice:synthetic.test"]);
        let state = sync["rooms"]["join"][ROOM]["state"]["events"]
            .as_array_mut()
            .unwrap();
        let power = state
            .iter_mut()
            .find(|event| event["type"] == "m.room.power_levels")
            .unwrap();
        power["content"]["users"]["@alice:synthetic.test"] = json!(100);
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sync))
            .with_priority(1)
            .mount(server)
            .await;
        synced(server, false, &["@alice:synthetic.test"]).await
    }

    #[tokio::test]
    async fn given_manager_and_valid_target_then_pin_writes_fresh_state() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"pinned":["$existing"]})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path(format!("/_matrix/client/v3/rooms/{ROOM}/event/$new")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type":"m.room.message","event_id":"$new","sender":"@alice:synthetic.test","origin_server_ts":10,"content":{"msgtype":"m.text","body":"pin me"}}))).expect(1).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .and(wiremock::matchers::body_json(
                json!({"pinned":["$existing","$new"]}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id":"$state"})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(set_pinned(&client, ROOM, "$new", true).await, Ok(()));
    }

    #[tokio::test]
    async fn given_full_pin_state_then_new_pin_is_refused_without_target_fetch() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        let ids: Vec<String> = (0..MAX_PINS).map(|i| format!("$pin{i}")).collect();
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"pinned":ids})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            set_pinned(&client, ROOM, "$new", true).await,
            Err(ErrorCode::LimitExceeded)
        );
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path().ends_with("/event/$new")
                    || request.method == "PUT")
        );
    }
    #[tokio::test]
    async fn given_duplicate_pin_ids_then_one_reference_and_one_event_fetch() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"pinned":["$duplicate","$duplicate"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/event/$duplicate"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type":"m.room.message","event_id":"$duplicate","sender":"@alice:synthetic.test",
                "origin_server_ts":10,"content":{"msgtype":"m.text","body":"one pin"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        pin_relations(&server, "$duplicate", json!({"chunk":[]})).await;
        let result = pinned_messages(&client, &MatrixViews::default(), ROOM, || true)
            .await
            .unwrap();
        assert_eq!(result.event_ids, vec!["$duplicate"]);
        assert_eq!(result.items.len(), 1);
        assert!(!result.truncated);
    }
    fn text_event(id: &str, sender: &str, time: u64, body: &str) -> Value {
        json!({"type":"m.room.message","event_id":id,"sender":sender,"origin_server_ts":time,"content":{"msgtype":"m.text","body":body}})
    }

    async fn pin_state(server: &MockServer, ids: Value) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"pinned":ids})))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn pin_event(server: &MockServer, id: &str, event: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/v3/rooms/{ROOM}/event/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(event))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn pin_relations(server: &MockServer, id: &str, response: Value) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v1/rooms/{ROOM}/relations/{id}/m.replace"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(server)
            .await;
    }

    fn replacement(id: &str, sender: &str, time: u64, body: &str) -> Value {
        let mut event = text_event(id, sender, time, "* fallback");
        event["room_id"] = json!(ROOM);
        event["content"]["m.new_content"] = json!({"msgtype":"m.text","body":body});
        event["content"]["m.relates_to"] = json!({"rel_type":"m.replace","event_id":"$pin"});
        event
    }

    #[tokio::test]
    async fn given_pin_edits_then_latest_same_sender_text_wins_and_forgery_is_ignored() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        pin_state(&server, json!(["$pin"])).await;
        pin_event(
            &server,
            "$pin",
            text_event("$pin", "@alice:synthetic.test", 1, "original"),
        )
        .await;
        pin_relations(
            &server,
            "$pin",
            json!({"chunk":[
                replacement("$older","@alice:synthetic.test",2,"older edit"),
                replacement("$newest","@alice:synthetic.test",3,"current text"),
                replacement("$forged","@mallory:synthetic.test",4,"forged text")
            ]}),
        )
        .await;
        let result = pinned_messages(&client, &MatrixViews::default(), ROOM, || true)
            .await
            .unwrap();
        assert!(
            matches!(&result.items[..], [TimelineItem::Message {body,..}] if body == "current text")
        );
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn given_incomplete_or_undecryptable_pin_edits_then_original_preview_is_suppressed() {
        for mode in ["unavailable", "partial", "undecryptable"] {
            let server = MockServer::start().await;
            let client = manager(&server).await;
            pin_state(&server, json!(["$pin"])).await;
            pin_event(
                &server,
                "$pin",
                text_event(
                    "$pin",
                    "@alice:synthetic.test",
                    1,
                    "obsolete sensitive text",
                ),
            )
            .await;
            if mode == "unavailable" {
                Mock::given(method("GET"))
                    .and(path(format!(
                        "/_matrix/client/v1/rooms/{ROOM}/relations/$pin/m.replace"
                    )))
                    .respond_with(
                        ResponseTemplate::new(503)
                            .set_body_json(json!({"errcode":"M_UNKNOWN","error":"failed"})),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
            } else {
                let response = if mode == "partial" {
                    json!({"chunk":[],"next_batch":"more"})
                } else {
                    json!({"chunk":[{"type":"m.room.encrypted","event_id":"$encrypted-edit","sender":"@alice:synthetic.test","origin_server_ts":2,"content":{"algorithm":"m.megolm.v1.aes-sha2","ciphertext":"unavailable","session_id":"missing","sender_key":"missing","device_id":"unknown"}}]})
                };
                pin_relations(&server, "$pin", response).await;
            }
            let result = pinned_messages(&client, &MatrixViews::default(), ROOM, || true)
                .await
                .unwrap();
            assert_eq!(result.event_ids, vec!["$pin"]);
            assert!(result.items.is_empty(), "mode={mode}");
            assert!(result.truncated);
            assert!(
                !serde_json::to_string(&result)
                    .unwrap()
                    .contains("obsolete sensitive text")
            );
        }
    }

    #[tokio::test]
    async fn given_slow_pin_fetch_then_total_deadline_preserves_refs_and_stops_later_fetches() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        pin_state(&server, json!(["$slow", "$later"])).await;
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/v3/rooms/{ROOM}/event/$slow")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(1))
                    .set_body_json(text_event("$slow", "@alice:synthetic.test", 1, "slow")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = pinned_messages_until(
            &client,
            &MatrixViews::default(),
            ROOM,
            || true,
            Instant::now() + Duration::from_millis(150),
        )
        .await
        .unwrap();
        assert!(result.truncated);
        assert_eq!(result.event_ids, vec!["$slow", "$later"]);
        assert!(result.items.is_empty());
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path().ends_with("/event/$later"))
        );
    }

    #[tokio::test]
    async fn given_session_closes_after_state_read_then_no_pin_content_fetch_starts() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let close = open.clone();
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .respond_with(move |_: &wiremock::Request| {
                close.store(false, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({"pinned":["$pin"]}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let result = pinned_messages(&client, &MatrixViews::default(), ROOM, || {
            open.load(std::sync::atomic::Ordering::SeqCst)
        })
        .await
        .unwrap();
        assert!(result.truncated);
        assert_eq!(result.event_ids, vec!["$pin"]);
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path().contains("/event/"))
        );
    }

    async fn ignored_date_fixture(server: &MockServer, later: bool) -> Client {
        let mut sync = sync_reply(false, &["@alice:synthetic.test"]);
        sync["account_data"]["events"].as_array_mut().unwrap().push(json!({"type":"m.ignored_user_list","content":{"ignored_users":{"@ignored:synthetic.test":{}}}}));
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sync))
            .with_priority(1)
            .mount(server)
            .await;
        let client = synced(server, false, &["@alice:synthetic.test"]).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/_matrix/client/v1/rooms/{ROOM}/timestamp_to_event"
            )))
            .and(query_param("dir", "f"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"event_id":"$hidden","origin_server_ts":10})),
            )
            .expect(1)
            .mount(server)
            .await;
        let after = if later {
            vec![text_event(
                "$visible",
                "@alice:synthetic.test",
                11,
                "visible",
            )]
        } else {
            vec![]
        };
        Mock::given(method("GET")).and(path(format!("/_matrix/client/v3/rooms/{ROOM}/context/$hidden")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event":text_event("$hidden","@ignored:synthetic.test",10,"hidden"),"events_before":[],"events_after":after,"state":[],"start":"older","end":"newer"}))).expect(1).mount(server).await;
        client
    }

    #[tokio::test]
    async fn given_ignored_date_target_then_visible_later_context_event_is_selected() {
        let server = MockServer::start().await;
        let client = ignored_date_fixture(&server, true).await;
        let result = jump_to_date(&client, &MatrixViews::default(), ROOM, 10)
            .await
            .unwrap();
        assert_eq!(result.event_id, "$visible");
        assert!(
            !result
                .page
                .items
                .iter()
                .any(|item| item_id(item) == Some("$hidden"))
        );
    }

    #[tokio::test]
    async fn given_no_visible_date_target_then_not_found_without_extra_history_scan() {
        let server = MockServer::start().await;
        let client = ignored_date_fixture(&server, false).await;
        assert!(matches!(
            jump_to_date(&client, &MatrixViews::default(), ROOM, 10).await,
            Err(ErrorCode::NotFound)
        ));
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path().ends_with("/messages"))
        );
    }

    #[tokio::test]
    async fn given_attachment_page_with_edit_then_no_original_event_lookup_occurs() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        Mock::given(method("GET")).and(path(format!("/_matrix/client/v3/rooms/{ROOM}/messages")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"start":"latest","end":"older","state":[],"chunk":[replacement("$edit","@alice:synthetic.test",2,"replacement")]})))
            .expect(1).mount(&server).await;
        let result = attachments(
            &client,
            &MatrixViews::default(),
            ROOM,
            None,
            AttachmentKind::All,
            &LocalSearchFilters::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.scanned, 1);
        assert!(result.items.is_empty());
        assert!(result.next_cursor.is_some());
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path().contains("/event/"))
        );
    }
    #[tokio::test]
    async fn given_equal_time_pin_edits_then_event_id_breaks_ties_in_either_order() {
        for reverse in [false, true] {
            let server = MockServer::start().await;
            let client = manager(&server).await;
            pin_state(&server, json!(["$pin"])).await;
            pin_event(
                &server,
                "$pin",
                text_event("$pin", "@alice:synthetic.test", 1, "original"),
            )
            .await;
            let mut edits = vec![
                replacement("$a", "@alice:synthetic.test", 2, "tie loser"),
                replacement("$z", "@alice:synthetic.test", 2, "tie winner"),
            ];
            if reverse {
                edits.reverse();
            }
            pin_relations(&server, "$pin", json!({"chunk": edits})).await;
            let result = pinned_messages(&client, &MatrixViews::default(), ROOM, || true)
                .await
                .unwrap();
            assert!(
                matches!(&result.items[..], [TimelineItem::Message { body, .. }] if body == "tie winner")
            );
        }
    }

    #[tokio::test]
    async fn given_101_pins_then_removal_preserves_every_other_id_in_order() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        let ids: Vec<String> = (0..101).map(|i| format!("$pin{i}")).collect();
        pin_state(&server, json!(ids)).await;
        let expected: Vec<&String> = ids.iter().filter(|id| id.as_str() != "$pin50").collect();
        Mock::given(method("PUT"))
            .and(path(format!(
                "/_matrix/client/v3/rooms/{ROOM}/state/m.room.pinned_events/"
            )))
            .and(wiremock::matchers::body_json(json!({"pinned":expected})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id":"$state"})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(set_pinned(&client, ROOM, "$pin50", false).await, Ok(()));
    }

    #[tokio::test]
    async fn given_101_pins_then_add_is_refused_without_fetch_or_write() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        pin_state(
            &server,
            json!((0..101).map(|i| format!("$pin{i}")).collect::<Vec<_>>()),
        )
        .await;
        assert_eq!(
            set_pinned(&client, ROOM, "$new", true).await,
            Err(ErrorCode::LimitExceeded)
        );
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.method == "PUT" || request.url.path().contains("/event/"))
        );
    }

    #[tokio::test]
    async fn given_pin_state_above_removal_count_cap_then_no_write_occurs() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        pin_state(
            &server,
            json!(
                (0..=MAX_PIN_STATE_IDS)
                    .map(|i| format!("$pin{i}"))
                    .collect::<Vec<_>>()
            ),
        )
        .await;
        assert_eq!(
            set_pinned(&client, ROOM, "$pin0", false).await,
            Err(ErrorCode::LimitExceeded)
        );
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.method == "PUT")
        );
    }

    #[tokio::test]
    async fn given_removal_content_above_byte_budget_then_no_write_occurs() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        let ids: Vec<String> = (0..MAX_PIN_STATE_IDS)
            .map(|i| format!("${i:03}{}", "a".repeat(MAX_EVENT_ID_BYTES - 4)))
            .collect();
        assert!(ids.iter().all(|id| id.len() == MAX_EVENT_ID_BYTES));
        pin_state(&server, json!(ids)).await;
        assert_eq!(
            set_pinned(&client, ROOM, &ids[0], false).await,
            Err(ErrorCode::LimitExceeded)
        );
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.method == "PUT")
        );
    }
    #[tokio::test]
    async fn given_untrustworthy_pin_relation_room_then_only_that_preview_is_suppressed() {
        for room_id in [json!("!other:synthetic.test"), json!(17)] {
            let server = MockServer::start().await;
            let client = manager(&server).await;
            pin_state(&server, json!(["$pin", "$healthy"])).await;
            pin_event(
                &server,
                "$pin",
                text_event(
                    "$pin",
                    "@alice:synthetic.test",
                    1,
                    "obsolete sensitive text",
                ),
            )
            .await;
            let mut bad = replacement(
                "$bad-room",
                "@alice:synthetic.test",
                2,
                "untrusted replacement",
            );
            bad["room_id"] = room_id;
            pin_relations(&server, "$pin", json!({"chunk":[bad]})).await;
            pin_event(
                &server,
                "$healthy",
                text_event("$healthy", "@alice:synthetic.test", 3, "healthy preview"),
            )
            .await;
            pin_relations(&server, "$healthy", json!({"chunk":[]})).await;
            let result = pinned_messages(&client, &MatrixViews::default(), ROOM, || true)
                .await
                .unwrap();
            assert_eq!(result.event_ids, vec!["$pin", "$healthy"]);
            assert!(result.truncated);
            assert!(
                matches!(&result.items[..], [TimelineItem::Message { event_id, body, .. }] if event_id == "$healthy" && body == "healthy preview")
            );
            let serialized = serde_json::to_string(&result).unwrap();
            assert!(!serialized.contains("obsolete sensitive text"));
            assert!(!serialized.contains("untrusted replacement"));
        }
    }

    #[tokio::test]
    async fn given_original_room_mismatch_then_bad_relation_does_not_mask_rejection() {
        let server = MockServer::start().await;
        let client = manager(&server).await;
        pin_state(&server, json!(["$pin"])).await;
        let mut original = text_event("$pin", "@alice:synthetic.test", 1, "wrong room");
        original["room_id"] = json!("!other:synthetic.test");
        pin_event(&server, "$pin", original).await;
        let mut bad = replacement("$bad-room", "@alice:synthetic.test", 2, "replacement");
        bad["room_id"] = json!(17);
        pin_relations(&server, "$pin", json!({"chunk":[bad]})).await;
        assert!(matches!(
            pinned_messages(&client, &MatrixViews::default(), ROOM, || true).await,
            Err(ErrorCode::Rejected)
        ));
    }
}
