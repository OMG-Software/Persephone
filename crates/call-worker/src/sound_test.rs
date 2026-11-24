//! Microphone and speaker tests (ADR-0059).
//!
//! A test opens the device that a call would open, in the worker's own
//! containment, so it shows whether the worker may use it. The microphone
//! test reports the loudest sample of every 100 ms; the speaker test plays a
//! short tone.
//!
//! * Windows: WASAPI in shared mode, as libwebrtc's Windows audio module
//!   does.
//! * Linux: one PulseAudio record or playback stream (also served by
//!   `pipewire-pulse`), through the client library that libwebrtc loads
//!   (`dlopen` of `libpulse.so.0`). It is the default stream with no device
//!   name, as libwebrtc opens it: a `pulse:` choice reaches it through
//!   `PULSE_SOURCE` or `PULSE_SINK`, which the caller checked first
//!   (`mapping::sound_path`). [`open`] connects to the sound server, so it
//!   runs before containment stage 2.
//! * macOS: a Core Audio HAL I/O procedure on the device, the layer that
//!   libwebrtc's Mac audio module uses. The microphone test checks the
//!   microphone permission first. macOS code is not compiled on the
//!   development machine; CI builds and tests it.
//!
//! [`open`] prepares a test. [`Opened::run`] runs it on a thread of its own
//! and blocks that thread; `stop` ends it early.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use persephone_call_protocol::FailureCode;

/// How long the microphone test listens when nothing stops it.
pub const LISTEN_TIME: Duration = Duration::from_secs(15);
/// How often the microphone test reports a level.
pub const LEVEL_PERIOD: Duration = Duration::from_millis(100);
/// How long the speaker test plays its tone.
pub const TONE_TIME: Duration = Duration::from_millis(1500);
/// The tone's pitch and loudness: an A at about -12 dBFS.
const TONE_HZ: f64 = 440.0;
const TONE_AMPLITUDE: f64 = 0.25;
/// The tone fades in and out over this long, so it does not click.
const TONE_FADE: f64 = 0.05;
/// The sample rate of the Windows and Linux tests; the system converts it
/// to the device's own. macOS uses the device's rate.
#[cfg_attr(target_os = "macos", allow(dead_code))]
const RATE: u32 = 48_000;

/// Which device a test opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Microphone,
    Speaker,
}

/// The tone's sample at frame `n` of a tone of `total` frames at
/// [`RATE`]: a sine with a short fade at both ends, and silence after the
/// end.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn tone_sample(n: u64, total: u64) -> f32 {
    tone_sample_at(n, total, f64::from(RATE))
}

/// [`tone_sample`] at the sample rate `rate`.
pub fn tone_sample_at(n: u64, total: u64, rate: f64) -> f32 {
    #[allow(clippy::cast_precision_loss)] // frame counts stay far below 2^52
    let (t, end) = (n as f64 / rate, total as f64 / rate);
    let fade = (t / TONE_FADE).min((end - t) / TONE_FADE).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation)] // |value| <= TONE_AMPLITUDE
    let sample = (TONE_AMPLITUDE * fade * (std::f64::consts::TAU * TONE_HZ * t).sin()) as f32;
    sample
}

/// The loudest absolute sample, 0.0 to 1.0. A sample that is not a number
/// counts as silence.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn peak(samples: &[f32]) -> f64 {
    samples
        .iter()
        .copied()
        .fold(0.0_f64, |loudest, sample| loudest.max(magnitude(sample)))
}

/// [`peak`] of little-endian 32-bit float samples in bytes. A last
/// incomplete sample is ignored.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn peak_le(bytes: &[u8]) -> f64 {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| f32::from_le_bytes(*sample))
        .fold(0.0_f64, |loudest, sample| loudest.max(magnitude(sample)))
}

fn magnitude(sample: f32) -> f64 {
    if sample.is_finite() {
        f64::from(sample.abs()).min(1.0)
    } else {
        0.0
    }
}

/// `frames` tone samples from frame `first` of a tone of `total` frames, as
/// little-endian 32-bit floats.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn tone_le(first: u64, frames: usize, total: u64) -> Vec<u8> {
    (first..first + frames as u64)
        .flat_map(|n| tone_sample(n, total).to_le_bytes())
        .collect()
}

/// A test that [`open`] prepared.
pub struct Opened(platform::Opened);

/// Prepares a test of the microphone or speaker `device` (`None`: the
/// system default). A chosen device that is missing is `DeviceNotFound`; no
/// other device is used. On Linux this connects to the sound server and
/// blocks for at most a few seconds: call it before containment stage 2.
pub fn open(kind: Kind, device: Option<&str>) -> Result<Opened, FailureCode> {
    platform::open(kind, device).map(Opened)
}

impl Opened {
    /// Runs the test until `stop`, or until it ends ([`LISTEN_TIME`],
    /// [`TONE_TIME`]). `started` runs once the device records or plays;
    /// the microphone test gives `level` the loudest sample of every
    /// [`LEVEL_PERIOD`]. A test stopped before its device ran ends without
    /// `started`.
    pub fn run(
        self,
        stop: &AtomicBool,
        started: impl FnOnce(),
        level: impl FnMut(f64),
    ) -> Result<(), FailureCode> {
        self.0.run(stop, started, level)
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use std::sync::atomic::AtomicBool;

    use persephone_call_protocol::FailureCode;

    use super::Kind;

    pub struct Opened;

    pub fn open(_kind: Kind, _device: Option<&str>) -> Result<Opened, FailureCode> {
        Err(FailureCode::Unsupported)
    }

    impl Opened {
        pub fn run(
            self,
            _stop: &AtomicBool,
            _started: impl FnOnce(),
            _level: impl FnMut(f64),
        ) -> Result<(), FailureCode> {
            Err(FailureCode::Unsupported)
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use persephone_call_protocol::FailureCode;
    use windows::Win32::Foundation::E_ACCESSDENIED;
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, EDataFlow, IAudioCaptureClient, IAudioClient,
        IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
        eCapture, eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::core::PCWSTR;

    use super::{Kind, LEVEL_PERIOD, LISTEN_TIME, RATE, TONE_TIME, peak, tone_sample};

    /// `WAVE_FORMAT_IEEE_FLOAT`: 32-bit float samples.
    const FLOAT_FORMAT: u16 = 3;
    /// The shared buffer: 200 ms, in 100 ns units.
    const BUFFER_TIME: i64 = 2_000_000;
    /// How long the thread sleeps between buffer reads and writes.
    const POLL: Duration = Duration::from_millis(10);
    /// The most samples read in one packet: a second of mono audio.
    const MAX_PACKET: u32 = RATE;

    /// A test to run. The device opens on the test's thread, which owns its
    /// COM state.
    pub struct Opened {
        kind: Kind,
        device: Option<String>,
    }

    pub fn open(kind: Kind, device: Option<&str>) -> Result<Opened, FailureCode> {
        Ok(Opened {
            kind,
            device: device.map(str::to_owned),
        })
    }

    impl Opened {
        pub fn run(
            self,
            stop: &AtomicBool,
            started: impl FnOnce(),
            level: impl FnMut(f64),
        ) -> Result<(), FailureCode> {
            match self.kind {
                Kind::Microphone => listen(self.device.as_deref(), stop, started, level),
                Kind::Speaker => play_tone(self.device.as_deref(), stop, started),
            }
        }
    }

    /// Ends the COM use of this thread when dropped.
    struct Com;

    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: balances the successful `CoInitializeEx` in `open`, on
            // the same thread, after every COM object of it is dropped
            // (declared after `Com`, so dropped before it).
            unsafe { CoUninitialize() };
        }
    }

    /// One mono 48 kHz float stream on a device, in shared mode.
    struct Stream {
        client: IAudioClient,
        _com: Com,
    }

    fn format() -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: FLOAT_FORMAT,
            nChannels: 1,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 32,
            cbSize: 0,
        }
    }

    /// Windows refuses a device that the user's privacy settings deny with
    /// `E_ACCESSDENIED`; anything else is a device failure.
    fn device_error(error: &windows::core::Error) -> FailureCode {
        if error.code() == E_ACCESSDENIED {
            FailureCode::DeviceDenied
        } else {
            FailureCode::AudioDevice
        }
    }

    /// Opens `device`, or the console default of `flow`. A chosen device
    /// that is missing is `DeviceNotFound`; no other device is used.
    fn open_stream(device: Option<&str>, flow: EDataFlow) -> Result<Stream, FailureCode> {
        // SAFETY: this thread has no COM state yet; `Com` ends it on drop.
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return Err(FailureCode::Internal);
        }
        let com = Com;
        // SAFETY: COM is initialised on this thread.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| FailureCode::AudioDevice)?;
        let endpoint: IMMDevice = match device {
            Some(id) => {
                let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
                // SAFETY: `wide` is a NUL-terminated UTF-16 string that lives
                // for the call.
                unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
                    .map_err(|_| FailureCode::DeviceNotFound)?
            }
            // SAFETY: a valid enumerator on its COM thread.
            None => unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
                .map_err(|_| FailureCode::DeviceNotFound)?,
        };
        // SAFETY: a valid endpoint; no activation parameters.
        let client: IAudioClient =
            unsafe { endpoint.Activate(CLSCTX_ALL, None) }.map_err(|e| device_error(&e))?;
        let format = format();
        // SAFETY: `format` is a complete `WAVEFORMATEX` without extra bytes,
        // alive for the call; Windows converts it to the mix format.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_TIME,
                0,
                &format,
                None,
            )
        }
        .map_err(|e| device_error(&e))?;
        Ok(Stream { client, _com: com })
    }

    fn listen(
        device: Option<&str>,
        stop: &AtomicBool,
        started: impl FnOnce(),
        mut level: impl FnMut(f64),
    ) -> Result<(), FailureCode> {
        let stream = open_stream(device, eCapture)?;
        // SAFETY: an initialised capture stream.
        let capture: IAudioCaptureClient =
            unsafe { stream.client.GetService() }.map_err(|e| device_error(&e))?;
        // SAFETY: an initialised stream.
        unsafe { stream.client.Start() }.map_err(|e| device_error(&e))?;
        started();
        let begin = Instant::now();
        let mut period = Instant::now();
        let mut loudest = 0.0_f64;
        let result = loop {
            if stop.load(Ordering::Acquire) || begin.elapsed() >= LISTEN_TIME {
                break Ok(());
            }
            std::thread::sleep(POLL);
            match read_packets(&capture) {
                Ok(peak) => loudest = loudest.max(peak),
                Err(code) => break Err(code),
            }
            if period.elapsed() >= LEVEL_PERIOD {
                level(loudest);
                loudest = 0.0;
                period = Instant::now();
            }
        };
        // SAFETY: a started stream; stopping twice is harmless.
        let _ = unsafe { stream.client.Stop() };
        result
    }

    /// Reads every waiting packet and returns its loudest sample. A packet
    /// that Windows marks silent counts as silence.
    fn read_packets(capture: &IAudioCaptureClient) -> Result<f64, FailureCode> {
        let mut loudest = 0.0_f64;
        loop {
            // SAFETY: a started capture stream.
            let waiting = unsafe { capture.GetNextPacketSize() }.map_err(|e| device_error(&e))?;
            if waiting == 0 {
                return Ok(loudest);
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0_u32;
            let mut flags = 0_u32;
            // SAFETY: valid out pointers; the buffer stays valid until
            // `ReleaseBuffer`.
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .map_err(|e| device_error(&e))?;
            #[allow(clippy::cast_sign_loss)] // a flag bit
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            if !silent && !data.is_null() && frames > 0 && frames <= MAX_PACKET {
                // SAFETY: mono 32-bit float: `frames` samples of 4 bytes,
                // aligned by the audio engine, valid until released.
                let samples =
                    unsafe { std::slice::from_raw_parts(data.cast::<f32>(), frames as usize) };
                loudest = loudest.max(peak(samples));
            }
            // SAFETY: releases the packet just taken, all of it.
            unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| device_error(&e))?;
        }
    }

    fn play_tone(
        device: Option<&str>,
        stop: &AtomicBool,
        started: impl FnOnce(),
    ) -> Result<(), FailureCode> {
        let stream = open_stream(device, eRender)?;
        // SAFETY: an initialised render stream.
        let render: IAudioRenderClient =
            unsafe { stream.client.GetService() }.map_err(|e| device_error(&e))?;
        // SAFETY: an initialised stream.
        let size = unsafe { stream.client.GetBufferSize() }.map_err(|e| device_error(&e))?;
        let total = u64::from(RATE) * u64::try_from(TONE_TIME.as_millis()).unwrap_or(0) / 1000;
        let mut written = 0_u64;
        // The first part goes in before the start, so it plays at once.
        write_tone(&stream.client, &render, size, &mut written, total)?;
        // SAFETY: an initialised stream with data in its buffer.
        unsafe { stream.client.Start() }.map_err(|e| device_error(&e))?;
        started();
        let result = loop {
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            std::thread::sleep(POLL);
            if let Err(code) = write_tone(&stream.client, &render, size, &mut written, total) {
                break Err(code);
            }
            // SAFETY: a started stream.
            let padding =
                unsafe { stream.client.GetCurrentPadding() }.map_err(|e| device_error(&e));
            match padding {
                Ok(0) if written >= total => break Ok(()),
                Ok(_) => {}
                Err(code) => break Err(code),
            }
        };
        // SAFETY: a started stream.
        let _ = unsafe { stream.client.Stop() };
        result
    }

    /// Fills the free part of the buffer with the next tone samples, or
    /// silence after the tone.
    fn write_tone(
        client: &IAudioClient,
        render: &IAudioRenderClient,
        size: u32,
        written: &mut u64,
        total: u64,
    ) -> Result<(), FailureCode> {
        // SAFETY: an initialised stream.
        let padding = unsafe { client.GetCurrentPadding() }.map_err(|e| device_error(&e))?;
        let free = size.saturating_sub(padding);
        let left = total.saturating_sub(*written);
        let frames = u32::try_from(left.min(u64::from(free))).unwrap_or(free);
        if frames == 0 {
            return Ok(());
        }
        // SAFETY: `frames` is at most the free space; the buffer is valid
        // until `ReleaseBuffer`.
        let data = unsafe { render.GetBuffer(frames) }.map_err(|e| device_error(&e))?;
        if !data.is_null() {
            // SAFETY: mono 32-bit float: `frames` samples of 4 bytes.
            let samples =
                unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), frames as usize) };
            for (n, sample) in samples.iter_mut().enumerate() {
                *sample = tone_sample(*written + n as u64, total);
            }
        }
        // SAFETY: releases what was taken, all written.
        unsafe { render.ReleaseBuffer(frames, 0) }.map_err(|e| device_error(&e))?;
        *written += u64::from(frames);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::cell::Cell;
    use std::ffi::{c_char, c_int, c_void};
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use persephone_call_protocol::FailureCode;

    use super::{Kind, LEVEL_PERIOD, LISTEN_TIME, RATE, TONE_TIME, peak_le, tone_le};
    use crate::pulse::symbol;

    /// The longest the connection and the stream may take to open.
    const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
    /// The longest the end of the tone may take to play out.
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
    /// The longest one wait of the main loop, in microseconds.
    const STEP_MICROSECONDS: c_int = 10_000;
    const BYTES_PER_FRAME: usize = 4;
    /// The recording fragment: 20 ms of mono float samples.
    const FRAGMENT_BYTES: u32 = RATE / 50 * 4;
    /// The playback buffer: 200 ms, as on Windows.
    const TARGET_BYTES: u32 = RATE / 5 * 4;
    /// The most bytes read from one fragment: a second of samples.
    const MAX_FRAGMENT_BYTES: usize = RATE as usize * BYTES_PER_FRAME;

    // The values and struct sizes below are from the PulseAudio 16.1
    // headers. A C file of static asserts checked each one against
    // `libpulse-dev` 1:16.1+dfsg1-2ubuntu10.1
    // (docs/evidence/device-tests-linux-macos.md). Check them again when
    // a value is added.
    // pa_context_flags_t: never start a server (the worker cannot execute).
    const PA_CONTEXT_NOAUTOSPAWN: c_int = 1;
    // pa_context_state_t.
    const PA_CONTEXT_READY: c_int = 4;
    const PA_CONTEXT_FAILED: c_int = 5;
    const PA_CONTEXT_TERMINATED: c_int = 6;
    // pa_stream_state_t.
    const PA_STREAM_READY: c_int = 2;
    const PA_STREAM_FAILED: c_int = 3;
    const PA_STREAM_TERMINATED: c_int = 4;
    // pa_sample_format_t.
    const PA_SAMPLE_FLOAT32LE: c_int = 5;
    // pa_stream_flags_t: the stream is never moved to another device, and
    // the buffer sizes below set the latency.
    const PA_STREAM_DONT_MOVE: c_int = 0x0200;
    const PA_STREAM_ADJUST_LATENCY: c_int = 0x2000;
    // pa_seek_mode_t.
    const PA_SEEK_RELATIVE: c_int = 0;
    // pa_error_code_t.
    const PA_ERR_ACCESS: c_int = 1;
    const PA_ERR_NOENTITY: c_int = 5;
    /// "The server's choice" in `pa_buffer_attr`.
    const SERVER_CHOICE: u32 = u32::MAX;

    /// `pa_sample_spec` (pulse/sample.h).
    #[repr(C)]
    pub(super) struct SampleSpec {
        format: c_int,
        rate: u32,
        channels: u8,
    }

    /// `pa_buffer_attr` (pulse/def.h).
    #[repr(C)]
    pub(super) struct BufferAttr {
        maxlength: u32,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        fragsize: u32,
    }

    type SuccessCallback = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void);
    type FreeCallback = unsafe extern "C" fn(*mut c_void);

    /// The library functions used. They stay valid: the library is never
    /// closed.
    struct Api {
        mainloop_new: unsafe extern "C" fn() -> *mut c_void,
        mainloop_get_api: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        mainloop_prepare: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
        mainloop_poll: unsafe extern "C" fn(*mut c_void) -> c_int,
        mainloop_dispatch: unsafe extern "C" fn(*mut c_void) -> c_int,
        mainloop_free: unsafe extern "C" fn(*mut c_void),
        context_new: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
        context_connect:
            unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *const c_void) -> c_int,
        context_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
        context_errno: unsafe extern "C" fn(*const c_void) -> c_int,
        context_disconnect: unsafe extern "C" fn(*mut c_void),
        context_unref: unsafe extern "C" fn(*mut c_void),
        stream_new: unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const SampleSpec,
            *const c_void,
        ) -> *mut c_void,
        stream_connect_record:
            unsafe extern "C" fn(*mut c_void, *const c_char, *const BufferAttr, c_int) -> c_int,
        stream_connect_playback: unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const BufferAttr,
            c_int,
            *const c_void,
            *mut c_void,
        ) -> c_int,
        stream_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
        stream_peek: unsafe extern "C" fn(*mut c_void, *mut *const c_void, *mut usize) -> c_int,
        stream_drop: unsafe extern "C" fn(*mut c_void) -> c_int,
        stream_writable_size: unsafe extern "C" fn(*const c_void) -> usize,
        stream_write: unsafe extern "C" fn(
            *mut c_void,
            *const c_void,
            usize,
            Option<FreeCallback>,
            i64,
            c_int,
        ) -> c_int,
        stream_drain:
            unsafe extern "C" fn(*mut c_void, Option<SuccessCallback>, *mut c_void) -> *mut c_void,
        stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
        stream_unref: unsafe extern "C" fn(*mut c_void),
        operation_cancel: unsafe extern "C" fn(*mut c_void),
        operation_unref: unsafe extern "C" fn(*mut c_void),
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| {
            let handle = crate::pulse::library()?;
            // SAFETY: each type below is the C signature from pulse/*.h.
            unsafe {
                Some(Api {
                    mainloop_new: symbol(handle, c"pa_mainloop_new")?,
                    mainloop_get_api: symbol(handle, c"pa_mainloop_get_api")?,
                    mainloop_prepare: symbol(handle, c"pa_mainloop_prepare")?,
                    mainloop_poll: symbol(handle, c"pa_mainloop_poll")?,
                    mainloop_dispatch: symbol(handle, c"pa_mainloop_dispatch")?,
                    mainloop_free: symbol(handle, c"pa_mainloop_free")?,
                    context_new: symbol(handle, c"pa_context_new")?,
                    context_connect: symbol(handle, c"pa_context_connect")?,
                    context_get_state: symbol(handle, c"pa_context_get_state")?,
                    context_errno: symbol(handle, c"pa_context_errno")?,
                    context_disconnect: symbol(handle, c"pa_context_disconnect")?,
                    context_unref: symbol(handle, c"pa_context_unref")?,
                    stream_new: symbol(handle, c"pa_stream_new")?,
                    stream_connect_record: symbol(handle, c"pa_stream_connect_record")?,
                    stream_connect_playback: symbol(handle, c"pa_stream_connect_playback")?,
                    stream_get_state: symbol(handle, c"pa_stream_get_state")?,
                    stream_peek: symbol(handle, c"pa_stream_peek")?,
                    stream_drop: symbol(handle, c"pa_stream_drop")?,
                    stream_writable_size: symbol(handle, c"pa_stream_writable_size")?,
                    stream_write: symbol(handle, c"pa_stream_write")?,
                    stream_drain: symbol(handle, c"pa_stream_drain")?,
                    stream_disconnect: symbol(handle, c"pa_stream_disconnect")?,
                    stream_unref: symbol(handle, c"pa_stream_unref")?,
                    operation_cancel: symbol(handle, c"pa_operation_cancel")?,
                    operation_unref: symbol(handle, c"pa_operation_unref")?,
                })
            }
        })
        .as_ref()
    }

    /// The test's answer for a sound server error: a refusal is
    /// `DeviceDenied`, a missing device `DeviceNotFound`, anything else a
    /// device failure.
    pub(super) fn failure(error: c_int) -> FailureCode {
        match error {
            PA_ERR_ACCESS => FailureCode::DeviceDenied,
            PA_ERR_NOENTITY => FailureCode::DeviceNotFound,
            _ => FailureCode::AudioDevice,
        }
    }

    /// An open sound server connection with one ready stream. It owns the
    /// main loop, the context and the stream, and frees them on drop.
    pub struct Opened {
        api: &'static Api,
        kind: Kind,
        mainloop: *mut c_void,
        context: *mut c_void,
        stream: *mut c_void,
    }

    // SAFETY: the plain main loop (not the threaded one) and its objects are
    // not tied to a thread. `Opened` owns all of them, and it is used by one
    // thread at a time: opened on one, then moved to the test's thread.
    unsafe impl Send for Opened {}

    impl Drop for Opened {
        fn drop(&mut self) {
            // SAFETY: each pointer is null or owned here. The stream goes
            // before its context, and the context before its main loop.
            unsafe {
                if !self.stream.is_null() {
                    (self.api.stream_disconnect)(self.stream);
                    (self.api.stream_unref)(self.stream);
                }
                if !self.context.is_null() {
                    (self.api.context_disconnect)(self.context);
                    (self.api.context_unref)(self.context);
                }
                if !self.mainloop.is_null() {
                    (self.api.mainloop_free)(self.mainloop);
                }
            }
        }
    }

    /// Connects to the sound server and opens the default record or
    /// playback stream. libwebrtc's PulseAudio module gives no device IDs
    /// that a test could open, so a chosen ID here is `DeviceNotFound`: a
    /// `pulse:` choice reaches the default stream through the environment,
    /// and the caller passes `None` for it.
    pub fn open(kind: Kind, device: Option<&str>) -> Result<Opened, FailureCode> {
        if device.is_some() {
            return Err(FailureCode::DeviceNotFound);
        }
        let api = api().ok_or(FailureCode::AudioDevice)?;
        let deadline = Instant::now() + OPEN_TIMEOUT;
        let mut opened = Opened {
            api,
            kind,
            // SAFETY: plain constructor.
            mainloop: unsafe { (api.mainloop_new)() },
            context: null_mut(),
            stream: null_mut(),
        };
        if opened.mainloop.is_null() {
            return Err(FailureCode::AudioDevice);
        }
        // SAFETY: the main loop is live; the name is a C string.
        opened.context = unsafe {
            (api.context_new)(
                (api.mainloop_get_api)(opened.mainloop),
                c"Persephone".as_ptr(),
            )
        };
        if opened.context.is_null() {
            return Err(FailureCode::AudioDevice);
        }
        // SAFETY: the context is live. No server name: the client
        // configuration and environment choose it, as for libwebrtc.
        if unsafe { (api.context_connect)(opened.context, null(), PA_CONTEXT_NOAUTOSPAWN, null()) }
            < 0
        {
            return Err(opened.failure());
        }
        opened.wait(deadline, |opened| {
            // SAFETY: the context is live.
            match unsafe { (opened.api.context_get_state)(opened.context) } {
                PA_CONTEXT_READY => Some(true),
                PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => Some(false),
                _ => None,
            }
        })?;
        let spec = SampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: RATE,
            channels: 1,
        };
        // SAFETY: a ready context, a C string name and a live spec; no
        // channel map (mono's default).
        opened.stream = unsafe {
            (api.stream_new)(
                opened.context,
                c"Persephone device test".as_ptr(),
                &spec,
                null(),
            )
        };
        if opened.stream.is_null() {
            return Err(opened.failure());
        }
        let flags = PA_STREAM_DONT_MOVE | PA_STREAM_ADJUST_LATENCY;
        // SAFETY: a new stream; no device name (the default stream, as
        // libwebrtc opens it); the library copies the attributes.
        let connected = unsafe {
            match kind {
                Kind::Microphone => (api.stream_connect_record)(
                    opened.stream,
                    null(),
                    &BufferAttr {
                        maxlength: SERVER_CHOICE,
                        tlength: SERVER_CHOICE,
                        prebuf: SERVER_CHOICE,
                        minreq: SERVER_CHOICE,
                        fragsize: FRAGMENT_BYTES,
                    },
                    flags,
                ),
                Kind::Speaker => (api.stream_connect_playback)(
                    opened.stream,
                    null(),
                    &BufferAttr {
                        maxlength: SERVER_CHOICE,
                        tlength: TARGET_BYTES,
                        prebuf: SERVER_CHOICE,
                        minreq: SERVER_CHOICE,
                        fragsize: SERVER_CHOICE,
                    },
                    flags,
                    null(),
                    null_mut(),
                ),
            }
        };
        if connected < 0 {
            return Err(opened.failure());
        }
        opened.wait(deadline, Opened::stream_ready)?;
        Ok(opened)
    }

    impl Opened {
        pub fn run(
            self,
            stop: &AtomicBool,
            started: impl FnOnce(),
            level: impl FnMut(f64),
        ) -> Result<(), FailureCode> {
            match self.kind {
                Kind::Microphone => self.listen(stop, started, level),
                Kind::Speaker => self.play_tone(stop, started),
            }
        }

        /// The sound server's last error as a test answer.
        fn failure(&self) -> FailureCode {
            // SAFETY: the context is live.
            failure(unsafe { (self.api.context_errno)(self.context) })
        }

        /// Runs the main loop once for at most one step. `false` on a loop
        /// error.
        fn step(&self) -> bool {
            // SAFETY: the main loop is live and used on this thread only.
            unsafe {
                (self.api.mainloop_prepare)(self.mainloop, STEP_MICROSECONDS) >= 0
                    && (self.api.mainloop_poll)(self.mainloop) >= 0
                    && (self.api.mainloop_dispatch)(self.mainloop) >= 0
            }
        }

        /// `Some(true)` once the stream is ready, `Some(false)` when it
        /// failed or ended.
        fn stream_ready(&self) -> Option<bool> {
            // SAFETY: the stream is live.
            match unsafe { (self.api.stream_get_state)(self.stream) } {
                PA_STREAM_READY => Some(true),
                PA_STREAM_FAILED | PA_STREAM_TERMINATED => Some(false),
                _ => None,
            }
        }

        /// Runs the main loop until `ready` answers. A failure is the sound
        /// server's error; a loop error or the deadline is a device failure.
        fn wait(
            &self,
            deadline: Instant,
            ready: impl Fn(&Self) -> Option<bool>,
        ) -> Result<(), FailureCode> {
            loop {
                match ready(self) {
                    Some(true) => return Ok(()),
                    Some(false) => return Err(self.failure()),
                    None => {}
                }
                if Instant::now() >= deadline || !self.step() {
                    return Err(FailureCode::AudioDevice);
                }
            }
        }

        /// One step of a running test. A stream that is no longer ready,
        /// for example because its device went, ends the test.
        fn step_running(&self) -> Result<(), FailureCode> {
            if !self.step() {
                return Err(FailureCode::AudioDevice);
            }
            match self.stream_ready() {
                Some(true) => Ok(()),
                _ => Err(self.failure()),
            }
        }

        fn listen(
            &self,
            stop: &AtomicBool,
            started: impl FnOnce(),
            mut level: impl FnMut(f64),
        ) -> Result<(), FailureCode> {
            started();
            let begin = Instant::now();
            let mut period = Instant::now();
            let mut loudest = 0.0_f64;
            while !stop.load(Ordering::Acquire) && begin.elapsed() < LISTEN_TIME {
                self.step_running()?;
                loudest = loudest.max(self.read_fragments()?);
                if period.elapsed() >= LEVEL_PERIOD {
                    level(loudest);
                    loudest = 0.0;
                    period = Instant::now();
                }
            }
            Ok(())
        }

        /// Reads every waiting fragment and returns its loudest sample.
        fn read_fragments(&self) -> Result<f64, FailureCode> {
            let mut loudest = 0.0_f64;
            loop {
                let mut data: *const c_void = null();
                let mut bytes = 0_usize;
                // SAFETY: a ready record stream; valid out pointers.
                if unsafe { (self.api.stream_peek)(self.stream, &mut data, &mut bytes) } < 0 {
                    return Err(self.failure());
                }
                if bytes == 0 {
                    return Ok(loudest);
                }
                // A null pointer with a size is a hole in the data: it is
                // dropped like data.
                if !data.is_null() {
                    // SAFETY: the library gives `bytes` readable bytes at
                    // `data` until `pa_stream_drop`; no more are read.
                    let fragment = unsafe {
                        std::slice::from_raw_parts(data.cast::<u8>(), bytes.min(MAX_FRAGMENT_BYTES))
                    };
                    loudest = loudest.max(peak_le(fragment));
                }
                // SAFETY: a fragment was peeked above.
                if unsafe { (self.api.stream_drop)(self.stream) } < 0 {
                    return Err(self.failure());
                }
            }
        }

        fn play_tone(&self, stop: &AtomicBool, started: impl FnOnce()) -> Result<(), FailureCode> {
            let total = u64::from(RATE) * u64::try_from(TONE_TIME.as_millis()).unwrap_or(0) / 1000;
            let mut written = 0_u64;
            // The first part goes in at once, so it plays at once.
            self.write_tone(&mut written, total)?;
            started();
            while written < total {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                self.step_running()?;
                self.write_tone(&mut written, total)?;
            }
            self.drain(stop)
        }

        /// Writes the next tone samples into the free part of the buffer.
        fn write_tone(&self, written: &mut u64, total: u64) -> Result<(), FailureCode> {
            // SAFETY: a ready playback stream.
            let writable = unsafe { (self.api.stream_writable_size)(self.stream) };
            if writable == usize::MAX {
                return Err(self.failure());
            }
            let left = usize::try_from(total.saturating_sub(*written)).unwrap_or(usize::MAX);
            let frames = (writable / BYTES_PER_FRAME)
                .min(left)
                .min(TARGET_BYTES as usize / BYTES_PER_FRAME);
            if frames == 0 {
                return Ok(());
            }
            let bytes = tone_le(*written, frames, total);
            // SAFETY: `bytes` lives for the call; without a free function
            // the library copies it.
            let result = unsafe {
                (self.api.stream_write)(
                    self.stream,
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    None,
                    0,
                    PA_SEEK_RELATIVE,
                )
            };
            if result < 0 {
                return Err(self.failure());
            }
            *written += frames as u64;
            Ok(())
        }

        /// Waits until the written tone has played, at most
        /// [`DRAIN_TIMEOUT`] or until `stop`.
        fn drain(&self, stop: &AtomicBool) -> Result<(), FailureCode> {
            let drained = Cell::new(false);
            // SAFETY: a ready playback stream. `drained` outlives the
            // operation: it is cancelled below, after which it never calls
            // back.
            let operation = unsafe {
                (self.api.stream_drain)(
                    self.stream,
                    Some(on_drained),
                    std::ptr::from_ref(&drained).cast_mut().cast(),
                )
            };
            if operation.is_null() {
                return Err(self.failure());
            }
            let deadline = Instant::now() + DRAIN_TIMEOUT;
            let result = loop {
                if drained.get() || stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                    break Ok(());
                }
                if let Err(code) = self.step_running() {
                    break Err(code);
                }
            };
            // SAFETY: the operation is owned here; cancelling one that ended
            // changes nothing.
            unsafe {
                (self.api.operation_cancel)(operation);
                (self.api.operation_unref)(operation);
            }
            result
        }
    }

    unsafe extern "C" fn on_drained(_stream: *mut c_void, _success: c_int, userdata: *mut c_void) {
        // SAFETY: `userdata` is the `Cell` that `drain` keeps alive until it
        // cancels the operation.
        unsafe { &*userdata.cast::<Cell<bool>>() }.set(true);
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    use persephone_call_protocol::FailureCode;

    use super::{Kind, LEVEL_PERIOD, LISTEN_TIME, TONE_TIME, peak, tone_sample_at};

    /// The longest wait for the user's answer to the microphone permission
    /// prompt, as for the camera.
    const AUTHORIZATION_WAIT: Duration = Duration::from_secs(60);
    /// How often the test looks at its stop flag, the levels and the tone.
    const POLL: Duration = Duration::from_millis(10);
    /// How much longer than the tone the speaker may take to play it.
    const TONE_GRACE: Duration = Duration::from_secs(2);
    /// The most buffers and the most samples of one buffer read in one I/O
    /// cycle.
    const MAX_BUFFERS: usize = 64;
    const MAX_BUFFER_SAMPLES: usize = 1 << 20;

    type OsStatus = i32;
    type AudioObjectId = u32;
    type IoProc = unsafe extern "C" fn(
        AudioObjectId,
        *const c_void,
        *const AudioBufferList,
        *const c_void,
        *mut AudioBufferList,
        *const c_void,
        *mut c_void,
    ) -> OsStatus;

    /// `AudioObjectPropertyAddress` (CoreAudio/AudioHardwareBase.h).
    #[repr(C)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    /// `AudioBuffer` (CoreAudioTypes.h).
    #[repr(C)]
    struct AudioBuffer {
        channels: u32,
        bytes: u32,
        data: *mut c_void,
    }

    /// `AudioBufferList`: a count, then that many `AudioBuffer`s.
    #[repr(C)]
    struct AudioBufferList {
        count: u32,
        buffers: [AudioBuffer; 1],
    }

    /// `AudioStreamBasicDescription` (CoreAudioTypes.h).
    #[repr(C)]
    #[derive(Default)]
    struct StreamFormat {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    const fn code(tag: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*tag)
    }

    const SYSTEM_OBJECT: AudioObjectId = 1;
    const UNKNOWN_OBJECT: AudioObjectId = 0;
    const SCOPE_GLOBAL: u32 = code(b"glob");
    const SCOPE_INPUT: u32 = code(b"inpt");
    const SCOPE_OUTPUT: u32 = code(b"outp");
    const ELEMENT_MAIN: u32 = 0;
    const DEFAULT_INPUT: u32 = code(b"dIn ");
    const DEFAULT_OUTPUT: u32 = code(b"dOut");
    const TRANSLATE_UID: u32 = code(b"uidd");
    const STREAM_FORMAT: u32 = code(b"sfmt");
    const LINEAR_PCM: u32 = code(b"lpcm");
    const FLAG_FLOAT: u32 = 1;
    const FLAG_BIG_ENDIAN: u32 = 2;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyData(
            object: AudioObjectId,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> OsStatus;
        fn AudioDeviceCreateIOProcID(
            device: AudioObjectId,
            procedure: IoProc,
            client: *mut c_void,
            id: *mut Option<IoProc>,
        ) -> OsStatus;
        fn AudioDeviceDestroyIOProcID(device: AudioObjectId, id: Option<IoProc>) -> OsStatus;
        fn AudioDeviceStart(device: AudioObjectId, id: Option<IoProc>) -> OsStatus;
        fn AudioDeviceStop(device: AudioObjectId, id: Option<IoProc>) -> OsStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            allocator: *const c_void,
            bytes: *const u8,
            length: isize,
            encoding: u32,
            external: u8,
        ) -> *const c_void;
        fn CFRelease(object: *const c_void);
    }

    /// A test to run. The device opens on the test's thread.
    pub struct Opened {
        kind: Kind,
        device: Option<String>,
    }

    pub fn open(kind: Kind, device: Option<&str>) -> Result<Opened, FailureCode> {
        Ok(Opened {
            kind,
            device: device.map(str::to_owned),
        })
    }

    /// Reads one property of `object` into `data`. `false` when the system
    /// refuses it or gives a value of another size.
    fn property<T>(
        object: AudioObjectId,
        selector: u32,
        scope: u32,
        qualifier: Option<&*const c_void>,
        data: &mut T,
    ) -> bool {
        let address = PropertyAddress {
            selector,
            scope,
            element: ELEMENT_MAIN,
        };
        let expected = u32::try_from(std::mem::size_of::<T>()).unwrap_or(0);
        let mut size = expected;
        let (qualifier_size, qualifier) = match qualifier {
            Some(value) => (
                u32::try_from(std::mem::size_of::<*const c_void>()).unwrap_or(0),
                std::ptr::from_ref(value).cast::<c_void>(),
            ),
            None => (0, std::ptr::null()),
        };
        // SAFETY: `data` has `size` writable bytes; the address and the
        // qualifier live for the call.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                qualifier_size,
                qualifier,
                &mut size,
                std::ptr::from_mut(data).cast(),
            )
        };
        status == 0 && size == expected
    }

    /// The chosen device by its Core Audio UID, or the system default for
    /// `kind`. A UID that names no device is `DeviceNotFound`; no other
    /// device is used.
    fn device(kind: Kind, uid: Option<&str>) -> Result<AudioObjectId, FailureCode> {
        let mut device = UNKNOWN_OBJECT;
        let found = match uid {
            None => {
                let selector = match kind {
                    Kind::Microphone => DEFAULT_INPUT,
                    Kind::Speaker => DEFAULT_OUTPUT,
                };
                property(SYSTEM_OBJECT, selector, SCOPE_GLOBAL, None, &mut device)
            }
            Some(uid) => {
                let length = isize::try_from(uid.len()).map_err(|_| FailureCode::DeviceNotFound)?;
                // SAFETY: `uid` has `length` bytes of UTF-8; the string is
                // released below.
                let string = unsafe {
                    CFStringCreateWithBytes(std::ptr::null(), uid.as_ptr(), length, UTF8, 0)
                };
                if string.is_null() {
                    return Err(FailureCode::DeviceNotFound);
                }
                let found = property(
                    SYSTEM_OBJECT,
                    TRANSLATE_UID,
                    SCOPE_GLOBAL,
                    Some(&string),
                    &mut device,
                );
                // SAFETY: the string was created above and is not used again.
                unsafe { CFRelease(string) };
                found
            }
        };
        if found && device != UNKNOWN_OBJECT {
            Ok(device)
        } else {
            Err(FailureCode::DeviceNotFound)
        }
    }

    /// The device's sample rate on the side `kind` uses. A device with no
    /// stream there is `DeviceNotFound`; a format other than 32-bit
    /// little-endian float, which the I/O procedure reads and writes, is a
    /// device failure.
    fn float_rate(device: AudioObjectId, kind: Kind) -> Result<f64, FailureCode> {
        let scope = match kind {
            Kind::Microphone => SCOPE_INPUT,
            Kind::Speaker => SCOPE_OUTPUT,
        };
        let mut format = StreamFormat::default();
        if !property(device, STREAM_FORMAT, scope, None, &mut format)
            || format.channels_per_frame == 0
        {
            return Err(FailureCode::DeviceNotFound);
        }
        let float = format.format_id == LINEAR_PCM
            && format.format_flags & FLAG_FLOAT != 0
            && format.format_flags & FLAG_BIG_ENDIAN == 0
            && format.bits_per_channel == 32;
        if !float || !(8_000.0..=384_000.0).contains(&format.sample_rate) {
            return Err(FailureCode::AudioDevice);
        }
        Ok(format.sample_rate)
    }

    /// Checks the microphone permission. `NotDetermined` asks the user and
    /// waits at most [`AUTHORIZATION_WAIT`]. `Ok(false)` is a stop during the
    /// wait; a refusal, a restriction or no answer is `DeviceDenied`.
    fn authorize(stop: &AtomicBool) -> Result<bool, FailureCode> {
        // SAFETY: `AVMediaTypeAudio` is a constant of the loaded framework.
        let media = unsafe { AVMediaTypeAudio }.ok_or(FailureCode::AudioDevice)?;
        // SAFETY: `media` is `AVMediaTypeAudio`, a type the call accepts.
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
        if status == AVAuthorizationStatus::Authorized {
            return Ok(true);
        }
        if status != AVAuthorizationStatus::NotDetermined {
            return Err(FailureCode::DeviceDenied);
        }
        let (sender, receiver) = mpsc::channel::<bool>();
        let handler: RcBlock<dyn Fn(Bool)> = RcBlock::new(move |granted: Bool| {
            let _ = sender.send(granted.as_bool());
        });
        // SAFETY: `media` is `AVMediaTypeAudio`. The framework copies the
        // block, so it outlives this call; it runs once on an arbitrary queue.
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler) };
        let deadline = Instant::now() + AUTHORIZATION_WAIT;
        while Instant::now() < deadline {
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(true) => return Ok(true),
                Ok(false) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        Err(FailureCode::DeviceDenied)
    }

    /// What the I/O procedure shares with the test's thread.
    struct Shared {
        kind: Kind,
        /// Microphone: the loudest sample since the last read, as `f64`
        /// bits. The value is never negative, so the bit order is the
        /// number order.
        loudest: AtomicU64,
        /// Speaker: the frames written so far, of a tone of `total` frames
        /// at `rate`.
        written: AtomicU64,
        total: u64,
        rate: f64,
    }

    /// The buffers of a list the system gives to the I/O procedure.
    ///
    /// # Safety
    ///
    /// `list` is null or a valid buffer list for the duration of `'a`.
    unsafe fn buffers<'a>(list: *const AudioBufferList) -> impl Iterator<Item = &'a AudioBuffer> {
        // SAFETY: as the caller states; `count` buffers follow the count.
        let count = if list.is_null() {
            0
        } else {
            unsafe { (*list).count as usize }.min(MAX_BUFFERS)
        };
        let first = if list.is_null() {
            std::ptr::null()
        } else {
            // SAFETY: as above.
            unsafe { (&raw const (*list).buffers).cast::<AudioBuffer>() }
        };
        // SAFETY: each index is below the list's count.
        (0..count).map(move |index| unsafe { &*first.add(index) })
    }

    /// The samples of one buffer of 32-bit floats, at most
    /// [`MAX_BUFFER_SAMPLES`].
    fn samples(buffer: &AudioBuffer) -> Option<(*mut f32, usize)> {
        if buffer.data.is_null() || buffer.channels == 0 {
            return None;
        }
        Some((
            buffer.data.cast::<f32>(),
            (buffer.bytes as usize / 4).min(MAX_BUFFER_SAMPLES),
        ))
    }

    /// The I/O procedure. It runs on the system's real-time audio thread:
    /// no allocation, no lock.
    unsafe extern "C" fn io_proc(
        _device: AudioObjectId,
        _now: *const c_void,
        input: *const AudioBufferList,
        _input_time: *const c_void,
        output: *mut AudioBufferList,
        _output_time: *const c_void,
        client: *mut c_void,
    ) -> OsStatus {
        // SAFETY: `client` is the `Shared` that `run` keeps alive until the
        // procedure is destroyed.
        let shared = unsafe { &*client.cast::<Shared>() };
        match shared.kind {
            Kind::Microphone => {
                let mut loudest = 0.0_f64;
                // SAFETY: the system gives a valid list for this call.
                for buffer in unsafe { buffers(input) } {
                    if let Some((data, length)) = samples(buffer) {
                        // SAFETY: the buffer holds `length` aligned floats
                        // for this call.
                        let samples = unsafe { std::slice::from_raw_parts(data, length) };
                        loudest = loudest.max(peak(samples));
                    }
                }
                shared
                    .loudest
                    .fetch_max(loudest.to_bits(), Ordering::AcqRel);
            }
            Kind::Speaker => {
                let first = shared.written.load(Ordering::Acquire);
                let mut frames = 0_usize;
                // SAFETY: the system gives a valid list for this call.
                for buffer in unsafe { buffers(output) } {
                    if let Some((data, length)) = samples(buffer) {
                        // SAFETY: the buffer holds `length` aligned floats
                        // for this call, and only this procedure writes it.
                        let samples = unsafe { std::slice::from_raw_parts_mut(data, length) };
                        let channels = buffer.channels as usize;
                        for (n, frame) in samples.chunks_mut(channels).enumerate() {
                            frame.fill(tone_sample_at(first + n as u64, shared.total, shared.rate));
                        }
                        frames = frames.max(length / channels);
                    }
                }
                shared
                    .written
                    .store(first + frames as u64, Ordering::Release);
            }
        }
        0
    }

    /// A created I/O procedure. Dropping it stops and destroys it, after
    /// which it never runs again.
    struct Running {
        device: AudioObjectId,
        id: Option<IoProc>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            // SAFETY: the procedure was created on this device; stopping one
            // that did not start is harmless.
            unsafe {
                AudioDeviceStop(self.device, self.id);
                AudioDeviceDestroyIOProcID(self.device, self.id);
            }
        }
    }

    impl Opened {
        pub fn run(
            self,
            stop: &AtomicBool,
            started: impl FnOnce(),
            mut level: impl FnMut(f64),
        ) -> Result<(), FailureCode> {
            // The device first: a missing one is the answer without a
            // permission prompt.
            let device = device(self.kind, self.device.as_deref())?;
            let rate = float_rate(device, self.kind)?;
            if self.kind == Kind::Microphone && !authorize(stop)? {
                return Ok(());
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let total = (rate * TONE_TIME.as_secs_f64()) as u64;
            let shared = Box::new(Shared {
                kind: self.kind,
                loudest: AtomicU64::new(0),
                written: AtomicU64::new(0),
                total,
                rate,
            });
            let client = std::ptr::from_ref::<Shared>(&shared)
                .cast_mut()
                .cast::<c_void>();
            let mut id: Option<IoProc> = None;
            // SAFETY: `client` stays valid until `Running` destroys the
            // procedure, which is dropped before `shared`.
            if unsafe { AudioDeviceCreateIOProcID(device, io_proc, client, &mut id) } != 0
                || id.is_none()
            {
                return Err(FailureCode::AudioDevice);
            }
            let running = Running { device, id };
            // SAFETY: a created procedure on this device.
            if unsafe { AudioDeviceStart(device, id) } != 0 {
                return Err(FailureCode::AudioDevice);
            }
            started();
            let result = match self.kind {
                Kind::Microphone => {
                    let begin = Instant::now();
                    let mut period = Instant::now();
                    while !stop.load(Ordering::Acquire) && begin.elapsed() < LISTEN_TIME {
                        std::thread::sleep(POLL);
                        if period.elapsed() >= LEVEL_PERIOD {
                            level(f64::from_bits(shared.loudest.swap(0, Ordering::AcqRel)));
                            period = Instant::now();
                        }
                    }
                    Ok(())
                }
                Kind::Speaker => {
                    // The tone has played once a tenth of a second of
                    // silence after it went to the device too.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let played = total + (rate / 10.0) as u64;
                    let deadline = Instant::now() + TONE_TIME + TONE_GRACE;
                    loop {
                        if stop.load(Ordering::Acquire)
                            || shared.written.load(Ordering::Acquire) >= played
                        {
                            break Ok(());
                        }
                        if Instant::now() >= deadline {
                            break Err(FailureCode::AudioDevice);
                        }
                        std::thread::sleep(POLL);
                    }
                }
            };
            drop(running);
            drop(shared);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use persephone_call_protocol::FailureCode;

    use super::{Kind, TONE_AMPLITUDE, open, peak, peak_le, tone_le, tone_sample};

    /// GIVEN samples with a negative peak, a value above full scale and a
    /// value that is not a number WHEN the peak is taken THEN it is the
    /// loudest absolute value, at most 1.0, and the NaN is ignored.
    #[test]
    fn given_samples_when_the_peak_is_taken_then_it_is_the_loudest_magnitude() {
        assert!((peak(&[0.1, -0.5, 0.25]) - 0.5).abs() < 1e-6);
        assert!((peak(&[f32::NAN, 0.2]) - 0.2).abs() < 1e-6);
        assert!((peak(&[3.0]) - 1.0).abs() < 1e-9);
        assert!(peak(&[]).abs() < f64::EPSILON);
    }

    /// GIVEN little-endian float bytes with a cut-off last sample WHEN the
    /// peak is taken THEN it matches the float peak and the partial sample
    /// is ignored.
    #[test]
    fn given_float_bytes_when_the_peak_is_taken_then_partial_samples_are_ignored() {
        let mut bytes: Vec<u8> = [0.1_f32, -0.75, f32::INFINITY]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        bytes.extend_from_slice(&1.0_f32.to_le_bytes()[..3]);
        assert!((peak_le(&bytes) - 0.75).abs() < 1e-6);
        assert!(peak_le(&[0, 0]).abs() < f64::EPSILON);
    }

    /// GIVEN a tone WHEN its samples are made THEN it starts and ends at
    /// silence (no click), never goes above its loudness, and is silent
    /// after its end.
    #[test]
    fn given_a_tone_then_it_fades_and_stays_within_its_loudness() {
        let total = 48_000;
        assert!(tone_sample(0, total).abs() < 1e-6);
        assert!(tone_sample(total, total).abs() < 1e-6);
        assert!(tone_sample(total + 100, total).abs() < 1e-6);
        #[allow(clippy::cast_possible_truncation)]
        let limit = TONE_AMPLITUDE as f32 + 1e-6;
        assert!((0..total).all(|n| tone_sample(n, total).abs() <= limit));
        let loudest = (0..total)
            .map(|n| tone_sample(n, total).abs())
            .fold(0.0, f32::max);
        assert!(loudest > 0.2, "{loudest}");
    }

    /// GIVEN a part of the tone WHEN it is made as bytes THEN it holds the
    /// same samples in little-endian order.
    #[test]
    fn given_part_of_the_tone_when_made_as_bytes_then_the_samples_match() {
        let bytes = tone_le(1000, 3, 48_000);
        assert_eq!(bytes.len(), 12);
        for (index, sample) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let value = f32::from_le_bytes(*sample);
            assert_eq!(
                value.to_bits(),
                tone_sample(1000 + index as u64, 48_000).to_bits()
            );
        }
    }

    /// GIVEN this machine WHEN its default microphone is opened and stopped
    /// after half a second, and its default speaker is started and stopped
    /// at once THEN both run and the microphone reports levels. A machine
    /// with no sound device or sound server, or one that refuses it,
    /// reports `DeviceNotFound`, `AudioDevice` or `DeviceDenied` instead. It
    /// records from the real microphone, so it is not in the default run:
    /// `cargo test --locked -p persephone-call-worker -- --ignored
    /// default_devices`. Not on macOS: a permission request from a test
    /// binary outside an app bundle ends the process.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    #[ignore = "records from the real microphone and starts the real speaker"]
    fn given_this_machine_when_the_default_devices_are_tested_then_they_run() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let no_device = |code| {
            matches!(
                code,
                FailureCode::DeviceNotFound | FailureCode::AudioDevice | FailureCode::DeviceDenied
            )
        };
        let stop = Arc::new(AtomicBool::new(false));
        let timer = stop.clone();
        let mut levels = 0;
        let listened = open(Kind::Microphone, None).and_then(|opened| {
            opened.run(
                &stop,
                move || {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        timer.store(true, Ordering::Release);
                    });
                },
                |_| levels += 1,
            )
        });
        match listened {
            Ok(()) => assert!(levels >= 3, "{levels} levels"),
            Err(code) => assert!(no_device(code), "{code:?}"),
        }

        let stop = Arc::new(AtomicBool::new(false));
        let at_once = stop.clone();
        let played = open(Kind::Speaker, None).and_then(|opened| {
            opened.run(
                &stop,
                move || at_once.store(true, Ordering::Release),
                |_| {},
            )
        });
        if let Err(code) = played {
            assert!(no_device(code), "{code:?}");
        }
    }

    /// GIVEN a chosen microphone ID that does not exist WHEN it is tested
    /// THEN the answer is `DeviceNotFound`, and nothing is recorded. On
    /// Linux every chosen ID is refused: a `pulse:` choice goes through the
    /// environment instead.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn given_an_unknown_microphone_id_then_it_is_not_found() {
        let stop = AtomicBool::new(true);
        let result = open(Kind::Microphone, Some("{0.0.1.00000000}.{not-a-device}"))
            .and_then(|opened| opened.run(&stop, || {}, |_| {}));
        assert_eq!(result, Err(FailureCode::DeviceNotFound));
    }

    /// GIVEN a chosen speaker ID that does not exist WHEN it is tested THEN
    /// the answer is `DeviceNotFound`, and nothing plays.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn given_an_unknown_speaker_id_then_it_is_not_found() {
        let stop = AtomicBool::new(true);
        let result = open(Kind::Speaker, Some("{0.0.0.00000000}.{not-a-device}"))
            .and_then(|opened| opened.run(&stop, || {}, |_| {}));
        assert_eq!(result, Err(FailureCode::DeviceNotFound));
    }

    /// GIVEN the PulseAudio structs WHEN they are laid out THEN they have
    /// the header sizes of `pa_sample_spec` and `pa_buffer_attr`; and GIVEN
    /// the sound server's error numbers THEN `PA_ERR_ACCESS` (1) is
    /// `DeviceDenied`, `PA_ERR_NOENTITY` (5) `DeviceNotFound`, and the rest
    /// a device failure. The numbers are written out, not taken from the
    /// constants, so a changed constant fails here; the header values were
    /// checked in C (see the constants).
    #[cfg(target_os = "linux")]
    #[test]
    fn given_pulse_audio_struct_sizes_and_error_numbers_then_they_map_as_the_headers_say() {
        use super::platform::{BufferAttr, SampleSpec, failure};
        assert_eq!(std::mem::size_of::<SampleSpec>(), 12);
        assert_eq!(std::mem::size_of::<BufferAttr>(), 20);
        assert_eq!(failure(1), FailureCode::DeviceDenied);
        assert_eq!(failure(5), FailureCode::DeviceNotFound);
        assert_eq!(failure(6), FailureCode::AudioDevice);
        assert_eq!(failure(0), FailureCode::AudioDevice);
    }
}
