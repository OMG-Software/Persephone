//! When this account is notified, and the desktop notifications it shows
//! (ADR-0022).
//!
//! * **The level** is the account's standard push rules on the homeserver,
//!   so every client and push gateway follows it. "Nothing" turns on the
//!   master rule (`.m.rule.master`). "All" and "mentions" turn it off and set
//!   the four default room rules (one-to-one or group, encrypted or not) to
//!   notify for every message, or not at all, leaving the mention and
//!   keyword rules as they are. Rules for single rooms are left alone. A
//!   mix of default rules another client set reads as `custom`.
//! * **A desktop notification** is offered for an event the SDK's push-rule
//!   evaluation says notifies, during sync. Only the room's name and the
//!   sender's name leave this module, projected like any name; never the
//!   message text or its type. The account's own events and events older
//!   than [`RECENT`] (the backlog a first sync after unlocking brings) are
//!   skipped. Whether notifications are on is the session's to check.

use crate::{contacts, views::project_name};
use matrix_sdk::{
    Client, Room,
    deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    notification_settings::{IsEncrypted, IsOneToOne, RoomNotificationMode},
    ruma::{
        MilliSecondsSinceUnixEpoch, OwnedUserId,
        push::{PredefinedOverrideRuleId, RuleKind},
    },
    sync::Notification,
};
use persephone_contracts::NotificationLevel;
use persephone_core_protocol::{ErrorCode, MAX_NOTIFICATION_NAME_BYTES};
use std::time::Duration;

/// Events older than this are not shown as desktop notifications.
pub const RECENT: Duration = Duration::from_secs(120);

/// The four kinds of room the default rules cover.
const KINDS: [(IsEncrypted, IsOneToOne); 4] = [
    (IsEncrypted::Yes, IsOneToOne::Yes),
    (IsEncrypted::Yes, IsOneToOne::No),
    (IsEncrypted::No, IsOneToOne::Yes),
    (IsEncrypted::No, IsOneToOne::No),
];

/// The level the master switch and the four default modes give.
pub fn level_of(master: bool, modes: &[RoomNotificationMode]) -> NotificationLevel {
    if master {
        NotificationLevel::None
    } else if modes
        .iter()
        .all(|mode| *mode == RoomNotificationMode::AllMessages)
    {
        NotificationLevel::All
    } else if modes
        .iter()
        .all(|mode| *mode == RoomNotificationMode::MentionsAndKeywordsOnly)
    {
        NotificationLevel::Mentions
    } else {
        NotificationLevel::Custom
    }
}

/// This account's level, from the push rules the client last synced.
pub async fn level(client: &Client) -> Result<NotificationLevel, ErrorCode> {
    let settings = client.notification_settings().await;
    let master = settings
        .is_push_rule_enabled(RuleKind::Override, PredefinedOverrideRuleId::Master)
        .await
        // A rule set without the master rule cannot be switched off by it.
        .unwrap_or(false);
    let mut modes = Vec::with_capacity(KINDS.len());
    for (encrypted, one_to_one) in KINDS {
        modes.push(
            settings
                .get_default_room_notification_mode(encrypted, one_to_one)
                .await,
        );
    }
    Ok(level_of(master, &modes))
}

/// Sets this account's level; `custom` is `invalid_argument`. The SDK does
/// not say whether the homeserver refused or did not answer: `unavailable`.
pub async fn set_level(client: &Client, level: NotificationLevel) -> Result<(), ErrorCode> {
    let mode = match level {
        NotificationLevel::All => RoomNotificationMode::AllMessages,
        NotificationLevel::Mentions => RoomNotificationMode::MentionsAndKeywordsOnly,
        NotificationLevel::None => RoomNotificationMode::Mute,
        NotificationLevel::Custom => return Err(ErrorCode::InvalidArgument),
    };
    let settings = client.notification_settings().await;
    let master = level == NotificationLevel::None;
    settings
        .set_push_rule_enabled(RuleKind::Override, PredefinedOverrideRuleId::Master, master)
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    if master {
        // The defaults stay as they were, so turning notifications back on
        // restores them.
        return Ok(());
    }
    for (encrypted, one_to_one) in KINDS {
        settings
            .set_default_room_notification_mode(encrypted, one_to_one, mode)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
    }
    Ok(())
}

/// Cuts a projected name to the event's limit on a character boundary.
fn bounded(mut name: String) -> String {
    if name.len() > MAX_NOTIFICATION_NAME_BYTES {
        let mut end = MAX_NOTIFICATION_NAME_BYTES;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    name
}

/// Whether an event sent at `sent` is recent at `now`: not older than
/// [`RECENT`]. A time in the future counts as recent.
pub fn recent(sent: MilliSecondsSinceUnixEpoch, now: MilliSecondsSinceUnixEpoch) -> bool {
    let (sent, now) = (u64::from(sent.get()), u64::from(now.get()));
    now.saturating_sub(sent) <= RECENT.as_millis() as u64
}

/// The room and sender names to show for `notification`, or `None` when it
/// is not shown: the account's own event, an invitation, an old event, or
/// one without a sender.
pub async fn shown(
    notification: &Notification,
    room: &Room,
    client: &Client,
) -> Option<(String, String)> {
    let RawAnySyncOrStrippedTimelineEvent::Sync(event) = &notification.event else {
        return None;
    };
    let sender: OwnedUserId = event.get_field("sender").ok()??;
    if client.user_id() == Some(&*sender) {
        return None;
    }
    let sent: MilliSecondsSinceUnixEpoch = event.get_field("origin_server_ts").ok()??;
    if !recent(sent, MilliSecondsSinceUnixEpoch::now()) {
        return None;
    }
    let contact = contacts::contact_of(client, room);
    let room_name = match (room.name(), &contact) {
        (None, Some(contact)) => contacts::shown(contact),
        _ => room
            .cached_display_name()
            .map(|name| project_name(&name.to_string()))
            .unwrap_or_else(|| "A room".to_owned()),
    };
    let sender_name = match room.get_member_no_sync(&sender).await {
        Ok(Some(member)) => member
            .display_name()
            .map(project_name)
            .unwrap_or_else(|| contacts::shown(&sender)),
        _ => contacts::shown(&sender),
    };
    Some((bounded(room_name), bounded(sender_name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::UInt;

    /// GIVEN the master switch and the default modes WHEN read THEN the
    /// switch wins, matching modes give their level, and a mix is custom.
    #[test]
    fn given_push_rules_then_the_level_follows_them() {
        use RoomNotificationMode::{AllMessages, MentionsAndKeywordsOnly, Mute};
        assert_eq!(level_of(true, &[AllMessages; 4]), NotificationLevel::None);
        assert_eq!(level_of(false, &[AllMessages; 4]), NotificationLevel::All);
        assert_eq!(
            level_of(false, &[MentionsAndKeywordsOnly; 4]),
            NotificationLevel::Mentions
        );
        // Another client's usual default: all in one-to-one rooms, mentions
        // in groups.
        assert_eq!(
            level_of(
                false,
                &[
                    AllMessages,
                    MentionsAndKeywordsOnly,
                    AllMessages,
                    MentionsAndKeywordsOnly
                ]
            ),
            NotificationLevel::Custom
        );
        assert_eq!(level_of(false, &[Mute; 4]), NotificationLevel::Custom);
    }

    /// GIVEN event times WHEN checked THEN only events at most two minutes
    /// old are recent; a clock ahead of this one still counts.
    #[test]
    fn given_event_times_then_only_recent_ones_notify() {
        let at = |millis: u32| MilliSecondsSinceUnixEpoch(UInt::from(millis));
        let now = at(1_000_000);
        assert!(recent(at(1_000_000), now));
        assert!(recent(at(1_000_000 - 120_000), now));
        assert!(!recent(at(1_000_000 - 120_001), now));
        assert!(recent(at(2_000_000), now));
    }

    /// GIVEN a long name WHEN bounded THEN it is cut on a character
    /// boundary within the limit.
    #[test]
    fn given_a_long_name_then_it_is_cut_on_a_character_boundary() {
        let name = "é".repeat(MAX_NOTIFICATION_NAME_BYTES);
        let cut = bounded(name);
        assert!(cut.len() <= MAX_NOTIFICATION_NAME_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(bounded("Plans".into()), "Plans");
    }
}
