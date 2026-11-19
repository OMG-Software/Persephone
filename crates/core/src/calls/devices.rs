//! The call's devices (ADR-0043, ADR-0048): the user's saved choice and the list
//! of devices the page offers.
//!
//! * The choice (microphone, speaker, camera, playout volume) belongs to the profile. It is one JSON record in the encrypted
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
    FromWorker, MAX_DEVICE_ID_BYTES, MAX_DEVICE_NAME_BYTES, MAX_DEVICES, MAX_VOLUME_PERCENT,
    MediaDevice, ToWorker,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const DEVICES_KEY: &str = "persephone.call_devices.v1";
/// The largest saved record: three IDs at the limit, each one escaped to
/// at most two bytes a byte (`"` and `\`) plus its quotes, the field names
/// and the volume. Every valid selection serializes under it.
const MAX_RECORD_BYTES: usize = 3 * (2 * MAX_DEVICE_ID_BYTES + 2) + 80;
/// How long the worker has to answer `ListDevices`.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the worker has to exit after the list.
const FINISH_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the wait looks for the answer.
const POLL: Duration = Duration::from_millis(20);

/// The saved microphone, speaker, camera and playout volume. `None` is the
/// system default (for the camera: the first one the worker finds; for the
/// volume: 100 percent). A record saved before cameras or the volume existed
/// has no such field and reads as `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub recording: Option<String>,
    pub playout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// The playout volume of remote call audio, 0 to 100 percent (ADR-0055).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
}

/// Whether `id` can be a device ID: not empty, bounded, no control character.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_DEVICE_ID_BYTES && !id.chars().any(char::is_control)
}

impl Selection {
    /// Whether every set ID and the volume are valid.
    pub fn is_valid(&self) -> bool {
        [&self.recording, &self.playout, &self.camera]
            .into_iter()
            .flatten()
            .all(|id| valid_id(id))
            && self
                .volume
                .is_none_or(|volume| volume <= MAX_VOLUME_PERCENT)
    }

    /// The playout volume to use: the saved one, else 100 percent.
    pub fn volume(&self) -> u8 {
        self.volume
            .unwrap_or(MAX_VOLUME_PERCENT)
            .min(MAX_VOLUME_PERCENT)
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
    pub recording: Vec<MediaDevice>,
    pub playout: Vec<MediaDevice>,
    pub cameras: Vec<MediaDevice>,
}

/// Why the devices could not be listed. The page sees one code.
#[derive(Debug, PartialEq, Eq)]
pub enum ListError {
    Start(StartError),
    /// The worker did not answer with a device list in time.
    NoAnswer,
    /// The page that asked has gone; nothing was listed.
    Cancelled,
}

/// On the portal camera path, gets the PipeWire remote from the camera portal
/// and sends it to the worker. Blocks for up to 60 seconds while the desktop
/// asks the user. Any failure sends nothing.
#[cfg(target_os = "linux")]
fn send_portal_remote(link: &super::worker::CallWorkerLink, cancel: &AtomicBool) {
    use crate::platform::linux::camera_portal;
    use std::os::fd::AsFd;
    if link.camera_path() != super::worker::CameraPath::Portal {
        return;
    }
    if let Ok(remote) = camera_portal::open_remote(cancel) {
        let _ = link.send_camera_handle(remote.as_fd());
    }
}

/// Lists the audio devices through a contained call worker. Blocks for up to
/// [`LIST_TIMEOUT`] after the worker starts; call it from its own thread.
/// Nothing starts where the host does not support calls. `cancel` is set
/// when the page that asked has gone: a camera portal request then ends,
/// and the worker finishes without listing ([`ListError::Cancelled`]).
pub fn list(host: &dyn CallHost, cancel: &AtomicBool) -> Result<Listing, ListError> {
    if !host.supported() {
        return Ok(Listing::default());
    }
    let mut link = host.start().map_err(ListError::Start)?;
    // Portal path (Linux): listing asks the portal for access, so the desktop
    // may show its dialog. Without access no cameras are listed; microphones
    // and speakers still are.
    #[cfg(target_os = "linux")]
    send_portal_remote(&link, cancel);
    if cancel.load(Ordering::SeqCst) {
        link.finish(FINISH_TIMEOUT);
        return Err(ListError::Cancelled);
    }
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
        Some(FromWorker::Devices {
            recording,
            playout,
            cameras,
        }) => Ok(Listing {
            supported: true,
            recording: clean(recording),
            playout: clean(playout),
            cameras: clean(cameras),
        }),
        _ => Err(ListError::NoAnswer),
    }
}

/// Makes worker devices safe to show: bounded IDs, normalized and bounded
/// names, no repeated ID (the first stays), at most [`MAX_DEVICES`].
fn clean(devices: Vec<MediaDevice>) -> Vec<MediaDevice> {
    let mut kept: Vec<MediaDevice> = Vec::new();
    for device in devices {
        if kept.len() == MAX_DEVICES {
            break;
        }
        let mut name = project_name(&device.name);
        while name.len() > MAX_DEVICE_NAME_BYTES {
            name.pop();
        }
        let Some(device) = MediaDevice::bounded(&device.id, &name) else {
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

    fn device(id: &str, name: &str) -> MediaDevice {
        MediaDevice {
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
    /// `sent` collects what the core sent to it; `recorded` is set when the
    /// command queue has closed and `sent` is complete.
    struct Host {
        supported: bool,
        start: Result<(), StartError>,
        reply: Option<FromWorker>,
        started: Arc<AtomicBool>,
        ended: Arc<AtomicBool>,
        sent: Arc<Mutex<Vec<ToWorker>>>,
        recorded: Arc<AtomicBool>,
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
                recorded: Arc::default(),
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
            let recorded = self.recorded.clone();
            std::thread::spawn(move || {
                while let Some(message) = command_rx.blocking_recv() {
                    sent.lock().unwrap().push(message);
                }
                recorded.store(true, Ordering::SeqCst);
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
                playout: None,
                camera: None,
                volume: None,
            })
        );
        assert_eq!(parse(b"{}"), Ok(Selection::default()));
        assert_eq!(
            parse(br#"{"recording":null,"playout":null,"camera":"cam"}"#),
            Ok(Selection {
                camera: Some("cam".into()),
                ..Selection::default()
            })
        );
        let long = format!(r#"{{"recording":"{}","playout":null}}"#, "d".repeat(513));
        let huge = format!(r#"{{"recording":"a","playout":"{}"}}"#, "d".repeat(2048));
        for bad in [
            &b"not json"[..],
            br#"{"recording":"","playout":null}"#,
            br#"{"recording":"a\nb","playout":null}"#,
            br#"{"recording":"a","playout":null,"extra":1}"#,
            br#"{"recording":1,"playout":null}"#,
            br#"{"recording":null,"playout":null,"camera":""}"#,
            br#"{"recording":null,"playout":null,"camera":"a\u0007"}"#,
            br#"{"recording":null,"playout":null,"camera":3}"#,
            br#"{"recording":null,"playout":null,"volume":101}"#,
            br#"{"recording":null,"playout":null,"volume":-1}"#,
            br#"{"recording":null,"playout":null,"volume":50.5}"#,
            br#"{"recording":null,"playout":null,"volume":"50"}"#,
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

    /// GIVEN three IDs at the limit made only of characters JSON escapes
    /// WHEN saved THEN the record fits under the cap and reads back.
    #[test]
    fn given_ids_that_escape_to_twice_their_size_then_the_record_round_trips() {
        for c in ['"', '\\'] {
            let id = c.to_string().repeat(MAX_DEVICE_ID_BYTES);
            let selection = Selection {
                recording: Some(id.clone()),
                playout: Some(id.clone()),
                camera: Some(id),
                volume: Some(MAX_VOLUME_PERCENT),
            };
            assert!(selection.is_valid());
            let bytes = serde_json::to_vec(&selection).unwrap();
            assert!(bytes.len() <= MAX_RECORD_BYTES, "{}", bytes.len());
            assert_eq!(parse(&bytes), Ok(selection));
        }
        let over = format!(
            r#"{{"recording":"a","playout":"{}"}}"#,
            "d".repeat(MAX_RECORD_BYTES)
        );
        assert_eq!(parse(over.as_bytes()), Err(Unreadable));
    }

    /// GIVEN a record with and without a volume WHEN read THEN a missing
    /// volume is 100 percent, a saved one is kept, and a record saved before
    /// the volume existed still reads.
    #[test]
    fn given_saved_volume_then_it_is_read_or_defaults_to_full() {
        let old = parse(br#"{"recording":"a","playout":null,"camera":"c"}"#).unwrap();
        assert_eq!((old.volume, old.volume()), (None, MAX_VOLUME_PERCENT));
        let quiet = parse(br#"{"recording":null,"playout":null,"volume":0}"#).unwrap();
        assert_eq!(quiet.volume(), 0);
        let half = Selection {
            volume: Some(50),
            ..Selection::default()
        };
        let bytes = serde_json::to_vec(&half).unwrap();
        assert_eq!(parse(&bytes), Ok(half));
        assert!(
            !Selection {
                volume: Some(MAX_VOLUME_PERCENT + 1),
                ..Selection::default()
            }
            .is_valid()
        );
        // No volume is not written, so an older build still reads the record.
        let none = serde_json::to_vec(&Selection::default()).unwrap();
        assert!(!String::from_utf8(none).unwrap().contains("volume"));
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
            camera: None,
            volume: None,
        };
        assert!(!selection.is_valid());
        let selection = Selection {
            camera: Some("c\u{7f}".into()),
            ..Selection::default()
        };
        assert!(!selection.is_valid());
    }

    /// GIVEN a host without call support WHEN listed THEN nothing starts and
    /// the answer is empty and unsupported.
    #[test]
    fn given_an_unsupported_host_then_nothing_starts() {
        let mut host = Host::new(None);
        host.supported = false;
        assert_eq!(list(&host, &AtomicBool::new(false)), Ok(Listing::default()));
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
            cameras: vec![device("c", " Front\u{202E}  camera "), device("c", "Again")],
        }));
        let listing = list(&host, &AtomicBool::new(false)).unwrap();
        assert!(listing.supported);
        assert_eq!(
            listing.recording,
            vec![
                device("a", "Mic \u{FFFD}gnp\u{FFFD}"),
                device("b", "Two spaces")
            ]
        );
        assert_eq!(listing.playout, vec![device("p", "")]);
        assert_eq!(listing.cameras, vec![device("c", "Front\u{FFFD} camera")]);
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

    /// GIVEN the page that asked has gone (the cancel flag is set) WHEN
    /// listed THEN the worker is ended without `ListDevices` and the listing
    /// is cancelled.
    #[test]
    fn given_a_cancelled_listing_then_the_worker_ends_without_listing() {
        let host = Host::new(Some(FromWorker::Devices {
            recording: Vec::new(),
            playout: Vec::new(),
            cameras: Vec::new(),
        }));
        assert_eq!(
            list(&host, &AtomicBool::new(true)),
            Err(ListError::Cancelled)
        );
        assert!(host.ended.load(Ordering::SeqCst));
        // `finish` queues `Leave`; the fake worker's thread records it later.
        // The link is gone, so the queue is closed: wait until the thread has
        // drained it, so the check below sees every command that was sent.
        for _ in 0..100 {
            if host.recorded.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            host.recorded.load(Ordering::SeqCst),
            "the recorder drained the closed queue"
        );
        assert_eq!(
            *host.sent.lock().unwrap(),
            vec![ToWorker::Leave],
            "the worker was told to leave and was not asked to list"
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
            assert_eq!(
                list(&host, &AtomicBool::new(false)),
                Err(ListError::NoAnswer)
            );
            assert!(host.ended.load(Ordering::SeqCst));
        }
        let mut host = Host::new(None);
        host.start = Err(StartError::Launch);
        assert_eq!(
            list(&host, &AtomicBool::new(false)),
            Err(ListError::Start(StartError::Launch))
        );
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
