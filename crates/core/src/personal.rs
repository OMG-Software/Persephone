//! Bounded drafts and room organisation in the encrypted profile.
//!
//! Each kind uses one crypto-store record, so clearing an entry also frees
//! its room slot. Missing records are empty. Invalid records and store errors
//! fail closed; a save never replaces a record it could not read.
//!
//! The caller must serialize mutations with the core's shared personal-data
//! gate. A read followed by a write is not an atomic store operation.

use crate::store::Stores;
use matrix_sdk::ruma::{EventId, RoomId};
use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::{RoomDraft, RoomPreference, RoomPreferences};
use persephone_core_protocol::ErrorCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;

const DRAFTS_KEY: &str = "persephone.room_drafts.v1";
const PREFERENCES_KEY: &str = "persephone.room_preferences.v1";
pub const MAX_PERSONAL_ROOMS: usize = 100;
pub const MAX_DRAFT_BYTES: usize = 16 * 1024;
pub const MAX_TOTAL_DRAFT_BYTES: usize = 100 * 1024;
pub const MAX_SECTION_BYTES: usize = 64;
pub const MAX_SECTIONS: usize = 16;
const MAX_RECORD_BYTES: usize = 112 * 1024;
const MAX_ID_BYTES: usize = 255;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StoredDraft {
    room_id: String,
    draft: RoomDraft,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Drafts {
    rooms: Vec<StoredDraft>,
}

fn valid_room_id(room_id: &str) -> bool {
    room_id.len() <= MAX_ID_BYTES && RoomId::parse(room_id).is_ok()
}

fn valid_draft(draft: &RoomDraft) -> bool {
    draft.body.len() <= MAX_DRAFT_BYTES
        && !draft
            .body
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        && draft
            .reply_to
            .as_ref()
            .is_none_or(|id| id.len() <= MAX_ID_BYTES && EventId::parse(id).is_ok())
}

fn valid_section(section: &str) -> bool {
    !section.is_empty()
        && section.len() <= MAX_SECTION_BYTES
        && section.trim() == section
        && !section.chars().any(char::is_control)
}

fn valid_drafts(drafts: &Drafts) -> bool {
    let mut rooms = BTreeSet::new();
    drafts.rooms.len() <= MAX_PERSONAL_ROOMS
        && drafts.rooms.iter().all(|entry| {
            valid_room_id(&entry.room_id)
                && valid_draft(&entry.draft)
                && entry.draft != RoomDraft::default()
                && rooms.insert(&entry.room_id)
        })
        && drafts
            .rooms
            .iter()
            .map(|entry| entry.draft.body.len())
            .sum::<usize>()
            <= MAX_TOTAL_DRAFT_BYTES
}

fn valid_preferences(preferences: &RoomPreferences) -> bool {
    let mut rooms = BTreeSet::new();
    let mut sections = BTreeSet::new();
    preferences.rooms.len() <= MAX_PERSONAL_ROOMS
        && preferences.rooms.iter().all(|entry| {
            valid_room_id(&entry.room_id)
                && (entry.favourite || entry.section.is_some())
                && entry.section.as_ref().is_none_or(|section| {
                    sections.insert(section);
                    valid_section(section)
                })
                && rooms.insert(&entry.room_id)
        })
        && sections.len() <= MAX_SECTIONS
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ErrorCode> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ErrorCode::Internal);
    }
    serde_json::from_slice(bytes).map_err(|_| ErrorCode::Internal)
}

async fn draft_record(stores: &Stores) -> Result<Drafts, ErrorCode> {
    let Some(bytes) = stores
        .crypto()
        .get_custom_value(DRAFTS_KEY)
        .await
        .map_err(|_| ErrorCode::Internal)?
    else {
        return Ok(Drafts::default());
    };
    let drafts = decode(&bytes)?;
    if !valid_drafts(&drafts) {
        return Err(ErrorCode::Internal);
    }
    Ok(drafts)
}

async fn write<T: Serialize>(stores: &Stores, key: &str, record: &T) -> Result<(), ErrorCode> {
    let bytes = serde_json::to_vec(record).map_err(|_| ErrorCode::Internal)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    stores
        .crypto()
        .set_custom_value(key, bytes)
        .await
        .map_err(|_| ErrorCode::Internal)
}

pub async fn load_draft(stores: &Stores, room_id: &str) -> Result<RoomDraft, ErrorCode> {
    if !valid_room_id(room_id) {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(draft_record(stores)
        .await?
        .rooms
        .into_iter()
        .find(|entry| entry.room_id == room_id)
        .map(|entry| entry.draft)
        .unwrap_or_default())
}

/// Saved draft identifiers, including rooms that this account has left.
/// This bounded list lets the user recover or discard orphaned local drafts.
pub async fn draft_rooms(stores: &Stores) -> Result<Vec<String>, ErrorCode> {
    Ok(draft_record(stores)
        .await?
        .rooms
        .into_iter()
        .map(|entry| entry.room_id)
        .collect())
}

/// Replaces or clears one draft. The caller must hold the mutation gate.
pub async fn save_draft(
    stores: &Stores,
    room_id: &str,
    draft: &RoomDraft,
) -> Result<(), ErrorCode> {
    if !valid_room_id(room_id) || !valid_draft(draft) {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut drafts = draft_record(stores).await?;
    drafts.rooms.retain(|entry| entry.room_id != room_id);
    if *draft != RoomDraft::default() {
        drafts.rooms.push(StoredDraft {
            room_id: room_id.into(),
            draft: draft.clone(),
        });
    }
    if !valid_drafts(&drafts) {
        return Err(ErrorCode::LimitExceeded);
    }
    write(stores, DRAFTS_KEY, &drafts).await
}

pub async fn preferences(stores: &Stores) -> Result<RoomPreferences, ErrorCode> {
    let Some(bytes) = stores
        .crypto()
        .get_custom_value(PREFERENCES_KEY)
        .await
        .map_err(|_| ErrorCode::Internal)?
    else {
        return Ok(RoomPreferences { rooms: Vec::new() });
    };
    let preferences = decode(&bytes)?;
    if !valid_preferences(&preferences) {
        return Err(ErrorCode::Internal);
    }
    Ok(preferences)
}

/// Replaces or clears one preference. The caller must hold the mutation gate.
pub async fn set_preference(stores: &Stores, preference: &RoomPreference) -> Result<(), ErrorCode> {
    if !valid_room_id(&preference.room_id)
        || preference
            .section
            .as_ref()
            .is_some_and(|section| section.chars().any(char::is_control))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let section = preference
        .section
        .as_ref()
        .map(|section| section.trim().to_owned());
    if section
        .as_ref()
        .is_some_and(|section| !valid_section(section))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut preferences = preferences(stores).await?;
    preferences
        .rooms
        .retain(|entry| entry.room_id != preference.room_id);
    if preference.favourite || section.is_some() {
        preferences.rooms.push(RoomPreference {
            room_id: preference.room_id.clone(),
            favourite: preference.favourite,
            section,
        });
    }
    if !valid_preferences(&preferences) {
        return Err(ErrorCode::LimitExceeded);
    }
    write(stores, PREFERENCES_KEY, &preferences).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        credentials::{CredentialSource, Credentials, PromptError, SecretPrompt},
        passphrase::{KdfParams, Unlock},
        store::{self, KEY_FILE, STORE_DIR},
        vault::{KeyProtector, VaultError},
    };
    use persephone_contracts::StoreState;
    use std::{fs, path::Path, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use zeroize::Zeroizing;

    // Reversible test fixture only. The real SDK store still uses encryption.
    struct Fixture;
    impl KeyProtector for Fixture {
        fn protect(&self, data: &[u8]) -> Result<Vec<u8>, VaultError> {
            Ok(data.iter().map(|b| b ^ 0xA5).collect())
        }
        fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
            Ok(Zeroizing::new(blob.iter().map(|b| b ^ 0xA5).collect()))
        }
    }
    impl CredentialSource for Fixture {
        fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
            Err(PromptError::Unsupported)
        }
        fn ask_secret(&self, _: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
            Ok(Zeroizing::new("correct horse battery".into()))
        }
    }
    fn open(path: &Path) -> Stores {
        store::open(
            path,
            &Fixture,
            &Unlock {
                source: Arc::new(Fixture),
                kdf: KdfParams::new(64, 1, 1).unwrap(),
                backoff: Duration::ZERO,
            },
        )
        .unwrap()
    }
    fn draft(body: &str) -> RoomDraft {
        RoomDraft {
            body: body.into(),
            reply_to: None,
        }
    }
    fn preference(room: usize, section: Option<&str>) -> RoomPreference {
        RoomPreference {
            room_id: format!("!room{room}:server.invalid"),
            favourite: true,
            section: section.map(str::to_owned),
        }
    }

    #[test]
    fn draft_policy_preserves_whitespace_and_bidi_but_rejects_bad_controls_ids_and_utf8_byte_excess()
     {
        for body in [
            "",
            " \n\t ",
            "مرحبا\u{202e}",
            &"é".repeat(MAX_DRAFT_BYTES / 2),
        ] {
            assert!(valid_draft(&draft(body)));
        }
        for body in [
            "carriage\rreturn",
            "null\0",
            &"é".repeat(MAX_DRAFT_BYTES / 2 + 1),
        ] {
            assert!(!valid_draft(&draft(body)));
        }
        assert!(!valid_draft(&RoomDraft {
            body: "text".into(),
            reply_to: Some("not-an-event".into())
        }));
        assert!(!valid_draft(&RoomDraft {
            body: "text".into(),
            reply_to: Some(format!("${}", "x".repeat(MAX_ID_BYTES)))
        }));
        assert!(!valid_room_id("not-a-room"));
    }

    #[test]
    fn persisted_drafts_and_preferences_reopen_and_stay_in_their_profile() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let stores = open(first.path());
        let saved = RoomDraft {
            body: "SYNTHETIC_DRAFT_PRIVATE_7C1D\n".into(),
            reply_to: Some("$reply".into()),
        };
        stores.block_on(async {
            save_draft(&stores, "!first:server.invalid", &saved)
                .await
                .unwrap();
            save_draft(&stores, "!second:server.invalid", &draft(" \n\t"))
                .await
                .unwrap();
            set_preference(&stores, &preference(1, Some(" Work ")))
                .await
                .unwrap();
        });
        assert!(stores.close());
        for entry in fs::read_dir(first.path().join(STORE_DIR)).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let marker = b"SYNTHETIC_DRAFT_PRIVATE_7C1D";
                assert!(
                    !fs::read(path)
                        .unwrap()
                        .windows(marker.len())
                        .any(|window| window == marker)
                );
            }
        }
        let stores = open(first.path());
        stores.block_on(async {
            assert_eq!(
                load_draft(&stores, "!first:server.invalid").await.unwrap(),
                saved
            );
            assert_eq!(
                load_draft(&stores, "!second:server.invalid").await.unwrap(),
                draft(" \n\t")
            );
            assert_eq!(
                preferences(&stores).await.unwrap().rooms[0]
                    .section
                    .as_deref(),
                Some("Work")
            );
        });
        assert!(stores.close());
        let other = open(second.path());
        other.block_on(async {
            assert_eq!(
                load_draft(&other, "!first:server.invalid").await.unwrap(),
                RoomDraft::default()
            );
            assert!(preferences(&other).await.unwrap().rooms.is_empty());
        });
        assert!(other.close());
    }

    #[test]
    fn corrupt_records_are_refused_and_not_replaced_by_mutations() {
        let dir = TempDir::new().unwrap();
        let stores = open(dir.path());
        stores.block_on(async {
            for key in [DRAFTS_KEY, PREFERENCES_KEY] {
                for bytes in [
                    b"not json".to_vec(),
                    br#"{"rooms":[],"extra":true}"#.to_vec(),
                    vec![b' '; MAX_RECORD_BYTES + 1],
                ] {
                    stores
                        .crypto()
                        .set_custom_value(key, bytes.clone())
                        .await
                        .unwrap();
                    if key == DRAFTS_KEY {
                        assert_eq!(
                            load_draft(&stores, "!room:server.invalid").await,
                            Err(ErrorCode::Internal)
                        );
                        assert_eq!(
                            save_draft(&stores, "!room:server.invalid", &draft("text")).await,
                            Err(ErrorCode::Internal)
                        );
                    } else {
                        assert_eq!(preferences(&stores).await, Err(ErrorCode::Internal));
                        assert_eq!(
                            set_preference(&stores, &preference(1, None)).await,
                            Err(ErrorCode::Internal)
                        );
                    }
                    assert_eq!(
                        stores.crypto().get_custom_value(key).await.unwrap(),
                        Some(bytes)
                    );
                }
            }
        });
        assert!(stores.close());
    }

    #[test]
    fn room_and_section_limits_do_not_lose_data_and_clear_frees_slots() {
        let dir = TempDir::new().unwrap();
        let stores = open(dir.path());
        stores.block_on(async {
            for room in 0..MAX_PERSONAL_ROOMS {
                save_draft(&stores, &preference(room, None).room_id, &draft("text"))
                    .await
                    .unwrap();
                set_preference(&stores, &preference(room, None))
                    .await
                    .unwrap();
            }
            let extra = preference(MAX_PERSONAL_ROOMS, None);
            assert_eq!(
                save_draft(&stores, &extra.room_id, &draft("text")).await,
                Err(ErrorCode::LimitExceeded)
            );
            assert_eq!(
                set_preference(&stores, &extra).await,
                Err(ErrorCode::LimitExceeded)
            );
            save_draft(&stores, &preference(0, None).room_id, &RoomDraft::default())
                .await
                .unwrap();
            set_preference(
                &stores,
                &RoomPreference {
                    favourite: false,
                    ..preference(0, None)
                },
            )
            .await
            .unwrap();
            save_draft(&stores, &extra.room_id, &draft("new"))
                .await
                .unwrap();
            set_preference(&stores, &extra).await.unwrap();
            assert_eq!(
                load_draft(&stores, &preference(0, None).room_id)
                    .await
                    .unwrap(),
                RoomDraft::default()
            );
            for section in 0..MAX_SECTIONS {
                set_preference(
                    &stores,
                    &preference(section + 1, Some(&format!("Section {section}"))),
                )
                .await
                .unwrap();
            }
            assert_eq!(
                set_preference(&stores, &preference(99, Some("Extra section"))).await,
                Err(ErrorCode::LimitExceeded)
            );
            assert_eq!(
                preferences(&stores).await.unwrap().rooms.len(),
                MAX_PERSONAL_ROOMS
            );
        });
        assert!(stores.close());
    }

    #[test]
    fn aggregate_and_serialized_limits_and_malformed_entries_are_rejected() {
        let mut drafts = Drafts::default();
        for room in 0..6 {
            drafts.rooms.push(StoredDraft {
                room_id: preference(room, None).room_id,
                draft: draft(&"x".repeat(MAX_DRAFT_BYTES)),
            });
        }
        assert!(valid_drafts(&drafts));
        drafts.rooms.push(StoredDraft {
            room_id: preference(6, None).room_id,
            draft: draft(&"x".repeat(4 * 1024)),
        });
        assert!(valid_drafts(&drafts));
        drafts.rooms[6].draft.body.push('x');
        assert!(!valid_drafts(&drafts));
        let duplicate = RoomPreferences {
            rooms: vec![preference(1, None), preference(1, None)],
        };
        assert!(!valid_preferences(&duplicate));
        for section in [
            "",
            " ",
            " Work",
            "Work\n",
            &"é".repeat(MAX_SECTION_BYTES / 2 + 1),
        ] {
            assert!(!valid_section(section));
        }
        assert!(valid_section(&"é".repeat(MAX_SECTION_BYTES / 2)));
        assert!(decode::<Drafts>(&vec![b' '; MAX_RECORD_BYTES + 1]).is_err());
    }

    #[test]
    fn rejected_inputs_and_aggregate_or_escaped_byte_excess_leave_saved_data_unchanged() {
        let dir = TempDir::new().unwrap();
        let stores = open(dir.path());
        stores.block_on(async {
            for room in 0..6 {
                save_draft(
                    &stores,
                    &preference(room, None).room_id,
                    &draft(&"x".repeat(MAX_DRAFT_BYTES)),
                )
                .await
                .unwrap();
            }
            let last = preference(6, None).room_id;
            save_draft(&stores, &last, &draft(&"x".repeat(4 * 1024)))
                .await
                .unwrap();
            let before = stores.crypto().get_custom_value(DRAFTS_KEY).await.unwrap();
            assert_eq!(
                save_draft(&stores, &last, &draft(&"x".repeat(4 * 1024 + 1))).await,
                Err(ErrorCode::LimitExceeded)
            );
            assert_eq!(
                stores.crypto().get_custom_value(DRAFTS_KEY).await.unwrap(),
                before
            );
            // The body budget fits, but escaped JSON exceeds the record budget.
            for room in 0..6 {
                save_draft(
                    &stores,
                    &preference(room, None).room_id,
                    &RoomDraft::default(),
                )
                .await
                .unwrap();
            }
            for room in 0..3 {
                save_draft(
                    &stores,
                    &preference(room, None).room_id,
                    &draft(&"\t".repeat(MAX_DRAFT_BYTES)),
                )
                .await
                .unwrap();
            }
            let escaped_before = stores.crypto().get_custom_value(DRAFTS_KEY).await.unwrap();
            assert_eq!(
                save_draft(
                    &stores,
                    &preference(3, None).room_id,
                    &draft(&"\t".repeat(MAX_DRAFT_BYTES))
                )
                .await,
                Err(ErrorCode::LimitExceeded)
            );
            assert_eq!(
                stores.crypto().get_custom_value(DRAFTS_KEY).await.unwrap(),
                escaped_before
            );
            for (room, invalid) in [
                ("not-a-room", draft("text")),
                ("!room:server.invalid", draft("bad\0")),
                (
                    "!room:server.invalid",
                    draft(&"x".repeat(MAX_DRAFT_BYTES + 1)),
                ),
            ] {
                assert_eq!(
                    save_draft(&stores, room, &invalid).await,
                    Err(ErrorCode::InvalidArgument)
                );
            }
            assert_eq!(
                stores.crypto().get_custom_value(DRAFTS_KEY).await.unwrap(),
                escaped_before
            );
            set_preference(&stores, &preference(1, Some("Work")))
                .await
                .unwrap();
            let before = stores
                .crypto()
                .get_custom_value(PREFERENCES_KEY)
                .await
                .unwrap();
            for section in ["", " ", "Work\n", &"é".repeat(MAX_SECTION_BYTES / 2 + 1)] {
                assert_eq!(
                    set_preference(&stores, &preference(1, Some(section))).await,
                    Err(ErrorCode::InvalidArgument)
                );
            }
            assert_eq!(
                stores
                    .crypto()
                    .get_custom_value(PREFERENCES_KEY)
                    .await
                    .unwrap(),
                before
            );
        });
        assert!(stores.close());
    }

    #[test]
    fn another_profiles_key_cannot_open_saved_personal_data() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let stores = open(first.path());
        stores
            .block_on(save_draft(
                &stores,
                "!room:server.invalid",
                &draft("private"),
            ))
            .unwrap();
        assert!(stores.close());
        assert!(open(second.path()).close());
        let key_path = first.path().join(KEY_FILE);
        let original = fs::read(&key_path).unwrap();
        fs::copy(second.path().join(KEY_FILE), &key_path).unwrap();
        let unlock = Unlock {
            source: Arc::new(Fixture),
            kdf: KdfParams::new(64, 1, 1).unwrap(),
            backoff: Duration::ZERO,
        };
        match store::open(first.path(), &Fixture, &unlock) {
            Err(state) => assert_eq!(state, StoreState::KeyRejected),
            Ok(stores) => {
                stores.close();
                panic!("wrong key opened the profile");
            }
        }
        fs::write(key_path, original).unwrap();
        let stores = open(first.path());
        assert_eq!(
            stores
                .block_on(load_draft(&stores, "!room:server.invalid"))
                .unwrap(),
            draft("private")
        );
        assert!(stores.close());
    }
}
