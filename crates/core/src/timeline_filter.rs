//! Which timeline items the account's switches (`TimelineEvents`) show.
//!
//! The core applies this before it fills a page, so a run of hidden events,
//! such as call connection details or live location updates, does not take
//! the page's places from messages. The page applies the same rules again
//! (`frontend/src/eventText.ts`), because it marks items deleted later and
//! shows a change of a switch before the next page arrives.
//!
//! Messages, images, files, gaps, encryption notices and messages of a type
//! this build has no text for always show: hiding a message that could not
//! be decrypted would hide that it exists. Edits, reactions and deletions
//! are not lines; they always pass, so the page can apply them.

use persephone_contracts::{
    MembershipChange, NoticeKind, OtherEvent, StateChange, TimelineEvents, TimelineItem,
};

/// Whether the timeline shows `item` with these switches.
pub fn shown(item: &TimelineItem, events: &TimelineEvents) -> bool {
    match item {
        TimelineItem::State { change, .. } => change_switch(change).is_none_or(|on| on(events)),
        TimelineItem::Other { event, deleted, .. } => {
            other_switch(*event).is_none_or(|on| on(events))
                && (!*deleted || events.deleted_messages)
        }
        TimelineItem::Notice { notice, .. } => {
            *notice != NoticeKind::Deleted || events.deleted_messages
        }
        TimelineItem::Message { .. }
        | TimelineItem::Image { .. }
        | TimelineItem::File { .. }
        | TimelineItem::Gap { .. }
        | TimelineItem::Edit { .. }
        | TimelineItem::Reaction { .. }
        | TimelineItem::Redaction { .. } => true,
    }
}

/// A switch, read from the settings.
type Switch = fn(&TimelineEvents) -> bool;

/// The switch for a room change, or `None` for one no switch hides.
fn change_switch(change: &StateChange) -> Option<Switch> {
    Some(match change {
        StateChange::Member { membership, .. } => match membership {
            MembershipChange::Joined => |e| e.joins,
            MembershipChange::Left => |e| e.leaves,
            MembershipChange::Invited
            | MembershipChange::InviteRejected
            | MembershipChange::InviteWithdrawn => |e| e.invitations,
            MembershipChange::Kicked | MembershipChange::Banned | MembershipChange::Unbanned => {
                |e| e.removals
            }
            MembershipChange::Knocked
            | MembershipChange::KnockWithdrawn
            | MembershipChange::KnockDenied => |e| e.join_requests,
        },
        StateChange::DisplayName { .. } => |e| e.display_names,
        StateChange::ProfilePicture { .. } => |e| e.profile_pictures,
        StateChange::Name { .. } => |e| e.room_name,
        StateChange::Topic { .. } => |e| e.topic,
        StateChange::Avatar {} => |e| e.room_picture,
        StateChange::Encryption {} => |e| e.encryption,
        StateChange::JoinRule { .. } => |e| e.join_rule,
        StateChange::HistoryVisibility { .. } => |e| e.history_visibility,
        StateChange::PowerLevels {} => |e| e.permissions,
        StateChange::Addresses {} => |e| e.addresses,
        StateChange::PinnedMessages {} => |e| e.pinned_messages,
        StateChange::GuestAccess { .. } => |e| e.guest_access,
        StateChange::ServerAcl {} => |e| e.server_access,
        StateChange::Created {} => |e| e.creation,
        StateChange::Upgraded {} => |e| e.upgrades,
    })
}

/// The switch for an other event's kind, or `None` for a message, which no
/// switch hides.
fn other_switch(event: OtherEvent) -> Option<Switch> {
    Some(match event {
        OtherEvent::VerificationRequest
        | OtherEvent::VerificationReady
        | OtherEvent::VerificationStart
        | OtherEvent::VerificationStep
        | OtherEvent::VerificationCancelled
        | OtherEvent::VerificationDone => |e| e.verification,
        OtherEvent::CallStarted
        | OtherEvent::CallAnswered
        | OtherEvent::CallDeclined
        | OtherEvent::CallEnded
        | OtherEvent::CallJoined
        | OtherEvent::CallLeft => |e| e.calls,
        OtherEvent::CallSignalling | OtherEvent::CallUpdated => |e| e.call_signalling,
        OtherEvent::PollStarted | OtherEvent::PollVote | OtherEvent::PollEnded => |e| e.polls,
        OtherEvent::Sticker => |e| e.stickers,
        OtherEvent::Location | OtherEvent::LocationStopped => |e| e.locations,
        OtherEvent::SpaceChild | OtherEvent::SpaceParent => |e| e.spaces,
        OtherEvent::Widget => |e| e.widgets,
        OtherEvent::ThirdPartyInvite => |e| e.invitations,
        OtherEvent::Message => return None,
        OtherEvent::Event | OtherEvent::State => |e| e.other_events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_contracts::UnixMillis;

    type Key = fn(&mut TimelineEvents) -> &mut bool;

    fn other(event: OtherEvent, deleted: bool) -> TimelineItem {
        TimelineItem::Other {
            event_id: "$o".into(),
            sender: "@a:example.org".into(),
            origin_server_ts: Some(UnixMillis::new(1).unwrap()),
            event,
            event_type: "t".into(),
            state: false,
            deleted,
        }
    }

    /// Every switch on but `key`.
    fn only_off(key: Key) -> TimelineEvents {
        let mut events = TimelineEvents::default();
        *key(&mut events) = false;
        events
    }

    /// Every switch off.
    fn all_off() -> TimelineEvents {
        let mut value = serde_json::to_value(TimelineEvents::default()).unwrap();
        for on in value.as_object_mut().unwrap().values_mut() {
            *on = serde_json::Value::Bool(false);
        }
        serde_json::from_value(value).unwrap()
    }

    /// GIVEN every other kind WHEN its switch is off THEN it is hidden, by
    /// the same switch the page uses (`frontend/src/eventText.ts`); a
    /// message of an unknown type has no switch.
    #[test]
    fn given_each_other_kind_then_its_own_switch_hides_it() {
        let cases: [(OtherEvent, Option<Key>); 27] = [
            (
                OtherEvent::VerificationRequest,
                Some(|e| &mut e.verification),
            ),
            (OtherEvent::VerificationReady, Some(|e| &mut e.verification)),
            (OtherEvent::VerificationStart, Some(|e| &mut e.verification)),
            (OtherEvent::VerificationStep, Some(|e| &mut e.verification)),
            (
                OtherEvent::VerificationCancelled,
                Some(|e| &mut e.verification),
            ),
            (OtherEvent::VerificationDone, Some(|e| &mut e.verification)),
            (OtherEvent::CallStarted, Some(|e| &mut e.calls)),
            (OtherEvent::CallAnswered, Some(|e| &mut e.calls)),
            (OtherEvent::CallDeclined, Some(|e| &mut e.calls)),
            (OtherEvent::CallEnded, Some(|e| &mut e.calls)),
            (OtherEvent::CallJoined, Some(|e| &mut e.calls)),
            (OtherEvent::CallLeft, Some(|e| &mut e.calls)),
            (OtherEvent::CallUpdated, Some(|e| &mut e.call_signalling)),
            (OtherEvent::CallSignalling, Some(|e| &mut e.call_signalling)),
            (OtherEvent::PollStarted, Some(|e| &mut e.polls)),
            (OtherEvent::PollVote, Some(|e| &mut e.polls)),
            (OtherEvent::PollEnded, Some(|e| &mut e.polls)),
            (OtherEvent::Sticker, Some(|e| &mut e.stickers)),
            (OtherEvent::Location, Some(|e| &mut e.locations)),
            (OtherEvent::LocationStopped, Some(|e| &mut e.locations)),
            (OtherEvent::SpaceChild, Some(|e| &mut e.spaces)),
            (OtherEvent::SpaceParent, Some(|e| &mut e.spaces)),
            (OtherEvent::Widget, Some(|e| &mut e.widgets)),
            (OtherEvent::ThirdPartyInvite, Some(|e| &mut e.invitations)),
            (OtherEvent::Event, Some(|e| &mut e.other_events)),
            (OtherEvent::State, Some(|e| &mut e.other_events)),
            (OtherEvent::Message, None),
        ];
        for (event, key) in cases {
            let item = other(event, false);
            assert!(shown(&item, &TimelineEvents::default()), "{event:?}");
            match key {
                Some(key) => assert!(!shown(&item, &only_off(key)), "{event:?}"),
                None => assert!(shown(&item, &all_off()), "{event:?}"),
            }
        }
    }

    /// GIVEN deleted lines switched off THEN a deleted other event and a
    /// deleted notice go, while a message and an encryption notice stay; a
    /// deleted other event also follows its kind's switch.
    #[test]
    fn given_deleted_lines_off_then_only_deleted_lines_go() {
        let no_deleted = only_off(|e| &mut e.deleted_messages);
        let notice = |notice| TimelineItem::Notice {
            event_id: "$n".into(),
            sender: "@a:example.org".into(),
            notice,
            origin_server_ts: None,
        };
        assert!(!shown(&other(OtherEvent::Sticker, true), &no_deleted));
        assert!(!shown(&other(OtherEvent::Message, true), &no_deleted));
        assert!(!shown(&notice(NoticeKind::Deleted), &no_deleted));
        assert!(shown(&notice(NoticeKind::Undecryptable), &no_deleted));
        assert!(shown(&other(OtherEvent::Sticker, false), &no_deleted));
        let no_stickers = only_off(|e| &mut e.stickers);
        assert!(!shown(&other(OtherEvent::Sticker, true), &no_stickers));
    }

    /// GIVEN room changes WHEN their switch is off THEN they go, by the same
    /// switches as the page (`frontend/src/stateText.ts`).
    #[test]
    fn given_room_changes_then_their_switch_hides_them() {
        let state = |change| TimelineItem::State {
            event_id: "$s".into(),
            sender: "@a:example.org".into(),
            origin_server_ts: None,
            change,
        };
        let member = |membership| StateChange::Member {
            target: "@b:example.org".into(),
            membership,
        };
        let cases: [(StateChange, Key); 8] = [
            (member(MembershipChange::Joined), |e| &mut e.joins),
            (member(MembershipChange::Left), |e| &mut e.leaves),
            (member(MembershipChange::InviteWithdrawn), |e| {
                &mut e.invitations
            }),
            (member(MembershipChange::Banned), |e| &mut e.removals),
            (member(MembershipChange::KnockDenied), |e| {
                &mut e.join_requests
            }),
            (StateChange::Avatar {}, |e| &mut e.room_picture),
            (StateChange::PowerLevels {}, |e| &mut e.permissions),
            (StateChange::ServerAcl {}, |e| &mut e.server_access),
        ];
        for (change, key) in cases {
            let item = state(change);
            assert!(shown(&item, &TimelineEvents::default()), "{item:?}");
            assert!(!shown(&item, &only_off(key)), "{item:?}");
        }
    }

    /// Switches set by name: on where `on` says so.
    fn switches(on: impl Fn(&str) -> bool) -> TimelineEvents {
        let mut value = serde_json::to_value(TimelineEvents::default()).unwrap();
        for (key, switch) in value.as_object_mut().unwrap() {
            *switch = serde_json::Value::Bool(on(key));
        }
        serde_json::from_value(value).unwrap()
    }

    /// GIVEN the cases in `timeline-switches.json`, which the page's test
    /// (`frontend/tests/event-text.test.mjs`) also reads, WHEN each switch
    /// that hides an item is off alone THEN the item is hidden, and with
    /// every other switch off it still shows. The two copies of the rules
    /// cannot drift apart on a listed case, and every switch has a case.
    #[test]
    fn given_the_shared_switch_cases_then_the_core_agrees_with_the_page() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Case {
            item: TimelineItem,
            hidden_by: Vec<String>,
        }
        let cases: Vec<Case> = serde_json::from_str(include_str!(
            "../../contracts/fixtures/timeline-switches.json"
        ))
        .unwrap();
        let keys: Vec<String> = serde_json::to_value(TimelineEvents::default())
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for case in &cases {
            let item = &case.item;
            assert!(shown(item, &TimelineEvents::default()), "{item:?}");
            for key in &case.hidden_by {
                assert!(keys.contains(key), "{key} is not a switch");
                assert!(!shown(item, &switches(|k| k != key)), "{key}: {item:?}");
            }
            let rest_off = switches(|k| case.hidden_by.iter().any(|h| h == k));
            assert!(shown(item, &rest_off), "another switch hides {item:?}");
        }
        for key in &keys {
            assert!(
                cases.iter().any(|case| case.hidden_by.contains(key)),
                "no case for {key}"
            );
        }
    }

    /// GIVEN every switch off THEN messages, encryption notices, gaps,
    /// relations and messages of an unknown type still pass.
    #[test]
    fn given_every_switch_off_then_messages_and_relations_still_pass() {
        let items = [
            TimelineItem::Message {
                event_id: "$m".into(),
                sender: "@a:example.org".into(),
                body: "hi".into(),
                origin_server_ts: None,
                reply_to: None,
            },
            TimelineItem::Notice {
                event_id: "$n".into(),
                sender: "@a:example.org".into(),
                notice: NoticeKind::Withheld,
                origin_server_ts: None,
            },
            TimelineItem::Gap { next_cursor: None },
            TimelineItem::Reaction {
                event_id: "$r".into(),
                sender: "@a:example.org".into(),
                origin_server_ts: None,
                reacts_to: "$m".into(),
                key: "+1".into(),
            },
            TimelineItem::Redaction {
                event_id: "$d".into(),
                sender: "@a:example.org".into(),
                origin_server_ts: None,
                redacts: "$m".into(),
            },
            other(OtherEvent::Message, false),
        ];
        for item in items {
            assert!(shown(&item, &all_off()), "{item:?}");
        }
    }
}
