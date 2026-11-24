//! Microphone and speaker tests (ADR-0059).
//!
//! A test opens the device that a call would open, in the worker's own
//! containment, so it shows whether the worker may use it. On Windows the
//! test talks to the device through WASAPI in shared mode, as libwebrtc's
//! Windows audio module does: the microphone test reports the loudest sample
//! of every 100 ms, the speaker test plays a short tone. Other systems
//! report `Unsupported` for now.
//!
//! Both tests run on a thread of their own and block it. `stop` ends them
//! early.

// The tone and level helpers serve the Windows tests; elsewhere only the
// unit tests use them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::Arc;
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
/// The sample rate of both tests. Windows converts it to the device's own.
const RATE: u32 = 48_000;

/// The tone's sample at frame `n` of a tone of `total` frames: a sine with a
/// short fade at both ends.
pub fn tone_sample(n: u64, total: u64) -> f32 {
    #[allow(clippy::cast_precision_loss)] // frame counts stay far below 2^52
    let (t, end) = (n as f64 / f64::from(RATE), total as f64 / f64::from(RATE));
    let fade = (t / TONE_FADE).min((end - t) / TONE_FADE).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation)] // |value| <= TONE_AMPLITUDE
    let sample = (TONE_AMPLITUDE * fade * (std::f64::consts::TAU * TONE_HZ * t).sin()) as f32;
    sample
}

/// The loudest absolute sample, 0.0 to 1.0. A sample that is not a number
/// counts as silence.
pub fn peak(samples: &[f32]) -> f64 {
    samples
        .iter()
        .filter(|sample| sample.is_finite())
        .fold(0.0_f64, |loudest, sample| {
            loudest.max(f64::from(sample.abs()).min(1.0))
        })
}

/// Listens on the microphone `device` (`None`: the system default) until
/// `stop` or [`LISTEN_TIME`]. `started` runs once the device records;
/// `level` gets the loudest sample of every [`LEVEL_PERIOD`].
pub fn listen(
    device: Option<&str>,
    stop: &Arc<AtomicBool>,
    started: impl FnOnce(),
    level: impl FnMut(f64),
) -> Result<(), FailureCode> {
    platform::listen(device, stop, started, level)
}

/// Plays the tone on the speaker `device` (`None`: the system default)
/// until `stop` or [`TONE_TIME`]. `started` runs once the device plays.
pub fn play_tone(
    device: Option<&str>,
    stop: &Arc<AtomicBool>,
    started: impl FnOnce(),
) -> Result<(), FailureCode> {
    platform::play_tone(device, stop, started)
}

#[cfg(not(windows))]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use persephone_call_protocol::FailureCode;

    pub fn listen(
        _device: Option<&str>,
        _stop: &Arc<AtomicBool>,
        _started: impl FnOnce(),
        _level: impl FnMut(f64),
    ) -> Result<(), FailureCode> {
        Err(FailureCode::Unsupported)
    }

    pub fn play_tone(
        _device: Option<&str>,
        _stop: &Arc<AtomicBool>,
        _started: impl FnOnce(),
    ) -> Result<(), FailureCode> {
        Err(FailureCode::Unsupported)
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::Arc;
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

    use super::{LEVEL_PERIOD, LISTEN_TIME, RATE, TONE_TIME, peak, tone_sample};

    /// `WAVE_FORMAT_IEEE_FLOAT`: 32-bit float samples.
    const FLOAT_FORMAT: u16 = 3;
    /// The shared buffer: 200 ms, in 100 ns units.
    const BUFFER_TIME: i64 = 2_000_000;
    /// How long the thread sleeps between buffer reads and writes.
    const POLL: Duration = Duration::from_millis(10);
    /// The most samples read in one packet: a second of mono audio.
    const MAX_PACKET: u32 = RATE;

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
    fn open(device: Option<&str>, flow: EDataFlow) -> Result<Stream, FailureCode> {
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

    pub fn listen(
        device: Option<&str>,
        stop: &Arc<AtomicBool>,
        started: impl FnOnce(),
        mut level: impl FnMut(f64),
    ) -> Result<(), FailureCode> {
        let stream = open(device, eCapture)?;
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

    pub fn play_tone(
        device: Option<&str>,
        stop: &Arc<AtomicBool>,
        started: impl FnOnce(),
    ) -> Result<(), FailureCode> {
        let stream = open(device, eRender)?;
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

#[cfg(test)]
mod tests {
    use super::{TONE_AMPLITUDE, peak, tone_sample};

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

    /// GIVEN a tone WHEN its samples are made THEN it starts and ends at
    /// silence (no click) and never goes above its loudness.
    #[test]
    fn given_a_tone_then_it_fades_and_stays_within_its_loudness() {
        let total = 48_000;
        assert!(tone_sample(0, total).abs() < 1e-6);
        assert!(tone_sample(total, total).abs() < 1e-6);
        #[allow(clippy::cast_possible_truncation)]
        let limit = TONE_AMPLITUDE as f32 + 1e-6;
        assert!((0..total).all(|n| tone_sample(n, total).abs() <= limit));
        let loudest = (0..total)
            .map(|n| tone_sample(n, total).abs())
            .fold(0.0, f32::max);
        assert!(loudest > 0.2, "{loudest}");
    }

    /// GIVEN this Windows machine WHEN its default microphone is opened and
    /// stopped after half a second, and its default speaker is started and
    /// stopped at once THEN both run and the microphone reports levels. A
    /// machine with no sound device (a build runner) reports
    /// `DeviceNotFound` or `AudioDevice` instead, and a chosen ID that does
    /// not exist is `DeviceNotFound`.
    #[cfg(windows)]
    #[test]
    fn given_this_machine_when_the_default_devices_are_tested_then_they_run() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use persephone_call_protocol::FailureCode;

        let no_device =
            // No device, or the privacy settings refuse it: both are answers.
            |code| {
                matches!(
                    code,
                    FailureCode::DeviceNotFound
                        | FailureCode::AudioDevice
                        | FailureCode::DeviceDenied
                )
            };
        let stop = Arc::new(AtomicBool::new(false));
        let timer = stop.clone();
        let mut levels = 0;
        let listened = super::listen(
            None,
            &stop,
            move || {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    timer.store(true, Ordering::Release);
                });
            },
            |_| levels += 1,
        );
        match listened {
            Ok(()) => assert!(levels >= 3, "{levels} levels"),
            Err(code) => assert!(no_device(code), "{code:?}"),
        }

        let stop = Arc::new(AtomicBool::new(false));
        let at_once = stop.clone();
        match super::play_tone(None, &stop, move || at_once.store(true, Ordering::Release)) {
            Ok(()) => {}
            Err(code) => assert!(no_device(code), "{code:?}"),
        }

        let stop = Arc::new(AtomicBool::new(true));
        assert_eq!(
            super::listen(
                Some("{0.0.1.00000000}.{not-a-device}"),
                &stop,
                || {},
                |_| {}
            ),
            Err(FailureCode::DeviceNotFound)
        );
    }
}
