//! The call's audio devices (ADR-0043): the user's saved choice and the list
//! of devices the page offers.
//!
//! * The choice belongs to the profile. It is one JSON record in the encrypted
//!   crypto store, like the proxy. It holds platform device IDs only; `None`
//!   is the system default. A device need not be present when it is saved.
//! * A record that cannot be read fails the join. A call never falls back to
//!   the defaults on its own.
//! * The list comes from a contained call worker, because only it runs the
//!   audio stack. Device names are untrusted operating system text: they are
//!   normalized like display names and bounded before the page sees them.

use super::worker::{CallHost, StartError};
use crate::{store::Stores, views::project_name};
use matrix_sdk_crypto::store::CryptoStore;
use persephone_call_protocol::{
    AudioDevice, FromWorker, MAX_DEVICE_ID_BYTES, MAX_DEVICE_NAME_BYTES, MAX_DEVICES, ToWorker,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const DEVICES_KEY: &str = "persephone.call_devices.v1";
const MAX_RECORD_BYTES: usize = 2048;
/// How long the worker has to answer `ListDevices`.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the worker has to exit after the list.
const FINISH_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the wait looks for the answer.
const POLL: Duration = Duration::from_millis(20);

/// The saved microphone and speaker. `None` is the system default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub recording: Option<String>,
    pub playout: Option<String>,
}

/// Whether `id` can be a device ID: not empty, bounded, no control character.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_DEVICE_ID_BYTES && !id.chars().any(char::is_control)
}

impl Selection {
    /// Whether every set ID is valid.
    pub fn is_valid(&self) -> bool {
        [&self.recording, &self.playout]
            .into_iter()
            .flatten()
            .all(|id| valid_id(id))
    }
}

/// The saved record cannot be read: a call must not start.
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable;

/// The profile's device choice; no record means both defaults.
pub async fn load(stores: &Stores) -> Result<Selection, Unreadable> {
    match stores.crypto().get_custom_value(DEVICES_KEY).await {
        Ok(None) => Ok(Selection::default()),
        Ok(Some(bytes)) => parse(&bytes),
        Err(_) => Err(Unreadable),
    }
}

fn parse(bytes: &[u8]) -> Result<Selection, Unreadable> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(Unreadable);
    }
    let selection: Selection = serde_json::from_slice(bytes).map_err(|_| Unreadable)?;
    // A record from elsewhere is checked like typed input.
    selection.is_valid().then_some(selection).ok_or(Unreadable)
}

/// Replaces the profile's device choice. `false` when it is invalid or not
/// stored.
pub async fn save(stores: &Stores, selection: &Selection) -> bool {
    if !selection.is_valid() {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec(selection) else {
        return false;
    };
    bytes.len() <= MAX_RECORD_BYTES
        && stores
            .crypto()
            .set_custom_value(DEVICES_KEY, bytes)
            .await
            .is_ok()
}

/// The devices to offer, or nothing where calls are not supported.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub supported: bool,
    pub recording: Vec<AudioDevice>,
    pub playout: Vec<AudioDevice>,
}

/// Why the devices could not be listed. The page sees one code.
#[derive(Debug, PartialEq, Eq)]
pub enum ListError {
    Start(StartError),
    /// The worker did not answer with a device list in time.
    NoAnswer,
}

/// Lists the audio devices through a contained call worker. Blocks for up to
/// [`LIST_TIMEOUT`] after the worker starts; call it from its own thread.
/// Nothing starts where the host does not support calls.
pub fn list(host: &dyn CallHost) -> Result<Listing, ListError> {
    if !host.supported() {
        return Ok(Listing::default());
    }
    let mut link = host.start().map_err(ListError::Start)?;
    let answer = if link.send(ToWorker::ListDevices).is_ok() {
        let deadline = Instant::now() + LIST_TIMEOUT;
        loop {
            match link.events.try_recv() {
                Ok(message) => break Some(message),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(POLL);
                }
                Err(_) => break None,
            }
        }
    } else {
        None
    };
    link.finish(FINISH_TIMEOUT);
    match answer {
        Some(FromWorker::Devices { recording, playout }) => Ok(Listing {
            supported: true,
            recording: clean(recording),
            playout: clean(playout),
        }),
        _ => Err(ListError::NoAnswer),
    }
}

/// Makes worker devices safe to show: bounded IDs, normalized and bounded
/// names, no repeated ID (the first stays), at most [`MAX_DEVICES`].
fn clean(devices: Vec<AudioDevice>) -> Vec<AudioDevice> {
    let mut kept: Vec<AudioDevice> = Vec::new();
    for device in devices {
        if kept.len() == MAX_DEVICES {
            break;
        }
        let mut name = project_name(&device.name);
        while name.len() > MAX_DEVICE_NAME_BYTES {
            name.pop();
        }
        let Some(device) = AudioDevice::bounded(&device.id, &name) else {
            continue;
        };
        if kept.iter().all(|other| other.id != device.id) {
            kept.push(device);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::worker::{CallWorkerLink, WorkerProcess};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::sync::mpsc;

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    struct Process(Arc<AtomicBool>);

    impl WorkerProcess for Process {
        fn kill(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wait_exit(&self, _timeout: Duration) -> bool {
            true
        }
    }

    /// A host that starts a fake worker. `reply` is what the worker sends;
    /// `sent` collects what the core sent to it.
    struct Host {
        supported: bool,
        start: Result<(), StartError>,
        reply: Option<FromWorker>,
        started: Arc<AtomicBool>,
        ended: Arc<AtomicBool>,
        sent: Arc<Mutex<Vec<ToWorker>>>,
    }

    impl Host {
        fn new(reply: Option<FromWorker>) -> Self {
            Self {
                supported: true,
                start: Ok(()),
                reply,
                started: Arc::default(),
                ended: Arc::default(),
                sent: Arc::default(),
            }
        }
    }

    impl CallHost for Host {
        fn start(&self) -> Result<CallWorkerLink, StartError> {
            self.started.store(true, Ordering::SeqCst);
            self.start?;
            let (commands, mut command_rx) = mpsc::channel(8);
            let (event_tx, events) = mpsc::channel(8);
            if let Some(reply) = self.reply.clone() {
                event_tx.try_send(reply).unwrap();
            }
            // A closed queue without an answer ends the wait at once.
            drop(event_tx);
            let sent = self.sent.clone();
            std::thread::spawn(move || {
                while let Some(message) = command_rx.blocking_recv() {
                    sent.lock().unwrap().push(message);
                }
            });
            Ok(CallWorkerLink::from_parts(
                commands,
                events,
                Box::new(Process(self.ended.clone())),
            ))
        }

        fn confirm_direct_connection(&self) -> bool {
            false
        }

        fn supported(&self) -> bool {
            self.supported
        }
    }

    /// GIVEN valid and invalid saved records WHEN parsed THEN only valid
    /// ones are read, and a missing field is the default.
    #[test]
    fn given_saved_records_then_only_valid_ones_are_read() {
        assert_eq!(
            parse(br#"{"recording":"a","playout":null}"#),
            Ok(Selection {
                recording: Some("a".into()),
                playout: None
            })
        );
        assert_eq!(parse(b"{}"), Ok(Selection::default()));
        let long = format!(r#"{{"recording":"{}","playout":null}}"#, "d".repeat(513));
        let huge = format!(r#"{{"recording":"a","playout":"{}"}}"#, "d".repeat(2048));
        for bad in [
            &b"not json"[..],
            br#"{"recording":"","playout":null}"#,
            br#"{"recording":"a\nb","playout":null}"#,
            br#"{"recording":"a","playout":null,"extra":1}"#,
            br#"{"recording":1,"playout":null}"#,
            long.as_bytes(),
            huge.as_bytes(),
        ] {
            assert_eq!(
                parse(bad),
                Err(Unreadable),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let at_limit = format!(r#"{{"recording":"{}","playout":null}}"#, "d".repeat(512));
        assert!(parse(at_limit.as_bytes()).is_ok());
    }

    /// GIVEN device IDs WHEN checked THEN empty, long and control IDs fail.
    #[test]
    fn given_device_ids_then_empty_long_and_control_ones_are_invalid() {
        assert!(valid_id("{0.0.1.00000000}.{6a2c}"));
        for bad in ["", "a\u{7f}", "a\tb", &"d".repeat(513)] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        let selection = Selection {
            recording: Some("ok".into()),
            playout: Some(String::new()),
        };
        assert!(!selection.is_valid());
    }

    /// GIVEN a host without call support WHEN listed THEN nothing starts and
    /// the answer is empty and unsupported.
    #[test]
    fn given_an_unsupported_host_then_nothing_starts() {
        let mut host = Host::new(None);
        host.supported = false;
        assert_eq!(list(&host), Ok(Listing::default()));
        assert!(!host.started.load(Ordering::SeqCst));
    }

    /// GIVEN a worker that lists devices WHEN listed THEN names are cleaned,
    /// repeated and bad IDs are dropped, the worker gets `ListDevices` and
    /// is ended.
    #[test]
    fn given_worker_devices_then_names_are_cleaned_and_the_worker_ends() {
        let host = Host::new(Some(FromWorker::Devices {
            recording: vec![
                device("a", "Mic \u{202E}gnp\u{200F}"),
                device("a", "Second with same ID"),
                device("b", "  Two   spaces "),
                device("", "No ID"),
            ],
            playout: vec![device("p", "")],
        }));
        let listing = list(&host).unwrap();
        assert!(listing.supported);
        assert_eq!(
            listing.recording,
            vec![
                device("a", "Mic \u{FFFD}gnp\u{FFFD}"),
                device("b", "Two spaces")
            ]
        );
        assert_eq!(listing.playout, vec![device("p", "")]);
        assert!(host.ended.load(Ordering::SeqCst));
        for _ in 0..100 {
            if !host.sent.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            host.sent.lock().unwrap().first(),
            Some(&ToWorker::ListDevices)
        );
    }

    /// GIVEN a worker that fails, answers another message, closes, or does
    /// not start WHEN listed THEN the listing fails and the worker is ended.
    #[test]
    fn given_a_bad_worker_answer_then_the_listing_fails() {
        for reply in [
            None,
            Some(FromWorker::Left),
            Some(FromWorker::Failed {
                code: persephone_call_protocol::FailureCode::Internal,
            }),
        ] {
            let host = Host::new(reply);
            assert_eq!(list(&host), Err(ListError::NoAnswer));
            assert!(host.ended.load(Ordering::SeqCst));
        }
        let mut host = Host::new(None);
        host.start = Err(StartError::Launch);
        assert_eq!(list(&host), Err(ListError::Start(StartError::Launch)));
    }

    /// GIVEN more devices than the limit and long names WHEN cleaned THEN
    /// the list and each name stay within bounds.
    #[test]
    fn given_oversized_lists_and_names_then_they_are_bounded() {
        let many: Vec<_> = (0..40)
            .map(|n| device(&format!("id{n}"), &"é".repeat(300)))
            .collect();
        let kept = clean(many);
        assert_eq!(kept.len(), MAX_DEVICES);
        assert!(kept.iter().all(|d| d.name.len() <= MAX_DEVICE_NAME_BYTES));
    }
}
