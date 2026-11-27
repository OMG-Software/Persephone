//! The Media Foundation camera backend (Windows).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use persephone_call_protocol::CameraState;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaSource, IMFMediaType, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video, MFSTARTUP_LITE,
    MFShutdown, MFStartup, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, PWSTR};

use super::capture_size_ok;
use super::{CapturedFrame, Format};
use crate::send_tier::SendTier;
use crate::video::{CaptureMode, pick_capture_mode};

/// A failed COM or Media Foundation step: the step and its HRESULT go to the
/// diagnostic log (only while it is on; no device name or ID), and the camera
/// is `Unavailable`. Without this note every failure looks the same.
fn note(step: &'static str, hresult: i32) -> CameraState {
    log::warn!("camera {step} failed: HRESULT {:#010x}", hresult as u32);
    CameraState::Unavailable
}

/// [`note`] for a `windows` error, as a `map_err` argument.
fn failed(step: &'static str) -> impl FnOnce(windows::core::Error) -> CameraState {
    move |error| note(step, error.code().0)
}

/// A step that succeeded but gave nothing usable.
fn missing(what: &'static str) -> CameraState {
    log::warn!("camera {what} missing or invalid");
    CameraState::Unavailable
}

/// Balances `CoInitializeEx` and `MFStartup` on the thread that made them.
struct Platform;

impl Platform {
    fn start() -> Result<Self, CameraState> {
        // SAFETY: COM is initialised on this thread only and `Drop` ends it
        // on the same thread. A failure here leaves nothing to undo.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if com.is_err() {
            return Err(note("CoInitializeEx", com.0));
        }
        // SAFETY: COM is initialised on this thread. `MFShutdown` is called
        // in `Drop` only after this call succeeded.
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            // SAFETY: Balances the successful `CoInitializeEx` above.
            unsafe { CoUninitialize() };
            return Err(failed("MFStartup")(error));
        }
        Ok(Self)
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        // SAFETY: Balances the successful `MFStartup` and `CoInitializeEx`
        // of `start`, on the same thread. Every COM object of this thread
        // is dropped before the `Platform` value.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

/// The video capture devices, as (symbolic link, friendly name).
pub fn list() -> Vec<(String, String)> {
    // MF and COM are set up on a thread of their own, so the caller's
    // thread keeps its COM state.
    std::thread::Builder::new()
        .name("camera-list".to_owned())
        .spawn(|| list_on_this_thread().unwrap_or_default())
        .ok()
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default()
}

fn list_on_this_thread() -> Result<Vec<(String, String)>, CameraState> {
    let _platform = Platform::start()?;
    let activates = enum_devices()?;
    let mut found = Vec::new();
    for activate in &activates {
        let link = string_attribute(
            activate,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        );
        let name = string_attribute(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
        if let Some(link) = link {
            found.push((link, name.unwrap_or_default()));
        }
    }
    // `activates` drops here, before `_platform`.
    drop(activates);
    Ok(found)
}

/// Enumerates the video capture devices. The returned objects must be
/// dropped before MF is shut down.
fn enum_devices() -> Result<Vec<IMFActivate>, CameraState> {
    let mut attributes: Option<IMFAttributes> = None;
    // SAFETY: `attributes` is a valid out pointer for one interface.
    unsafe { MFCreateAttributes(&mut attributes, 1) }.map_err(failed("MFCreateAttributes"))?;
    let attributes = attributes.ok_or_else(|| missing("MFCreateAttributes"))?;
    // SAFETY: `attributes` is a live attribute store; both GUID pointers
    // are valid for the call.
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(failed("SetGUID source type"))?;
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: `array` and `count` are valid out pointers. On success the
    // call allocates `count` interface slots with CoTaskMemAlloc.
    unsafe { MFEnumDeviceSources(&attributes, &mut array, &mut count) }
        .map_err(failed("MFEnumDeviceSources"))?;
    let mut devices = Vec::new();
    if !array.is_null() {
        // SAFETY: On success `array` points to `count` initialised slots.
        // Taking each slot moves its reference out, so the slot holds
        // `None` and the memory block can be freed without a double
        // release. The block came from CoTaskMemAlloc.
        unsafe {
            let slots = std::slice::from_raw_parts_mut(array, count as usize);
            devices.extend(slots.iter_mut().filter_map(Option::take));
            CoTaskMemFree(Some(array as *const c_void));
        }
    }
    Ok(devices)
}

/// A string attribute, or `None` when it is missing or not UTF-16.
fn string_attribute(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut text = PWSTR::null();
    let mut length = 0_u32;
    // SAFETY: `key` and both out pointers are valid. On success the call
    // allocates the string with CoTaskMemAlloc.
    unsafe { activate.GetAllocatedString(key, &mut text, &mut length) }.ok()?;
    if text.is_null() {
        return None;
    }
    // SAFETY: On success `text` points to a NUL-terminated UTF-16 string.
    let value = unsafe { text.to_string() }.ok();
    // SAFETY: `text` came from CoTaskMemAlloc and is not used again.
    unsafe { CoTaskMemFree(Some(text.0 as *const c_void)) };
    value
}

/// The reader's video stream index.
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// A source, its activation object and its reader. Drop order matters:
/// the reader first, then the source is shut down.
struct Opened {
    reader: Option<IMFSourceReader>,
    source: Option<IMFMediaSource>,
    activate: IMFActivate,
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.reader = None;
        self.source = None;
        // SAFETY: Shuts down the source this activation object created.
        // No other reference to the source or reader is left.
        let _ = unsafe { self.activate.ShutdownObject() };
    }
}

fn open(device_id: &str, target: SendTier) -> Result<(Opened, u32), CameraState> {
    let activates = enum_devices()?;
    let activate = activates
        .into_iter()
        .find(|activate| {
            string_attribute(
                activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            )
            .is_some_and(|link| link == device_id)
        })
        .ok_or_else(|| {
            log::warn!("camera to open is not listed");
            CameraState::NotFound
        })?;
    // SAFETY: `activate` is a live activation object.
    let source: IMFMediaSource =
        unsafe { activate.ActivateObject() }.map_err(failed("ActivateObject"))?;
    let mut opened = Opened {
        reader: None,
        source: Some(source.clone()),
        activate,
    };
    let mut attributes: Option<IMFAttributes> = None;
    // SAFETY: `attributes` is a valid out pointer for one interface.
    unsafe { MFCreateAttributes(&mut attributes, 1) }.map_err(failed("MFCreateAttributes"))?;
    let attributes = attributes.ok_or_else(|| missing("MFCreateAttributes"))?;
    // SAFETY: `attributes` is live; the GUID pointer is valid.
    unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }
        .map_err(failed("SetUINT32 video processing"))?;
    // SAFETY: `source` and `attributes` are live interfaces.
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
        .map_err(failed("MFCreateSourceReaderFromMediaSource"))?;
    drop(source);
    let stride = configure(&reader, target)?;
    opened.reader = Some(reader);
    Ok((opened, stride))
}

/// Picks the camera format and asks for NV12 out of it. Returns the stride
/// of the output. The mode is the one that suits the tier `target`
/// ([`pick_capture_mode`]).
fn configure(reader: &IMFSourceReader, target: SendTier) -> Result<u32, CameraState> {
    // SAFETY: `reader` is a live reader; the stream index is the constant
    // the API defines.
    unsafe { reader.SetStreamSelection(STREAM, true) }.map_err(failed("SetStreamSelection"))?;
    let mut types: Vec<IMFMediaType> = Vec::new();
    let mut modes: Vec<CaptureMode> = Vec::new();
    for index in 0..256_u32 {
        // SAFETY: `reader` is live. An index past the last type is an error.
        let Ok(media_type) = (unsafe { reader.GetNativeMediaType(STREAM, index) }) else {
            break;
        };
        let (width, height) = frame_size(&media_type).unwrap_or((0, 0));
        if !capture_size_ok(width, height) {
            continue;
        }
        modes.push(CaptureMode {
            width,
            height,
            fps: frame_rate(&media_type).unwrap_or(0),
        });
        types.push(media_type);
    }
    let best = pick_capture_mode(&modes, target).ok_or_else(|| {
        log::warn!("camera has no usable format: {} listed", modes.len());
        CameraState::Unavailable
    })?;
    let chosen = &types[best];
    let (width, height) = frame_size(chosen).ok_or_else(|| missing("frame size"))?;
    // SAFETY: `reader` and `chosen` are live interfaces.
    unsafe { reader.SetCurrentMediaType(STREAM, None, chosen) }
        .map_err(failed("SetCurrentMediaType native"))?;
    // SAFETY: Creates a media type object that this function owns.
    let wanted = unsafe { MFCreateMediaType() }.map_err(failed("MFCreateMediaType"))?;
    // SAFETY: `wanted` is live; every GUID pointer is valid for the call.
    unsafe {
        wanted
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
            .and_then(|()| {
                wanted.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(width) << 32) | u64::from(height),
                )
            })
    }
    .map_err(failed("NV12 media type"))?;
    // SAFETY: `reader` and `wanted` are live interfaces. The reader adds
    // the decoder or converter that NV12 output needs.
    unsafe { reader.SetCurrentMediaType(STREAM, None, &wanted) }
        .map_err(failed("SetCurrentMediaType NV12"))?;
    current_stride(reader)
}

fn frame_size(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    // SAFETY: `media_type` is a live attribute store; the GUID is valid.
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    Some(((packed >> 32) as u32, packed as u32))
}

fn frame_rate(media_type: &IMFMediaType) -> Option<u32> {
    // SAFETY: `media_type` is a live attribute store; the GUID is valid.
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.ok()?;
    let (numerator, denominator) = ((packed >> 32) as u32, packed as u32);
    numerator.checked_div(denominator)
}

/// The row stride of the reader's current (NV12) output. When the type has
/// no stride, the width is used.
fn current_stride(reader: &IMFSourceReader) -> Result<u32, CameraState> {
    // SAFETY: `reader` is a live reader.
    let current =
        unsafe { reader.GetCurrentMediaType(STREAM) }.map_err(failed("GetCurrentMediaType"))?;
    let (width, height) = frame_size(&current).ok_or_else(|| missing("output frame size"))?;
    if !capture_size_ok(width, height) {
        log::warn!("camera output size {width}x{height} is outside the limits");
        return Err(CameraState::Unavailable);
    }
    // SAFETY: `current` is a live attribute store; the GUID is valid.
    let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|value| value as i32)
        .unwrap_or(width as i32);
    u32::try_from(stride)
        .ok()
        .filter(|stride| *stride >= width)
        .ok_or_else(|| missing("output stride"))
}

/// The thread body of a capture. Reports the result of opening once, then
/// delivers frames until `stop`, an error or the end of the stream.
pub fn capture<F, E>(
    device_id: &str,
    target: SendTier,
    stop: &AtomicBool,
    open_result: Sender<Result<(), CameraState>>,
    mut on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send,
    E: FnOnce() + Send,
{
    let Ok(platform) = Platform::start() else {
        let _ = open_result.send(Err(CameraState::Unavailable));
        return;
    };
    let (opened, mut stride) = match open(device_id, target) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = open_result.send(Err(error));
            drop(platform);
            return;
        }
    };
    let _ = open_result.send(Ok(()));
    drop(open_result);
    let failed = read_frames(&opened, stop, &mut stride, &mut on_frame);
    // Release the camera before MF is shut down.
    drop(opened);
    drop(platform);
    if failed && !stop.load(Ordering::Acquire) {
        on_end();
    }
}

/// Reads until `stop`. Returns true when the capture ended by an error or
/// the end of the stream.
fn read_frames<F: FnMut(CapturedFrame<'_>)>(
    opened: &Opened,
    stop: &AtomicBool,
    stride: &mut u32,
    on_frame: &mut F,
) -> bool {
    let Some(reader) = opened.reader.as_ref() else {
        return true;
    };
    let mut size = current_size(reader);
    while !stop.load(Ordering::Acquire) {
        let mut flags = 0_u32;
        let mut sample = None;
        // SAFETY: `reader` is live; `flags` and `sample` are valid out
        // pointers for the synchronous call.
        let read = unsafe {
            reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))
        };
        if read.is_err()
            || flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32 != 0
        {
            return true;
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            match (current_stride(reader), current_size(reader)) {
                (Ok(new_stride), Some(new_size)) => {
                    *stride = new_stride;
                    size = Some(new_size);
                }
                _ => return true,
            }
        }
        let (Some(sample), Some((width, height))) = (sample, size) else {
            continue;
        };
        // SAFETY: `sample` is a live sample.
        let Ok(buffer) = (unsafe { sample.ConvertToContiguousBuffer() }) else {
            continue;
        };
        deliver(&buffer, width, height, *stride, on_frame);
    }
    false
}

fn current_size(reader: &IMFSourceReader) -> Option<(u32, u32)> {
    // SAFETY: `reader` is a live reader.
    let current = unsafe { reader.GetCurrentMediaType(STREAM) }.ok()?;
    frame_size(&current).filter(|&(width, height)| capture_size_ok(width, height))
}

/// Locks the buffer and hands a whole NV12 picture to the callback. A short
/// buffer is skipped.
fn deliver<F: FnMut(CapturedFrame<'_>)>(
    buffer: &IMFMediaBuffer,
    width: u32,
    height: u32,
    stride: u32,
    on_frame: &mut F,
) {
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut length = 0_u32;
    // SAFETY: `data` and `length` are valid out pointers. On success `data`
    // points to `length` readable bytes until `Unlock`.
    if unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }.is_err() {
        return;
    }
    if !data.is_null() {
        // SAFETY: The buffer is locked and holds `length` readable bytes. The
        // slice ends before `Unlock` below, and the callback cannot keep it
        // past its return.
        let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) };
        // `checked` refuses a buffer that is shorter than a whole picture.
        if let Some(frame) = CapturedFrame::checked(Format::Nv12, width, height, stride, bytes) {
            on_frame(frame);
        }
    }
    // SAFETY: Balances the successful `Lock` above.
    let _ = unsafe { buffer.Unlock() };
}
