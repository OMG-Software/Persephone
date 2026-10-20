use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use matrix_sdk::{
    Client, Room, deserialized_responses::EncryptionInfo, event_handler::EventHandlerDropGuard,
    ruma::events::room::message::OriginalSyncRoomMessageEvent,
};

const MAX_RECORDS: usize = 32;
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_BODY_BYTES: usize = 256;

#[derive(Debug)]
struct MessageRecord {
    event_id: String,
    sender: String,
    room_id: String,
    body: String,
    decrypted: bool,
}

#[derive(Debug, Default)]
struct RecorderState {
    records: Vec<MessageRecord>,
    failed: bool,
}

/// Records a bounded set of decrypted room messages observed during sync.
pub struct SyncReceipts {
    state: Arc<Mutex<RecorderState>>,
    // Removing this guard unregisters the sync handler when the recorder drops.
    _guard: EventHandlerDropGuard,
}

impl SyncReceipts {
    /// Start observing decrypted and plaintext room messages delivered by sync.
    pub fn new(client: &Client) -> Self {
        let state = Arc::new(Mutex::new(RecorderState::default()));
        let handler_state = Arc::clone(&state);
        let handle = client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent,
                  room: Room,
                  encryption_info: Option<EncryptionInfo>| {
                let _ = record(
                    &handler_state,
                    event.event_id.as_str(),
                    event.sender.as_str(),
                    room.room_id().as_str(),
                    event.content.body(),
                    encryption_info.is_some(),
                );
                std::future::ready(())
            },
        );

        Self {
            state,
            _guard: client.event_handler_drop_guard(handle),
        }
    }

    /// Return whether sync delivered this exact event as an SDK-decrypted message.
    pub fn has_exact(
        &self,
        event_id: &str,
        sender: &str,
        room_id: &str,
        text: &str,
    ) -> Result<bool> {
        validate_sizes(event_id, sender, room_id, text)?;
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("sync receipt recorder lock is poisoned"))?;

        if state.failed {
            bail!("sync receipt recorder exceeded a configured bound");
        }

        Ok(matches_exact(&state, event_id, sender, room_id, text))
    }
}

fn validate_sizes(event_id: &str, sender: &str, room_id: &str, body: &str) -> Result<()> {
    if event_id.len() > MAX_IDENTIFIER_BYTES
        || sender.len() > MAX_IDENTIFIER_BYTES
        || room_id.len() > MAX_IDENTIFIER_BYTES
    {
        bail!("sync receipt identifier exceeds {MAX_IDENTIFIER_BYTES} bytes");
    }
    if body.len() > MAX_BODY_BYTES {
        bail!("sync receipt body exceeds {MAX_BODY_BYTES} bytes");
    }
    Ok(())
}

fn record(
    state: &Mutex<RecorderState>,
    event_id: &str,
    sender: &str,
    room_id: &str,
    body: &str,
    decrypted: bool,
) -> Result<()> {
    if let Err(error) = validate_sizes(event_id, sender, room_id, body) {
        if let Ok(mut state) = state.lock() {
            state.failed = true;
        }
        return Err(error);
    }

    let mut state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("sync receipt recorder lock is poisoned"))?;

    if state.failed {
        bail!("sync receipt recorder exceeded a configured bound");
    }

    // Event IDs are unique within a room. Keep the first observed copy so
    // repeated sync snapshots cannot consume additional slots or rewrite it.
    if state
        .records
        .iter()
        .any(|record| record.event_id == event_id && record.room_id == room_id)
    {
        return Ok(());
    }

    if state.records.len() >= MAX_RECORDS {
        state.failed = true;
        bail!("sync receipt recorder capacity exceeded");
    }

    state.records.push(MessageRecord {
        event_id: event_id.to_owned(),
        sender: sender.to_owned(),
        room_id: room_id.to_owned(),
        body: body.to_owned(),
        decrypted,
    });
    Ok(())
}

fn matches_exact(
    state: &RecorderState,
    event_id: &str,
    sender: &str,
    room_id: &str,
    body: &str,
) -> bool {
    state.records.iter().any(|record| {
        record.decrypted
            && record.event_id == event_id
            && record.sender == sender
            && record.room_id == room_id
            && record.body == body
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_ID: &str = "$event:example.org";
    const SENDER: &str = "@alice:example.org";
    const ROOM_ID: &str = "!room:example.org";
    const BODY: &str = "received over native sync";

    fn state_with_message(decrypted: bool) -> Mutex<RecorderState> {
        let state = Mutex::new(RecorderState::default());
        record(&state, EVENT_ID, SENDER, ROOM_ID, BODY, decrypted).unwrap();
        state
    }

    fn matches(
        state: &Mutex<RecorderState>,
        event_id: &str,
        sender: &str,
        room_id: &str,
        body: &str,
    ) -> bool {
        let state = state.lock().unwrap();
        matches_exact(&state, event_id, sender, room_id, body)
    }

    #[test]
    fn plaintext_callback_cannot_satisfy_a_receipt() {
        let state = state_with_message(false);
        assert!(!matches(&state, EVENT_ID, SENDER, ROOM_ID, BODY));
    }

    #[test]
    fn every_receipt_field_must_match_exactly() {
        let state = state_with_message(true);
        assert!(matches(&state, EVENT_ID, SENDER, ROOM_ID, BODY));
        assert!(!matches(
            &state,
            "$other:example.org",
            SENDER,
            ROOM_ID,
            BODY
        ));
        assert!(!matches(
            &state,
            EVENT_ID,
            "@bob:example.org",
            ROOM_ID,
            BODY
        ));
        assert!(!matches(
            &state,
            EVENT_ID,
            SENDER,
            "!other:example.org",
            BODY
        ));
        assert!(!matches(
            &state,
            EVENT_ID,
            SENDER,
            ROOM_ID,
            "different text"
        ));
    }

    #[test]
    fn repeated_event_in_same_room_is_deduplicated() {
        let state = Mutex::new(RecorderState::default());
        record(&state, EVENT_ID, SENDER, ROOM_ID, BODY, true).unwrap();
        record(
            &state,
            EVENT_ID,
            SENDER,
            ROOM_ID,
            "snapshot duplicate",
            true,
        )
        .unwrap();

        let state = state.lock().unwrap();
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].body, BODY);
    }

    #[test]
    fn capacity_and_oversized_fields_fail_closed() {
        let state = Mutex::new(RecorderState::default());
        for index in 0..MAX_RECORDS {
            let event_id = format!("$event-{index}:example.org");
            record(&state, &event_id, SENDER, ROOM_ID, BODY, true).unwrap();
        }
        assert!(record(&state, "$overflow:example.org", SENDER, ROOM_ID, BODY, true).is_err());
        assert!(state.lock().unwrap().failed);

        let oversized_state = Mutex::new(RecorderState::default());
        let oversized_body = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(
            record(
                &oversized_state,
                EVENT_ID,
                SENDER,
                ROOM_ID,
                &oversized_body,
                true
            )
            .is_err()
        );
        assert!(oversized_state.lock().unwrap().failed);
    }
}
