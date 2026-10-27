//! This account's display name and the devices (sessions) signed in to it.
//!
//! * The display name is the homeserver's word, projected like any name when
//!   it is shown. Setting one sends the page's text after validation; an
//!   empty name removes it.
//! * The device list is the homeserver's `/devices` answer. The device ID and
//!   name are projected like names. The last-seen IP address never crosses to
//!   the UI. Whether a device is verified comes from this device's store: the
//!   account's own cross-signing identity signed it.
//! * A failure to reach the homeserver is `unavailable`; an answer refusing
//!   the request is `rejected`. Nothing is retried.

use crate::views::project_name;
use matrix_sdk::Client;
use persephone_contracts::{DeviceList, DeviceSummary, DisplayName, UnixMillis};
use persephone_core_protocol::{ErrorCode, MAX_DISPLAY_NAME_BYTES};

/// Most devices a `device_list` reply holds.
pub const MAX_DEVICES: usize = 100;

/// A display name the page sent: within the limit and free of control
/// characters. Empty means none.
pub fn valid_display_name(name: &str) -> bool {
    name.len() <= MAX_DISPLAY_NAME_BYTES && !name.chars().any(char::is_control)
}

/// This account's display name.
pub async fn display_name(client: &Client) -> Result<DisplayName, ErrorCode> {
    let name = client
        .account()
        .get_display_name()
        .await
        .map_err(|error| request_error(&error))?;
    Ok(DisplayName {
        display_name: name.as_deref().map(project_name),
    })
}

/// Sets this account's display name; a blank `name` removes it. Replies with
/// the name now set.
pub async fn set_display_name(client: &Client, name: &str) -> Result<DisplayName, ErrorCode> {
    let name = name.trim();
    let name = (!name.is_empty()).then_some(name);
    client
        .account()
        .set_display_name(name)
        .await
        .map_err(|error| request_error(&error))?;
    Ok(DisplayName {
        display_name: name.map(project_name),
    })
}

/// The devices signed in to this account: this device first, then the most
/// recently seen, at most [`MAX_DEVICES`].
pub async fn device_list(client: &Client) -> Result<DeviceList, ErrorCode> {
    let response = client.devices().await.map_err(|error| {
        if error.as_client_api_error().is_some() {
            ErrorCode::Rejected
        } else {
            ErrorCode::Unavailable
        }
    })?;
    let own_user = client.user_id().ok_or(ErrorCode::NotFound)?.to_owned();
    let own_device = client.device_id().map(ToOwned::to_owned);
    let encryption = client.encryption();
    let mut devices = Vec::new();
    for device in response.devices.iter() {
        let verified = matches!(
            encryption.get_device(&own_user, &device.device_id).await,
            Ok(Some(known)) if known.is_cross_signed_by_owner()
        );
        devices.push(DeviceSummary {
            device_id: project_name(device.device_id.as_str()),
            display_name: device.display_name.as_deref().map(project_name),
            last_seen: device
                .last_seen_ts
                .map(|seen| UnixMillis::saturating(seen.get().into())),
            current: own_device.as_deref() == Some(&*device.device_id),
            verified,
        });
    }
    order(&mut devices);
    let truncated = devices.len() > MAX_DEVICES;
    devices.truncate(MAX_DEVICES);
    Ok(DeviceList { devices, truncated })
}

/// This device first, then the most recently seen; unseen devices last, in
/// ID order.
fn order(devices: &mut [DeviceSummary]) {
    devices.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(b.last_seen.cmp(&a.last_seen))
            .then(a.device_id.cmp(&b.device_id))
    });
}

/// A refusal from the homeserver is `rejected`; anything else, such as no
/// answer at all, is `unavailable`.
fn request_error(error: &matrix_sdk::Error) -> ErrorCode {
    if error.client_api_error_kind().is_some() {
        ErrorCode::Rejected
    } else {
        ErrorCode::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, seen: Option<u64>, current: bool) -> DeviceSummary {
        DeviceSummary {
            device_id: id.into(),
            display_name: None,
            last_seen: seen.and_then(UnixMillis::new),
            current,
            verified: false,
        }
    }

    /// GIVEN display names from the page WHEN checked THEN empty and
    /// bounded text passes, and long or control-bearing text is refused.
    #[test]
    fn given_display_names_then_long_or_control_names_are_refused() {
        assert!(valid_display_name(""));
        assert!(valid_display_name("Alice"));
        assert!(valid_display_name(&"a".repeat(MAX_DISPLAY_NAME_BYTES)));
        for invalid in [
            "a".repeat(MAX_DISPLAY_NAME_BYTES + 1),
            "Al\nice".into(),
            "Al\u{7}ice".into(),
        ] {
            assert!(!valid_display_name(&invalid), "{invalid:?}");
        }
    }

    /// GIVEN devices seen at different times WHEN ordered THEN this device
    /// comes first, then the most recently seen, then the unseen by ID.
    #[test]
    fn given_devices_then_this_one_comes_first_then_the_most_recent() {
        let mut devices = vec![
            device("B", None, false),
            device("OLD", Some(1_000), false),
            device("A", None, false),
            device("ME", Some(500), true),
            device("NEW", Some(9_000), false),
        ];
        order(&mut devices);
        let ids: Vec<&str> = devices.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(ids, ["ME", "NEW", "OLD", "A", "B"]);
    }
}
