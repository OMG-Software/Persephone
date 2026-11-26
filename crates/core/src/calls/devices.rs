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

use super::worker::{CallHost, StartError, WorkerAudio};
use crate::{store::Stores, views::project_name};
use matrix_sdk_crypto::store::CryptoStore;
use persephone_call_protocol::{
    Background, BackgroundPicture, BlurStrength, ColourFilter, DEFAULT_MIC_GAIN_PERCENT,
    FromWorker, MAX_DEVICE_ID_BYTES, MAX_DEVICE_NAME_BYTES, MAX_DEVICES, MAX_VOLUME_PERCENT,
    MediaDevice, ToWorker, VideoEffect, valid_mic_gain,
};
use persephone_contracts::{CallBackground, CallColourFilter, CallVideoEffect, call_blur_valid};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const DEVICES_KEY: &str = "persephone.call_devices.v1";
/// The largest saved record: three IDs at the limit, each one escaped to
/// at most two bytes a byte (`"` and `\`) plus its quotes, the field names
/// the volume, the microphone settings, the video effect and the mirror
/// flag. Every valid selection serializes under it.
const MAX_RECORD_BYTES: usize = 3 * (2 * MAX_DEVICE_ID_BYTES + 2) + 256;
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
    /// The automatic microphone level (automatic gain control, ADR-0062).
    /// `None` is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_level: Option<bool>,
    /// The manual microphone gain in percent, 0 to 400 in steps of 5
    /// (ADR-0062). `None` is 100 percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_gain: Option<u16>,
    /// The own camera's video effect (ADR-0068). `None` is no effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<CallVideoEffect>,
    /// The own camera's preview is mirrored on the page (ADR-0068). `None`
    /// is on: the page mirrored it before the choice existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<bool>,
}

// The page's blur bounds repeat the worker protocol's (ADR-0068). A blur
// that the contract accepts and the protocol refuses would make
// `worker_effect` drop the whole saved effect at each join, so they must
// match. The page's copies are typed as the contract's exported literals.
const _: () = {
    assert!(persephone_contracts::CALL_BLUR_MIN == persephone_call_protocol::MIN_BLUR_PERCENT);
    assert!(persephone_contracts::CALL_BLUR_MAX == persephone_call_protocol::MAX_BLUR_PERCENT);
    assert!(persephone_contracts::CALL_BLUR_STEP == persephone_call_protocol::BLUR_STEP_PERCENT);
    assert!(
        persephone_contracts::CALL_BLUR_DEFAULT == persephone_call_protocol::DEFAULT_BLUR_PERCENT
    );
};

/// The worker's form of a saved video effect, or `None` for a blur strength
/// that is out of range or off the step (ADR-0068).
pub fn worker_effect(effect: CallVideoEffect) -> Option<VideoEffect> {
    let blur = BlurStrength::new(effect.blur)?;
    let background = match effect.background {
        CallBackground::None => Background::None,
        CallBackground::Blur => Background::Blur(blur),
        CallBackground::Dusk => Background::Picture(BackgroundPicture::Dusk),
        CallBackground::Sea => Background::Picture(BackgroundPicture::Sea),
        CallBackground::Forest => Background::Picture(BackgroundPicture::Forest),
        CallBackground::Studio => Background::Picture(BackgroundPicture::Studio),
    };
    let filter = match effect.filter {
        CallColourFilter::None => ColourFilter::None,
        CallColourFilter::Greyscale => ColourFilter::Greyscale,
        CallColourFilter::Warm => ColourFilter::Warm,
        CallColourFilter::Cool => ColourFilter::Cool,
        CallColourFilter::Brighten => ColourFilter::Brighten,
    };
    Some(VideoEffect { background, filter })
}

/// Whether `id` can be a device ID: not empty, bounded, no control character.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_DEVICE_ID_BYTES && !id.chars().any(char::is_control)
}

impl Selection {
    /// Whether every set ID, the volume and the microphone gain are valid.
    pub fn is_valid(&self) -> bool {
        [&self.recording, &self.playout, &self.camera]
            .into_iter()
            .flatten()
            .all(|id| valid_id(id))
            && self
                .volume
                .is_none_or(|volume| volume <= MAX_VOLUME_PERCENT)
            && self.mic_gain.is_none_or(valid_mic_gain)
            && self
                .effect
                .is_none_or(|effect| call_blur_valid(effect.blur))
    }

    /// The saved video effect, else none (ADR-0068).
    pub fn effect(&self) -> CallVideoEffect {
        self.effect.unwrap_or_default()
    }

    /// The video effect for the worker. A record that `is_valid` refused
    /// never gets here; a blur that is not valid reads as no effect only
    /// for such a record.
    pub fn video_effect(&self) -> VideoEffect {
        worker_effect(self.effect()).unwrap_or(VideoEffect::NONE)
    }

    /// Whether the own preview is mirrored: the saved choice, else on.
    pub fn mirror(&self) -> bool {
        self.mirror.unwrap_or(true)
    }

    /// The automatic microphone level to use: the saved one, else on.
    pub fn auto_level(&self) -> bool {
        self.auto_level.unwrap_or(true)
    }

    /// The manual microphone gain to use: the saved one, else 100 percent.
    /// A gain that is not valid reads as 100 percent; `is_valid` refuses the
    /// record before this is used.
    pub fn mic_gain(&self) -> u16 {
        self.mic_gain
            .filter(|gain| valid_mic_gain(*gain))
            .unwrap_or(DEFAULT_MIC_GAIN_PERCENT)
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
    /// The worker can run background blur and pictures (ADR-0068).
    pub background_effects: bool,
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
/// asks the user; `cancel` ends the wait. Any failure sends nothing. The
/// listing and the camera test (ADR-0059) use it.
#[cfg(target_os = "linux")]
pub(super) fn send_portal_remote(link: &super::worker::CallWorkerLink, cancel: &AtomicBool) {
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
    // The list names no sound server device: it opens none.
    let mut link = host
        .start(&WorkerAudio::default())
        .map_err(ListError::Start)?;
    // Portal path (Linux): listing asks the portal for access, so the desktop
    // may show its dialog. Without access no cameras are listed; microphones
    // and speakers still are.
    #[cfg(target_os = "linux")]
    send_portal_remote(&link, cancel);
    if cancel.load(Ordering::SeqCst) {
        link.finish(FINISH_TIMEOUT);
        return Err(ListError::Cancelled);
    }
    // This worker gets no `SetDiagnostics`, so it sends no records, and its
    // first message is the answer. Turning its log on would need this wait
    // to skip `FromWorker::Diagnostic` (ADR-0058).
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
            background_effects,
        }) => Ok(Listing {
            supported: true,
            recording: clean(recording),
            playout: clean(playout),
            cameras: clean(cameras),
            background_effects,
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
    use persephone_call_protocol::MAX_MIC_GAIN_PERCENT;
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
        fn start(&self, audio: &WorkerAudio) -> Result<CallWorkerLink, StartError> {
            assert_eq!(audio, &WorkerAudio::default(), "the list names no device");
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
                ..Selection::default()
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

    /// GIVEN a record with and without a video effect WHEN read THEN a
    /// missing effect is none and the preview is mirrored, a saved one
    /// is kept and has its worker form, and a blur off the 10 percent steps
    /// or an unknown name is unreadable (ADR-0068).
    #[test]
    fn given_a_saved_video_effect_then_it_is_read_or_default() {
        let old = parse(br#"{"recording":"a","playout":null,"volume":40}"#).unwrap();
        assert_eq!(
            (old.effect, old.mirror, old.effect(), old.mirror()),
            (None, None, CallVideoEffect::default(), true)
        );
        assert_eq!(old.video_effect(), VideoEffect::NONE);
        let set = parse(
            br#"{"recording":null,"playout":null,"effect":{"background":"blur","blur":70,"filter":"warm"},"mirror":true}"#,
        )
        .unwrap();
        assert!(set.mirror());
        let plain = parse(br#"{"recording":null,"playout":null,"mirror":false}"#).unwrap();
        assert!(!plain.mirror(), "a saved choice wins");
        assert_eq!(
            set.video_effect(),
            VideoEffect {
                background: Background::Blur(BlurStrength::new(70).unwrap()),
                filter: ColourFilter::Warm,
            }
        );
        for bad in [
            &br#"{"recording":null,"playout":null,"effect":{"background":"blur","blur":55,"filter":"none"}}"#[..],
            br#"{"recording":null,"playout":null,"effect":{"background":"blur","blur":0,"filter":"none"}}"#,
            br#"{"recording":null,"playout":null,"effect":{"background":"blur","blur":110,"filter":"none"}}"#,
            br#"{"recording":null,"playout":null,"effect":{"background":"moon","blur":50,"filter":"none"}}"#,
            br#"{"recording":null,"playout":null,"effect":{"background":"none","blur":50,"filter":"sepia"}}"#,
            br#"{"recording":null,"playout":null,"effect":{"background":"none","blur":50}}"#,
            br#"{"recording":null,"playout":null,"effect":{"background":"none","blur":50,"filter":"none","x":1}}"#,
            br#"{"recording":null,"playout":null,"mirror":1}"#,
        ] {
            assert_eq!(
                parse(bad),
                Err(Unreadable),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// GIVEN each saved background and filter WHEN given to the worker THEN
    /// each has its own worker form, the blur strength goes only with blur,
    /// and a blur that is not valid has none (ADR-0068).
    #[test]
    fn given_each_saved_effect_then_its_worker_form_matches() {
        let pictures = [
            (CallBackground::Dusk, BackgroundPicture::Dusk),
            (CallBackground::Sea, BackgroundPicture::Sea),
            (CallBackground::Forest, BackgroundPicture::Forest),
            (CallBackground::Studio, BackgroundPicture::Studio),
        ];
        for (saved, picture) in pictures {
            let effect = CallVideoEffect {
                background: saved,
                blur: 30,
                filter: CallColourFilter::None,
            };
            assert_eq!(
                worker_effect(effect).map(|effect| effect.background),
                Some(Background::Picture(picture))
            );
        }
        let filters = [
            (CallColourFilter::None, ColourFilter::None),
            (CallColourFilter::Greyscale, ColourFilter::Greyscale),
            (CallColourFilter::Warm, ColourFilter::Warm),
            (CallColourFilter::Cool, ColourFilter::Cool),
            (CallColourFilter::Brighten, ColourFilter::Brighten),
        ];
        for (saved, filter) in filters {
            let effect = CallVideoEffect {
                filter: saved,
                ..CallVideoEffect::default()
            };
            assert_eq!(
                worker_effect(effect),
                Some(VideoEffect {
                    background: Background::None,
                    filter,
                })
            );
        }
        for blur in [0, 5, 55, 110] {
            let effect = CallVideoEffect {
                background: CallBackground::Blur,
                blur,
                filter: CallColourFilter::None,
            };
            assert_eq!(worker_effect(effect), None, "{blur}");
        }
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
                auto_level: Some(false),
                mic_gain: Some(MAX_MIC_GAIN_PERCENT),
                effect: Some(CallVideoEffect {
                    background: CallBackground::Forest,
                    blur: 100,
                    filter: CallColourFilter::Greyscale,
                }),
                mirror: Some(false),
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

    /// GIVEN records with and without the microphone settings WHEN read THEN
    /// a missing level is on and a missing gain is 100 percent, saved values
    /// are kept, an older record still reads, and a gain over 400 percent,
    /// off the 5 percent step or of the wrong type is refused.
    #[test]
    fn given_saved_microphone_settings_then_they_are_read_or_default() {
        let old = parse(br#"{"recording":"a","playout":null,"volume":40}"#).unwrap();
        assert_eq!(
            (
                old.auto_level,
                old.mic_gain,
                old.auto_level(),
                old.mic_gain()
            ),
            (None, None, true, DEFAULT_MIC_GAIN_PERCENT)
        );
        let set = parse(br#"{"recording":null,"playout":null,"auto_level":false,"mic_gain":250}"#)
            .unwrap();
        assert_eq!((set.auto_level(), set.mic_gain()), (false, 250));
        for gain in [0, 5, 395, MAX_MIC_GAIN_PERCENT] {
            let selection = Selection {
                mic_gain: Some(gain),
                ..Selection::default()
            };
            assert!(selection.is_valid(), "{gain}");
            assert_eq!(
                parse(&serde_json::to_vec(&selection).unwrap()),
                Ok(selection)
            );
        }
        for bad in [
            &br#"{"recording":null,"playout":null,"mic_gain":401}"#[..],
            br#"{"recording":null,"playout":null,"mic_gain":405}"#,
            br#"{"recording":null,"playout":null,"mic_gain":7}"#,
            br#"{"recording":null,"playout":null,"mic_gain":-5}"#,
            br#"{"recording":null,"playout":null,"mic_gain":65536}"#,
            br#"{"recording":null,"playout":null,"mic_gain":100.5}"#,
            br#"{"recording":null,"playout":null,"mic_gain":"100"}"#,
            br#"{"recording":null,"playout":null,"auto_level":1}"#,
            br#"{"recording":null,"playout":null,"auto_level":"on"}"#,
        ] {
            assert_eq!(
                parse(bad),
                Err(Unreadable),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(
            !Selection {
                mic_gain: Some(MAX_MIC_GAIN_PERCENT + 5),
                ..Selection::default()
            }
            .is_valid()
        );
        // Nothing set is not written, so an older build still reads it.
        let none = String::from_utf8(serde_json::to_vec(&Selection::default()).unwrap()).unwrap();
        assert!(!none.contains("auto_level") && !none.contains("mic_gain"));
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
            ..Selection::default()
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
            background_effects: true,
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
        assert!(listing.background_effects);
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
            background_effects: false,
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
