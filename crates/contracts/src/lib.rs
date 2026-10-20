use serde::{Deserialize, Serialize};
use ts_rs::{Config, TS};

/// Deliberately small frontend projection; never expose SDK, authentication, or key types here.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomSummary {
    pub room_id: String,
    pub display_name: Option<String>,
    /// u32 caps this count below JavaScript's exact-integer limit.
    pub unread_count: u32,
}

/// Serde's internally tagged wire representation is mirrored by ts-rs serde compatibility.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimelineItem {
    Message {
        event_id: String,
        sender: String,
        body: String,
    },
    Gap {
        next_cursor: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimelinePage {
    pub room_id: String,
    pub items: Vec<TimelineItem>,
    pub next_cursor: Option<String>,
}

/// Reply of the `app_status` command, the shell's only command. It carries no
/// system, account or user data.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppStatus {
    /// The application version, from the desktop crate's manifest.
    pub version: String,
    /// The current development milestone, e.g. "P1".
    pub milestone: String,
}

pub fn sample_room_summary() -> RoomSummary {
    RoomSummary {
        room_id: "!example:server.invalid".into(),
        display_name: None,
        unread_count: u32::MAX,
    }
}

pub fn sample_timeline_page() -> TimelinePage {
    TimelinePage {
        room_id: "!example:server.invalid".into(),
        items: vec![
            TimelineItem::Message {
                event_id: "$event".into(),
                sender: "@alice:server.invalid".into(),
                body: "hostile <img src=x onerror=alert(1)>".into(),
            },
            TimelineItem::Gap { next_cursor: None },
        ],
        next_cursor: Some("cursor-2".into()),
    }
}

pub fn generated_typescript() -> String {
    let config = Config::default();
    let mut output = [
        format!("export {}", AppStatus::decl(&config)),
        format!("export {}", RoomSummary::decl(&config)),
        format!("export {}", TimelineItem::decl(&config)),
        format!("export {}", TimelinePage::decl(&config)),
    ]
    .join("\n\n")
        + "\n\n";
    output.push_str(&format!(
        "export const roomSummaryFixture = {} as const satisfies RoomSummary;\n\n",
        serde_json::to_string_pretty(&sample_room_summary()).expect("DTO serializes")
    ));
    output.push_str(&format!(
        "export const timelinePageFixture = {} as const satisfies TimelinePage;\n",
        serde_json::to_string_pretty(&sample_timeline_page()).expect("DTO serializes")
    ));
    output
}
