//! This account's settings, kept in its encrypted profile.
//!
//! The settings are one JSON record in the crypto store, like the plaintext
//! consent list, so each account has its own and signing out wipes them with
//! the rest of the profile. No setting is kept by the web view or outside the
//! profile.
//!
//! The stored record is read leniently: a setting it does not have takes its
//! default, and one this build does not know is ignored, so a record written
//! by an older or newer build still reads. A record that is too large or not
//! JSON reads as the defaults; the next save replaces it. A text size out of
//! its bounds reads as its default, so a damaged record cannot make the
//! interface unusable. A theme this build does not know reads as the
//! default theme and keeps the other settings.

use crate::store::Stores;
use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::{AccountSettings, PresenceStatus, Theme, TimelineEvents};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const SETTINGS_KEY: &str = "persephone.account_settings.v1";
const MAX_SETTINGS_BYTES: usize = 4096;

/// The stored form. Every field has a default, and unknown fields are
/// ignored; the wire form ([`AccountSettings`]) stays strict.
#[derive(Deserialize, Serialize)]
#[serde(default)]
struct Stored {
    enter_sends: bool,
    chat_text_percent: u16,
    interface_text_percent: u16,
    desktop_notifications: bool,
    send_typing: bool,
    send_read_receipts: bool,
    idle_lock_minutes: u16,
    notification_names: bool,
    load_room_pictures: bool,
    capture_protection: bool,
    /// Kept as a map of switch names, read by [`timeline_events`].
    timeline_events: Map<String, Value>,
    /// Kept as any value, read by [`theme`]: a theme name from another
    /// build must not make the whole record unreadable.
    theme: Value,
    share_presence: bool,
    /// Kept as any value, read by [`presence`], like the theme.
    presence: Value,
}

impl Default for Stored {
    fn default() -> Self {
        Self::from(AccountSettings::default())
    }
}

impl From<AccountSettings> for Stored {
    fn from(settings: AccountSettings) -> Self {
        Self {
            enter_sends: settings.enter_sends,
            chat_text_percent: settings.chat_text_percent,
            interface_text_percent: settings.interface_text_percent,
            desktop_notifications: settings.desktop_notifications,
            send_typing: settings.send_typing,
            send_read_receipts: settings.send_read_receipts,
            idle_lock_minutes: settings.idle_lock_minutes,
            notification_names: settings.notification_names,
            load_room_pictures: settings.load_room_pictures,
            capture_protection: settings.capture_protection,
            timeline_events: match serde_json::to_value(settings.timeline_events) {
                Ok(Value::Object(switches)) => switches,
                _ => Map::new(),
            },
            theme: serde_json::to_value(settings.theme).unwrap_or(Value::Null),
            share_presence: settings.share_presence,
            presence: serde_json::to_value(settings.presence).unwrap_or(Value::Null),
        }
    }
}

/// The theme from a stored value, leniently: a name this build does not
/// know, or a value that is not a name, reads as the default theme.
fn theme(stored: &Value) -> Theme {
    Theme::deserialize(stored).unwrap_or_default()
}

/// The shared presence from a stored value, leniently: anything but one of
/// the three names reads as online.
fn presence(stored: &Value) -> PresenceStatus {
    PresenceStatus::deserialize(stored).unwrap_or_default()
}

/// The timeline's switches from a stored map, leniently: a switch the map
/// does not have, or does not hold as true or false, takes its default
/// (shown), and a name this build does not know is ignored.
fn timeline_events(stored: &Map<String, Value>) -> TimelineEvents {
    let defaults = TimelineEvents::default();
    let Ok(Value::Object(mut switches)) = serde_json::to_value(defaults) else {
        return defaults;
    };
    for (name, value) in &mut switches {
        if let Some(Value::Bool(on)) = stored.get(name) {
            *value = Value::Bool(*on);
        }
    }
    serde_json::from_value(Value::Object(switches)).unwrap_or(defaults)
}

impl From<Stored> for AccountSettings {
    fn from(stored: Stored) -> Self {
        let defaults = AccountSettings::default();
        let mut settings = Self {
            enter_sends: stored.enter_sends,
            chat_text_percent: stored.chat_text_percent,
            interface_text_percent: stored.interface_text_percent,
            desktop_notifications: stored.desktop_notifications,
            send_typing: stored.send_typing,
            send_read_receipts: stored.send_read_receipts,
            idle_lock_minutes: stored.idle_lock_minutes,
            notification_names: stored.notification_names,
            load_room_pictures: stored.load_room_pictures,
            capture_protection: stored.capture_protection,
            timeline_events: timeline_events(&stored.timeline_events),
            theme: theme(&stored.theme),
            share_presence: stored.share_presence,
            presence: presence(&stored.presence),
        };
        if settings.idle_lock_minutes > persephone_contracts::IDLE_LOCK_MINUTES_MAX {
            settings.idle_lock_minutes = defaults.idle_lock_minutes;
        }
        if !(AccountSettings {
            interface_text_percent: defaults.interface_text_percent,
            ..settings
        })
        .within_bounds()
        {
            settings.chat_text_percent = defaults.chat_text_percent;
        }
        if !(AccountSettings {
            chat_text_percent: defaults.chat_text_percent,
            ..settings
        })
        .within_bounds()
        {
            settings.interface_text_percent = defaults.interface_text_percent;
        }
        settings
    }
}

fn decode(bytes: &[u8]) -> AccountSettings {
    if bytes.len() > MAX_SETTINGS_BYTES {
        return AccountSettings::default();
    }
    serde_json::from_slice::<Stored>(bytes)
        .unwrap_or_default()
        .into()
}

/// The settings, or `None` when the store cannot be read. A record never
/// written gives the defaults.
pub async fn load(stores: &Stores) -> Option<AccountSettings> {
    match stores.crypto().get_custom_value(SETTINGS_KEY).await {
        Ok(Some(bytes)) => Some(decode(&bytes)),
        Ok(None) => Some(AccountSettings::default()),
        Err(_) => None,
    }
}

/// Replaces the settings: `true` once they are written.
pub async fn save(stores: &Stores, settings: AccountSettings) -> bool {
    let Ok(bytes) = serde_json::to_vec(&Stored::from(settings)) else {
        return false;
    };
    stores
        .crypto()
        .set_custom_value(SETTINGS_KEY, bytes)
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_saved_record_then_it_reads_back() {
        let changed = AccountSettings {
            enter_sends: false,
            chat_text_percent: 140,
            interface_text_percent: 90,
            desktop_notifications: true,
            send_typing: true,
            send_read_receipts: true,
            idle_lock_minutes: 30,
            notification_names: false,
            load_room_pictures: true,
            capture_protection: false,
            timeline_events: TimelineEvents {
                leaves: false,
                room_picture: false,
                ..TimelineEvents::default()
            },
            theme: Theme::Blue,
            share_presence: false,
            presence: PresenceStatus::Unavailable,
        };
        let bytes = serde_json::to_vec(&Stored::from(changed)).unwrap();
        assert_eq!(decode(&bytes), changed);
    }

    /// GIVEN stored timeline switches that are partial, damaged, or from
    /// another build WHEN read THEN each known switch held as true or false
    /// keeps its value, and every other switch is on (shown).
    #[test]
    fn given_partial_or_damaged_timeline_switches_then_the_rest_are_shown() {
        let read = |record: &[u8]| decode(record).timeline_events;
        assert_eq!(read(b"{}"), TimelineEvents::default());
        assert_eq!(
            read(br#"{"timeline_events":{"leaves":false,"futureKind":false}}"#),
            TimelineEvents {
                leaves: false,
                ..TimelineEvents::default()
            }
        );
        assert_eq!(
            read(br#"{"timeline_events":{"joins":"no","topic":0,"leaves":null}}"#),
            TimelineEvents::default()
        );
        // Not a map at all: the whole record is damaged, so all defaults.
        assert_eq!(
            decode(br#"{"enter_sends":false,"timeline_events":[false]}"#),
            AccountSettings::default()
        );
    }

    /// GIVEN a stored theme WHEN read THEN a known theme keeps its value,
    /// and an unknown, damaged or missing one reads as the default theme
    /// while the other settings are kept.
    #[test]
    fn given_an_unknown_or_damaged_stored_theme_then_it_reads_as_the_default() {
        for (record, expected) in [
            (&br#"{"theme":"red"}"#[..], Theme::Red),
            (br#"{"theme":"blue"}"#, Theme::Blue),
            (br#"{"theme":"orange"}"#, Theme::Orange),
            (br#"{}"#, Theme::Orange),
        ] {
            assert_eq!(decode(record).theme, expected);
        }
        for value in [
            r#""green""#,
            r#""Red""#,
            "null",
            "7",
            r#"["red"]"#,
            r#"{"red":true}"#,
            r#"{"red":null}"#,
        ] {
            let record =
                format!(r#"{{"enter_sends":false,"chat_text_percent":125,"theme":{value}}}"#);
            assert_eq!(
                decode(record.as_bytes()),
                AccountSettings {
                    enter_sends: false,
                    chat_text_percent: 125,
                    ..AccountSettings::default()
                },
                "{record}"
            );
        }
    }

    /// GIVEN a record from before the presence settings, or one with a
    /// damaged presence WHEN read THEN presence is shared as online, and
    /// the other settings are kept (ADR-0057).
    #[test]
    fn given_an_old_or_damaged_presence_record_then_online_is_shared() {
        let old = decode(br#"{"enter_sends":false}"#);
        assert!(old.share_presence && !old.enter_sends);
        assert_eq!(old.presence, PresenceStatus::Online);
        let off = decode(br#"{"share_presence":false,"presence":"offline"}"#);
        assert!(!off.share_presence);
        assert_eq!(off.presence, PresenceStatus::Offline);
        for value in [r#""busy""#, "null", "1", r#"{"online":null}"#] {
            let record = format!(r#"{{"enter_sends":false,"presence":{value}}}"#);
            let read = decode(record.as_bytes());
            assert_eq!(read.presence, PresenceStatus::Online, "{record}");
            assert!(!read.enter_sends, "{record}");
        }
    }

    /// GIVEN a record saved by a build from before the other-event switches,
    /// with every room-change switch it knew WHEN read THEN its switches and
    /// other settings keep their values, and each newer switch is on.
    #[test]
    fn given_a_record_from_before_the_other_event_switches_then_it_reads_with_them_on() {
        let record = br#"{"enter_sends":false,"theme":"blue","timeline_events":{"joins":true,"leaves":false,"invitations":true,"removals":true,"joinRequests":true,"displayNames":true,"profilePictures":true,"roomName":true,"topic":true,"roomPicture":false,"encryption":true,"joinRule":true,"historyVisibility":true,"permissions":true,"addresses":true,"pinnedMessages":true,"guestAccess":true,"serverAccess":true,"creation":true,"upgrades":true}}"#;
        let read = decode(record);
        assert_eq!(
            read,
            AccountSettings {
                enter_sends: false,
                theme: Theme::Blue,
                timeline_events: TimelineEvents {
                    leaves: false,
                    room_picture: false,
                    ..TimelineEvents::default()
                },
                ..AccountSettings::default()
            }
        );
        let events = read.timeline_events;
        assert!(events.verification && events.calls && events.polls && events.other_events);
        assert!(events.deleted_messages && events.call_signalling);
        assert!(events.stickers && events.locations && events.spaces && events.widgets);
    }

    #[test]
    fn given_a_record_from_another_build_then_known_settings_read_and_others_default() {
        // An older build's record has only Enter to send.
        assert_eq!(
            decode(br#"{"enter_sends":false,"theme":"dark"}"#),
            AccountSettings {
                enter_sends: false,
                ..AccountSettings::default()
            }
        );
        assert_eq!(decode(b"{}"), AccountSettings::default());
        assert!(
            decode(br#"{"desktop_notifications":true}"#).notification_names,
            "a record from before the setting names the sender, as before"
        );
        assert!(
            decode(br#"{"notification_names":true,"load_room_pictures":true}"#).capture_protection,
            "a record from before the setting hides the window from capture"
        );
        assert!(
            !decode(br#"{"capture_protection":false}"#).capture_protection,
            "a stored off stays off"
        );
        assert!(
            !decode(br#"{"notification_names":true}"#).load_room_pictures,
            "a record from before the setting loads no room picture by itself"
        );
    }

    /// GIVEN a stored text size out of its bounds WHEN read THEN that size
    /// takes its default and the other settings are kept.
    #[test]
    fn given_an_out_of_bounds_text_size_then_it_reads_as_its_default() {
        let read = decode(
            br#"{"enter_sends":false,"chat_text_percent":5000,"interface_text_percent":120,"desktop_notifications":true}"#,
        );
        assert_eq!(read.chat_text_percent, 100);
        assert_eq!(read.interface_text_percent, 120);
        assert!(!read.enter_sends && read.desktop_notifications);
        assert_eq!(
            decode(br#"{"idle_lock_minutes":60000}"#).idle_lock_minutes,
            0,
            "an idle time over a day reads as off"
        );
        let read = decode(br#"{"chat_text_percent":150,"interface_text_percent":10}"#);
        assert_eq!(
            (read.chat_text_percent, read.interface_text_percent),
            (150, 100)
        );
    }

    #[test]
    fn given_a_damaged_or_oversized_record_then_the_defaults_are_used() {
        for bytes in [&b"not json"[..], br#"{"enter_sends":"no"}"#, br#"[1]"#] {
            assert_eq!(decode(bytes), AccountSettings::default());
        }
        let mut large = br#"{"enter_sends":false,"pad":""#.to_vec();
        large.resize(MAX_SETTINGS_BYTES + 1, b'x');
        large.extend_from_slice(br#""}"#);
        assert_eq!(decode(&large), AccountSettings::default());
    }
}
