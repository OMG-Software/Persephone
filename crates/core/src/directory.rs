//! Public room directories: listing a page of one, and joining a room it
//! listed.
//!
//! * A page comes from this account's homeserver's directory, or, when the
//!   page names a server, from that server's directory, which the homeserver
//!   asks over federation. Nothing connects to the other server directly.
//!   Both servers see the filter. Many homeservers refuse federated directory
//!   requests; that answer is `rejected`.
//! * Everything in a page is the directory's word. Names and topics are
//!   projected like names and message bodies, and topics are cut short. Room
//!   IDs and addresses are sent unchanged, and only when they are valid and
//!   visible ASCII; a room without a valid ID is left out. Spaces are left
//!   out: Persephone does not show them. Avatars are not sent: the page
//!   fetches no remote images.
//! * The page can join only a room that a page of this session listed as
//!   open to anyone, and only through the server that listed it. The core
//!   remembers at most [`MAX_OFFERED_ROOMS`] such rooms, oldest forgotten
//!   first, and forgets them all when the session changes. Any other room is
//!   `not_found`. This is a weak limit, not a consent check: the page picks
//!   the server, and a server can list any room as open (ADR-0037). It keeps
//!   the page from choosing the join servers, joining by address, or joining
//!   without a directory listing the room.
//! * Public rooms are usually not encrypted. Joining one does not change
//!   ADR-0005: sending there still needs the user's consent.

use crate::views::{project_body, project_name};
use matrix_sdk::{
    Client, RoomState,
    ruma::{
        OwnedRoomId, OwnedServerName, RoomId, ServerName, UInt,
        api::client::directory::get_public_rooms_filtered,
        directory::{Filter, PublicRoomsChunk, RoomTypeFilter},
        room::JoinRuleKind,
    },
};
use persephone_contracts::{JoinedRoom, PublicRoom, PublicRoomPage};
use persephone_core_protocol::{
    ErrorCode, MAX_DIRECTORY_FILTER_BYTES, MAX_DIRECTORY_TOKEN_BYTES, MAX_OFFERED_ROOMS,
    MAX_PUBLIC_ROOMS, MAX_ROOM_ID_BYTES, MAX_SERVER_NAME_BYTES,
};
use std::collections::VecDeque;

/// Most characters of a room topic sent to the page.
pub const MAX_PUBLIC_TOPIC_CHARS: usize = 500;

/// Longest room address sent to the page, in bytes.
const MAX_ALIAS_BYTES: usize = 255;

/// A server name the page sent: a valid Matrix server name within the
/// limit, made of visible ASCII.
pub fn parse_server(server: &str) -> Option<OwnedServerName> {
    (server.len() <= MAX_SERVER_NAME_BYTES && server.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| ServerName::parse(server).ok())
        .flatten()
}

/// A directory filter the page sent: some visible text, within the limit,
/// and no control characters.
pub fn valid_filter(filter: &str) -> bool {
    filter.len() <= MAX_DIRECTORY_FILTER_BYTES
        && !filter.trim().is_empty()
        && !filter.chars().any(char::is_control)
}

/// A page token: not empty, within the limit and visible ASCII. The core
/// sends only such tokens to the page, so the page can send back only such
/// tokens.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_DIRECTORY_TOKEN_BYTES
        && token.bytes().all(|b| b.is_ascii_graphic())
}

/// A validated `public_rooms` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub server: Option<OwnedServerName>,
    pub filter: Option<String>,
    pub since: Option<String>,
}

impl PageRequest {
    /// The request the page sent, if every part is valid. An empty server
    /// name is refused: the page sends none for its own homeserver.
    pub fn parse(server: Option<&str>, filter: Option<&str>, since: Option<&str>) -> Option<Self> {
        let server = match server {
            Some(server) => Some(parse_server(server)?),
            None => None,
        };
        if filter.is_some_and(|filter| !valid_filter(filter))
            || since.is_some_and(|since| !valid_token(since))
        {
            return None;
        }
        Some(Self {
            server,
            filter: filter.map(|filter| filter.trim().to_owned()),
            since: since.map(ToOwned::to_owned),
        })
    }
}

/// Rooms the page may join: those a page of this session listed as open to
/// anyone, with the server whose directory listed them.
#[derive(Default)]
pub struct Offers {
    generation: u64,
    rooms: VecDeque<(OwnedRoomId, Option<OwnedServerName>)>,
}

impl Offers {
    /// Remembers the joinable rooms of a page listed in `generation`. A new
    /// generation forgets every room of the one before; a page from an older
    /// generation, which finished after the session changed, is ignored. A
    /// room listed again moves to the newest place, with the server that
    /// listed it last.
    pub fn remember(
        &mut self,
        generation: u64,
        rooms: impl IntoIterator<Item = (OwnedRoomId, Option<OwnedServerName>)>,
    ) {
        if generation < self.generation {
            return;
        }
        if self.generation != generation {
            self.generation = generation;
            self.rooms.clear();
        }
        for (room_id, server) in rooms {
            self.rooms.retain(|(known, _)| *known != room_id);
            if self.rooms.len() == MAX_OFFERED_ROOMS {
                self.rooms.pop_front();
            }
            self.rooms.push_back((room_id, server));
        }
    }

    /// The servers to join `room_id` through, if a page of `generation`
    /// listed it as joinable.
    pub fn via(&self, generation: u64, room_id: &RoomId) -> Option<Vec<OwnedServerName>> {
        if self.generation != generation {
            return None;
        }
        self.rooms
            .iter()
            .find(|(known, _)| known == room_id)
            .map(|(_, server)| server.iter().cloned().collect())
    }
}

/// A room ID or address sent unchanged, if it is valid, within the limit
/// and visible ASCII.
fn identifier(value: &str, max: usize) -> Option<String> {
    (value.len() <= max && value.bytes().all(|b| b.is_ascii_graphic())).then(|| value.to_owned())
}

/// A topic projected like a message body and cut short.
fn topic(raw: &str) -> Option<String> {
    let projected = project_body(raw.trim());
    if projected.is_empty() {
        return None;
    }
    let mut chars = projected.char_indices();
    match chars.nth(MAX_PUBLIC_TOPIC_CHARS) {
        Some((end, _)) => Some(format!("{}…", &projected[..end])),
        None => Some(projected),
    }
}

/// The page's view of one directory entry, or none for an entry the page
/// does not show: a space, or one without a valid room ID.
fn project(chunk: &PublicRoomsChunk, joined: impl Fn(&RoomId) -> bool) -> Option<PublicRoom> {
    if chunk.room_type.is_some() {
        return None;
    }
    let room_id = identifier(chunk.room_id.as_str(), MAX_ROOM_ID_BYTES)?;
    Some(PublicRoom {
        room_id,
        name: chunk
            .name
            .as_deref()
            .map(project_name)
            .filter(|name| !name.is_empty()),
        alias: chunk
            .canonical_alias
            .as_ref()
            .and_then(|alias| identifier(alias.as_str(), MAX_ALIAS_BYTES)),
        topic: chunk.topic.as_deref().and_then(topic),
        members: u32::try_from(chunk.num_joined_members).unwrap_or(u32::MAX),
        world_readable: chunk.world_readable,
        guests_can_join: chunk.guest_can_join,
        joinable: chunk.join_rule == JoinRuleKind::Public,
        joined: joined(&chunk.room_id),
    })
}

/// Projects a directory reply: at most [`MAX_PUBLIC_ROOMS`] rooms, and the
/// next token only when it is one the page may send back. Returns the page
/// and the rooms it offers to join.
fn page(
    response: get_public_rooms_filtered::v3::Response,
    server: Option<&OwnedServerName>,
    joined: impl Fn(&RoomId) -> bool,
) -> (PublicRoomPage, Vec<(OwnedRoomId, Option<OwnedServerName>)>) {
    let mut rooms = Vec::new();
    let mut offers = Vec::new();
    for chunk in response.chunk.iter().take(MAX_PUBLIC_ROOMS) {
        let Some(room) = project(chunk, &joined) else {
            continue;
        };
        if room.joinable {
            offers.push((chunk.room_id.clone(), server.cloned()));
        }
        rooms.push(room);
    }
    let next = response.next_batch.filter(|token| valid_token(token));
    let total = response
        .total_room_count_estimate
        .map(|total| u32::try_from(total).unwrap_or(u32::MAX));
    (PublicRoomPage { rooms, next, total }, offers)
}

/// A refusal from the homeserver (a 4xx answer) is `rejected`. Anything
/// transient is `unavailable`: no answer at all, a server error (5xx), or a
/// rate limit (429), as for media (`media.rs`).
fn request_error(error: Option<&matrix_sdk::ruma::api::error::Error>) -> ErrorCode {
    match error {
        Some(error) if error.status_code.is_client_error() && error.status_code.as_u16() != 429 => {
            ErrorCode::Rejected
        }
        _ => ErrorCode::Unavailable,
    }
}

/// Lists one page of a public room directory. Returns the page and the rooms
/// it offers to join.
pub async fn list(
    client: &Client,
    request: &PageRequest,
) -> Result<(PublicRoomPage, Vec<(OwnedRoomId, Option<OwnedServerName>)>), ErrorCode> {
    let mut filter = Filter::new();
    filter.generic_search_term = request.filter.clone();
    // Ordinary rooms only; a server that ignores this still has its spaces
    // left out by `project`.
    filter.room_types = vec![RoomTypeFilter::Default];
    let mut call = get_public_rooms_filtered::v3::Request::new();
    call.server = request.server.clone();
    call.limit = Some(UInt::from(MAX_PUBLIC_ROOMS as u32));
    call.since = request.since.clone();
    call.filter = filter;
    let response = client
        .public_rooms_filtered(call)
        .await
        .map_err(|error| request_error(error.as_client_api_error()))?;
    Ok(page(response, request.server.as_ref(), |room_id| {
        client
            .get_room(room_id)
            .is_some_and(|room| room.state() == RoomState::Joined)
    }))
}

/// Joins a listed room through the server that listed it.
pub async fn join(
    client: &Client,
    room_id: &RoomId,
    via: &[OwnedServerName],
) -> Result<JoinedRoom, ErrorCode> {
    let room = client
        .join_room_by_id_or_alias(room_id.into(), via)
        .await
        .map_err(|error| request_error(error.as_client_api_error()))?;
    Ok(JoinedRoom {
        room_id: room.room_id().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{
        RoomAliasId, directory::PublicRoomsChunkInit, room::RoomType, room_alias_id, room_id,
    };

    fn chunk(room_id: &RoomId) -> PublicRoomsChunk {
        PublicRoomsChunkInit {
            num_joined_members: UInt::from(3u32),
            room_id: room_id.to_owned(),
            world_readable: false,
            guest_can_join: false,
        }
        .into()
    }

    fn response(chunks: Vec<PublicRoomsChunk>) -> get_public_rooms_filtered::v3::Response {
        let mut response = get_public_rooms_filtered::v3::Response::new();
        response.chunk = chunks;
        response
    }

    /// GIVEN server names from the page WHEN parsed THEN only valid Matrix
    /// server names within the limit pass.
    #[test]
    fn given_server_names_then_only_valid_names_within_the_limit_pass() {
        for valid in [
            "example.org",
            "matrix.example.org:8448",
            "127.0.0.1",
            "[::1]:8448",
        ] {
            assert!(parse_server(valid).is_some(), "{valid}");
        }
        let long = format!("{}.org", "a".repeat(MAX_SERVER_NAME_BYTES));
        for invalid in [
            "",
            " example.org",
            "example.org/",
            "https://example.org",
            "@bob:example.org",
            "exa mple.org",
            "ex\u{e4}mple.org",
            long.as_str(),
        ] {
            assert!(parse_server(invalid).is_none(), "{invalid:?}");
        }
    }

    /// GIVEN a page request WHEN parsed THEN no server means the own
    /// homeserver, the filter is trimmed, and blank, oversized or
    /// control-bearing parts refuse the whole request.
    #[test]
    fn given_page_requests_then_invalid_parts_refuse_the_request() {
        assert_eq!(
            PageRequest::parse(None, None, None),
            Some(PageRequest {
                server: None,
                filter: None,
                since: None
            })
        );
        let parsed = PageRequest::parse(Some("example.org"), Some(" rust "), Some("p30")).unwrap();
        assert_eq!(
            parsed.server.as_deref().map(ServerName::as_str),
            Some("example.org")
        );
        assert_eq!(parsed.filter.as_deref(), Some("rust"));
        assert_eq!(parsed.since.as_deref(), Some("p30"));
        let long_filter = "a".repeat(MAX_DIRECTORY_FILTER_BYTES + 1);
        let long_token = "a".repeat(MAX_DIRECTORY_TOKEN_BYTES + 1);
        for (server, filter, since) in [
            (Some(""), None, None),
            (Some("bad name"), None, None),
            (None, Some(""), None),
            (None, Some("   "), None),
            (None, Some("a\u{7}"), None),
            (None, Some(long_filter.as_str()), None),
            (None, None, Some("")),
            (None, None, Some("p 30")),
            (None, None, Some(long_token.as_str())),
        ] {
            assert!(
                PageRequest::parse(server, filter, since).is_none(),
                "{server:?} {filter:?} {since:?}"
            );
        }
    }

    /// GIVEN a directory reply WHEN projected THEN names and topics are
    /// projected and cut short, spaces and rooms with odd IDs are left out,
    /// only rooms open to anyone are offered, and joined rooms are marked.
    #[test]
    fn given_a_directory_reply_then_the_page_is_projected_and_bounded() {
        let mut open = chunk(room_id!("!open:example.org"));
        open.name = Some("Rust\u{202e} talk".into());
        open.canonical_alias = Some(room_alias_id!("#rust:example.org").to_owned());
        open.topic = Some("a".repeat(MAX_PUBLIC_TOPIC_CHARS + 10));
        open.world_readable = true;
        let mut knock = chunk(room_id!("!knock:example.org"));
        knock.join_rule = JoinRuleKind::Knock;
        let mut space = chunk(room_id!("!space:example.org"));
        space.room_type = Some(RoomType::Space);
        let joined_room = chunk(room_id!("!joined:example.org"));
        let server = ServerName::parse("example.org").unwrap();
        let (page, offers) = page(
            response(vec![open, knock, space, joined_room]),
            Some(&server),
            |room_id| room_id == "!joined:example.org",
        );
        let ids: Vec<&str> = page
            .rooms
            .iter()
            .map(|room| room.room_id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "!open:example.org",
                "!knock:example.org",
                "!joined:example.org"
            ]
        );
        let open = &page.rooms[0];
        assert_eq!(open.name.as_deref(), Some("Rust\u{fffd} talk"));
        assert_eq!(open.alias.as_deref(), Some("#rust:example.org"));
        let topic = open.topic.as_deref().unwrap();
        assert_eq!(topic.chars().count(), MAX_PUBLIC_TOPIC_CHARS + 1);
        assert!(topic.ends_with('…'));
        assert!(open.world_readable && open.joinable && !open.joined);
        assert!(!page.rooms[1].joinable);
        assert!(page.rooms[2].joined);
        let offered: Vec<&str> = offers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(offered, ["!open:example.org", "!joined:example.org"]);
        assert!(offers.iter().all(|(_, via)| via.as_ref() == Some(&server)));
    }

    /// GIVEN a reply with too many rooms and odd tokens WHEN projected THEN
    /// at most the page limit is listed, and a token the page could not
    /// send back ends the listing.
    #[test]
    fn given_too_many_rooms_or_odd_tokens_then_the_page_is_cut_and_ends() {
        let chunks: Vec<PublicRoomsChunk> = (0..MAX_PUBLIC_ROOMS + 5)
            .map(|n| chunk(&RoomId::parse(format!("!r{n}:example.org")).unwrap()))
            .collect();
        let mut reply = response(chunks);
        reply.next_batch = Some("p30".into());
        reply.total_room_count_estimate = Some(UInt::from(1000u32));
        let (listed, offers) = page(reply, None, |_| false);
        assert_eq!(listed.rooms.len(), MAX_PUBLIC_ROOMS);
        assert_eq!(offers.len(), MAX_PUBLIC_ROOMS);
        assert!(offers.iter().all(|(_, via)| via.is_none()));
        assert_eq!(listed.next.as_deref(), Some("p30"));
        assert_eq!(listed.total, Some(1000));
        for odd in [
            "p 30".to_owned(),
            "a".repeat(MAX_DIRECTORY_TOKEN_BYTES + 1),
            String::new(),
        ] {
            let mut reply = response(Vec::new());
            reply.next_batch = Some(odd.clone());
            assert_eq!(page(reply, None, |_| false).0.next, None, "{odd:?}");
        }
    }

    /// GIVEN a full page of the longest names, addresses, topics and IDs the
    /// core sends, made of four-byte characters and characters JSON escapes,
    /// WHEN projected THEN it still fits one bounded reply, so a large
    /// directory page is never refused for its size.
    #[test]
    fn given_a_worst_case_page_then_it_fits_one_bounded_reply() {
        let quoted = |prefix: &str, len: usize| format!("{prefix}{}:s", "\"".repeat(len - 3));
        let chunks: Vec<PublicRoomsChunk> = (0..MAX_PUBLIC_ROOMS)
            .map(|n| {
                let mut id = quoted("!", MAX_ROOM_ID_BYTES);
                id.replace_range(1..4, &format!("{n:03}"));
                let mut chunk = chunk(&RoomId::parse(&id).expect("a loose room ID parses"));
                chunk.name = Some("\u{1F600}".repeat(1000));
                chunk.topic = Some("\u{1F600}".repeat(1000));
                chunk.canonical_alias =
                    Some(RoomAliasId::parse(quoted("#", MAX_ALIAS_BYTES)).expect("alias parses"));
                chunk.num_joined_members = UInt::MAX;
                chunk
            })
            .collect();
        let mut reply = response(chunks);
        reply.next_batch = Some("\\".repeat(MAX_DIRECTORY_TOKEN_BYTES));
        reply.total_room_count_estimate = Some(UInt::MAX);
        let (listed, _) = page(reply, None, |_| false);
        assert_eq!(listed.rooms.len(), MAX_PUBLIC_ROOMS);
        assert!(listed.rooms.iter().all(|room| room.alias.is_some()));
        assert!(listed.next.is_some());
        let bytes = serde_json::to_vec(&listed).unwrap().len();
        assert!(
            bytes <= crate::views::MAX_PAGE_BYTES,
            "{bytes} > {}",
            crate::views::MAX_PAGE_BYTES
        );
    }

    /// GIVEN a listing the page shows in full, whose last page offers a whole
    /// page past the rooms it shows WHEN the page asks to join THEN every
    /// room it shows is still joinable.
    #[test]
    fn given_a_full_listing_then_every_shown_room_stays_joinable() {
        use persephone_core_protocol::MAX_LISTED_ROOMS;
        let mut offers = Offers::default();
        let pages = MAX_LISTED_ROOMS / MAX_PUBLIC_ROOMS + 1;
        for page in 0..pages {
            offers.remember(
                1,
                (0..MAX_PUBLIC_ROOMS).map(|n| {
                    let id = format!("!p{page}r{n}:example.org");
                    (RoomId::parse(id).unwrap(), None)
                }),
            );
        }
        for shown in 0..MAX_LISTED_ROOMS {
            let (page, n) = (shown / MAX_PUBLIC_ROOMS, shown % MAX_PUBLIC_ROOMS);
            let id = RoomId::parse(format!("!p{page}r{n}:example.org")).unwrap();
            assert!(offers.via(1, &id).is_some(), "{id}");
        }
    }

    /// GIVEN listed rooms WHEN the page asks to join THEN only a room listed
    /// as joinable in this session is found, with the server that listed it
    /// last; the oldest are forgotten past the limit, a new session forgets
    /// them all, and a late page of an old session changes nothing.
    #[test]
    fn given_offers_then_only_rooms_listed_in_this_session_are_joinable() {
        let server = ServerName::parse("example.org").unwrap();
        let other = ServerName::parse("other.example").unwrap();
        let first = room_id!("!first:example.org");
        let mut offers = Offers::default();
        offers.remember(1, [(first.to_owned(), Some(server.clone()))]);
        assert_eq!(offers.via(1, first), Some(vec![server.clone()]));
        assert_eq!(offers.via(1, room_id!("!never:example.org")), None);
        assert_eq!(offers.via(2, first), None);
        // Listed again by another server: joined through that one.
        offers.remember(1, [(first.to_owned(), Some(other.clone()))]);
        assert_eq!(offers.via(1, first), Some(vec![other]));
        // Own homeserver's directory: no server to name.
        let own = room_id!("!own:example.org");
        offers.remember(1, [(own.to_owned(), None)]);
        assert_eq!(offers.via(1, own), Some(Vec::new()));
        // Past the limit, the oldest goes first.
        offers.remember(
            1,
            (0..MAX_OFFERED_ROOMS - 1)
                .map(|n| (RoomId::parse(format!("!r{n}:example.org")).unwrap(), None)),
        );
        assert_eq!(offers.via(1, first), None);
        assert_eq!(offers.via(1, own), Some(Vec::new()));
        // A new session forgets the old one's rooms.
        offers.remember(2, []);
        assert_eq!(offers.via(2, own), None);
        let newer = room_id!("!newer:example.org");
        offers.remember(2, [(newer.to_owned(), None)]);
        // A page of the old session that finishes late changes nothing.
        offers.remember(1, [(own.to_owned(), None)]);
        assert_eq!(offers.via(1, own), None);
        assert_eq!(offers.via(2, own), None);
        assert_eq!(offers.via(2, newer), Some(Vec::new()));
    }
}
