//! The AVFoundation camera backend (macOS, ADR-0049). macOS code is not
//! compiled on the development machine; CI builds and tests it.
//!
//! Listing reads the devices with a discovery session and starts no capture.
//! A capture runs an `AVCaptureSession` with one video data output. Its
//! delegate runs on a serial dispatch queue, copies each NV12 picture into a
//! tight buffer and hands it to the callback. The IDs are the devices'
//! `uniqueID`.

use std::slice;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceTypeBuiltInWideAngleCamera,
    AVCaptureDeviceTypeContinuityCamera, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset,
    AVCaptureSessionPreset640x480, AVCaptureSessionPreset960x540, AVCaptureSessionPreset1280x720,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaType,
    AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use persephone_call_protocol::CameraState;

use super::{CapturedFrame, Format, capture_size_ok};
use crate::send_tier::SendTier;

/// The longest wait for the user's answer to the camera permission prompt.
const AUTHORIZATION_WAIT: Duration = Duration::from_secs(60);
/// How often a capture looks at its stop flag and its device.
const POLL: Duration = Duration::from_millis(100);
/// No picture is larger than this on its long side or its short side.
const MAX_LONG_SIDE: usize = 1280;
const MAX_SHORT_SIDE: usize = 720;
/// The key of `kCVPixelBufferPixelFormatTypeKey`.
const PIXEL_FORMAT_KEY: &str = "PixelFormatType";

/// What the delegate needs on its queue: the callback and the copy buffer.
struct Sink {
    on_frame: Box<dyn FnMut(CapturedFrame<'_>) + Send>,
    buffer: Vec<u8>,
}

struct Ivars {
    sink: Mutex<Sink>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and the class has
    // no `Drop` and no other superclass duties.
    #[unsafe(super(NSObject))]
    #[name = "PersephoneCameraFrameDelegate"]
    #[ivars = Ivars]
    struct FrameDelegate;

    unsafe impl NSObjectProtocol for FrameDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn capture_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.deliver(sample);
        }
    }
);

impl FrameDelegate {
    fn new(on_frame: Box<dyn FnMut(CapturedFrame<'_>) + Send>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            sink: Mutex::new(Sink {
                on_frame,
                buffer: Vec::new(),
            }),
        });
        // SAFETY: `init` of `NSObject` is the designated initialiser, and the
        // ivars are set.
        unsafe { msg_send![super(this), init] }
    }

    /// Copies one NV12 picture and gives it to the callback. A picture that
    /// is not NV12, not a size the worker takes or not whole is dropped.
    fn deliver(&self, sample: &CMSampleBuffer) {
        // SAFETY: `sample` is a live sample buffer for the whole callback.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let pixels = &*image;
        if CVPixelBufferGetPixelFormatType(pixels)
            != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            || CVPixelBufferGetPlaneCount(pixels) != 2
        {
            return;
        }
        let (width, height) = (
            CVPixelBufferGetWidth(pixels),
            CVPixelBufferGetHeight(pixels),
        );
        let (Ok(width32), Ok(height32)) = (u32::try_from(width), u32::try_from(height)) else {
            return;
        };
        if !capture_size_ok(width32, height32)
            || width.max(height) > MAX_LONG_SIDE
            || width.min(height) > MAX_SHORT_SIDE
        {
            return;
        }
        let Ok(mut sink) = self.ivars().sink.lock() else {
            return;
        };
        let Sink { on_frame, buffer } = &mut *sink;
        let flags = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: `pixels` is a live pixel buffer. The unlock below balances
        // a successful lock.
        if unsafe { CVPixelBufferLockBaseAddress(pixels, flags) } != kCVReturnSuccess {
            return;
        }
        let copied = copy_nv12(pixels, width, height, buffer);
        // SAFETY: Balances the successful lock above. Nothing reads the
        // planes after it.
        unsafe { CVPixelBufferUnlockBaseAddress(pixels, flags) };
        if !copied {
            return;
        }
        let stride = width + (width & 1);
        if let Some(frame) =
            CapturedFrame::checked(Format::Nv12, width32, height32, stride as u32, buffer)
        {
            on_frame(frame);
        }
    }
}

/// Copies the two planes of a locked NV12 buffer into `out`, as rows of
/// `stride` bytes (the width rounded up to even): the Y plane, then the
/// interleaved UV plane. Returns false for planes that are too small.
fn copy_nv12(
    pixels: &objc2_core_video::CVPixelBuffer,
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) -> bool {
    let stride = width + (width & 1);
    let chroma_rows = height.div_ceil(2);
    let y_pitch = CVPixelBufferGetBytesPerRowOfPlane(pixels, 0);
    let c_pitch = CVPixelBufferGetBytesPerRowOfPlane(pixels, 1);
    let y_base = CVPixelBufferGetBaseAddressOfPlane(pixels, 0)
        .cast::<u8>()
        .cast_const();
    let c_base = CVPixelBufferGetBaseAddressOfPlane(pixels, 1)
        .cast::<u8>()
        .cast_const();
    if y_base.is_null()
        || c_base.is_null()
        || CVPixelBufferGetHeightOfPlane(pixels, 0) < height
        || CVPixelBufferGetHeightOfPlane(pixels, 1) < chroma_rows
        || CVPixelBufferGetWidthOfPlane(pixels, 1).saturating_mul(2) < stride
        || y_pitch < width
        || c_pitch < stride
    {
        return false;
    }
    let total = stride * (height + chroma_rows);
    if out.len() != total {
        out.clear();
        out.resize(total, 0);
    }
    for row in 0..height {
        // SAFETY: The plane has `height` rows of `y_pitch >= width` bytes,
        // all readable while the buffer is locked. `row < height`.
        let source = unsafe { slice::from_raw_parts(y_base.add(row * y_pitch), width) };
        out[row * stride..row * stride + width].copy_from_slice(source);
    }
    for row in 0..chroma_rows {
        // SAFETY: The chroma plane has at least `chroma_rows` rows of
        // `c_pitch >= stride` bytes, readable while the buffer is locked.
        let source = unsafe { slice::from_raw_parts(c_base.add(row * c_pitch), stride) };
        let start = (height + row) * stride;
        out[start..start + stride].copy_from_slice(source);
    }
    true
}

/// The video capture devices the system lists. No session starts.
fn devices() -> Vec<Retained<AVCaptureDevice>> {
    // SAFETY: `AVMediaTypeVideo` is a constant of the loaded framework.
    let Some(media) = (unsafe { AVMediaTypeVideo }) else {
        return Vec::new();
    };
    // SAFETY: The device type constants are strings of the loaded framework.
    // `ExternalUnknown` is deprecated from macOS 14, but the newer
    // `External` does not exist on macOS 13, the minimum system.
    #[allow(deprecated)]
    let types = unsafe {
        NSArray::from_slice(&[
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            objc2_av_foundation::AVCaptureDeviceTypeExternalUnknown,
            AVCaptureDeviceTypeContinuityCamera,
        ])
    };
    // SAFETY: `types` and `media` are live. The discovery session only reads
    // the device list.
    let session = unsafe {
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &types,
            Some(media),
            AVCaptureDevicePosition::Unspecified,
        )
    };
    // SAFETY: `session` is live.
    unsafe { session.devices() }.to_vec()
}

/// The video capture devices, as (unique ID, localised name).
pub fn list() -> Vec<(String, String)> {
    autoreleasepool(|_| {
        devices()
            .iter()
            .map(|device| {
                // SAFETY: `device` is a live capture device.
                unsafe {
                    (
                        device.uniqueID().to_string(),
                        device.localizedName().to_string(),
                    )
                }
            })
            .collect()
    })
}

/// Checks the camera permission. `NotDetermined` asks the user and waits at
/// most [`AUTHORIZATION_WAIT`]; a refusal, a restriction or a stop is
/// `Unavailable`.
fn authorize(stop: &AtomicBool) -> Result<(), CameraState> {
    // SAFETY: `AVMediaTypeVideo` is a constant of the loaded framework.
    let media = unsafe { AVMediaTypeVideo }.ok_or(CameraState::Unavailable)?;
    // SAFETY: `media` is `AVMediaTypeVideo`, a type the call accepts.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    if status == AVAuthorizationStatus::Authorized {
        Ok(())
    } else if status == AVAuthorizationStatus::NotDetermined {
        request_access(media, stop)
    } else {
        Err(CameraState::Unavailable)
    }
}

fn request_access(media: &AVMediaType, stop: &AtomicBool) -> Result<(), CameraState> {
    let (sender, receiver) = mpsc::channel::<bool>();
    let handler: RcBlock<dyn Fn(Bool)> = RcBlock::new(move |granted: Bool| {
        let _ = sender.send(granted.as_bool());
    });
    // SAFETY: `media` is `AVMediaTypeVideo`. The framework copies the block,
    // so it outlives this call; it runs once on an arbitrary queue.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler) };
    let deadline = Instant::now() + AUTHORIZATION_WAIT;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        match receiver.recv_timeout(POLL) {
            Ok(true) => return Ok(()),
            Ok(false) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Err(CameraState::Unavailable)
}

/// An open capture: the session with its input and output. `Drop` stops it
/// and waits until the delegate queue is idle, so the camera is released and
/// no callback runs after it.
struct Opened {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    device: Retained<AVCaptureDevice>,
    queue: DispatchRetained<DispatchQueue>,
    // Held because the output keeps only a weak reference to its delegate.
    _delegate: Retained<FrameDelegate>,
}

impl Opened {
    /// Whether the camera is still plugged in and the session still runs.
    fn alive(&self) -> bool {
        // SAFETY: `device` and `session` are live objects.
        unsafe { self.device.isConnected() && self.session.isRunning() }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: `session` and `output` are live. After `stopRunning` and the
        // removal of the delegate no new callback starts.
        unsafe {
            self.session.stopRunning();
            self.output.setSampleBufferDelegate_queue(None, None);
        }
        // An empty block on the serial queue runs after every callback that
        // was already queued.
        self.queue.exec_sync(|| {});
    }
}

/// The session presets the worker accepts, best first for the tier `target`
/// (`SendTier::preset_order`). All are at most 1280x720.
fn presets(target: SendTier) -> [&'static AVCaptureSessionPreset; 3] {
    // SAFETY: These are constants of the loaded framework.
    let (p1280, p960, p640) = unsafe {
        (
            AVCaptureSessionPreset1280x720,
            AVCaptureSessionPreset960x540,
            AVCaptureSessionPreset640x480,
        )
    };
    target.preset_order().map(|(width, _)| match width {
        1280 => p1280,
        960 => p960,
        _ => p640,
    })
}

/// The video settings that ask for NV12 (video range) pictures.
fn nv12_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    let key = NSString::from_str(PIXEL_FORMAT_KEY);
    let value = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
    let object: &AnyObject = &value;
    NSDictionary::from_slices(&[&*key], &[object])
}

fn open(
    device_id: &str,
    target: SendTier,
    stop: &AtomicBool,
    on_frame: Box<dyn FnMut(CapturedFrame<'_>) + Send>,
) -> Result<Opened, CameraState> {
    authorize(stop)?;
    if stop.load(Ordering::Acquire) {
        return Err(CameraState::Unavailable);
    }
    let device = devices()
        .into_iter()
        // SAFETY: `candidate` is a live capture device.
        .find(|candidate| unsafe { candidate.uniqueID() }.to_string() == device_id)
        .ok_or(CameraState::NotFound)?;
    // SAFETY: `device` is a live capture device.
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
        .map_err(|_| CameraState::Unavailable)?;
    // SAFETY: Both calls make new objects that this function owns.
    let (session, output) = unsafe { (AVCaptureSession::new(), AVCaptureVideoDataOutput::new()) };
    let delegate = FrameDelegate::new(on_frame);
    let queue = DispatchQueue::new("persephone.camera.frames", DispatchQueueAttr::SERIAL);
    // SAFETY: Every object is live. The configuration is committed before the
    // session starts, and the delegate outlives the output's use of it
    // because `Opened` keeps it and `Drop` removes it.
    unsafe {
        session.beginConfiguration();
        let preset_set = presets(target).into_iter().any(|preset| {
            let usable = session.canSetSessionPreset(preset);
            if usable {
                session.setSessionPreset(preset);
            }
            usable
        });
        if !preset_set || !session.canAddInput(&input) {
            session.commitConfiguration();
            return Err(CameraState::Unavailable);
        }
        session.addInput(&input);
        output.setVideoSettings(Some(&nv12_settings()));
        output.setAlwaysDiscardsLateVideoFrames(true);
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        if !session.canAddOutput(&output) {
            output.setSampleBufferDelegate_queue(None, None);
            session.commitConfiguration();
            return Err(CameraState::Unavailable);
        }
        session.addOutput(&output);
        session.commitConfiguration();
    }
    let opened = Opened {
        session,
        output,
        device,
        queue,
        _delegate: delegate,
    };
    // SAFETY: `session` is configured. `startRunning` blocks until the session
    // runs or fails; `Drop` of `opened` stops it on every path.
    unsafe { opened.session.startRunning() };
    if opened.alive() {
        Ok(opened)
    } else {
        Err(CameraState::Unavailable)
    }
}

/// The thread body of a capture. Reports the result of opening once, then
/// waits for `stop` or for the camera to go away. The delegate queue delivers
/// the pictures.
pub fn capture<F, E>(
    device_id: &str,
    target: SendTier,
    stop: &AtomicBool,
    open_result: Sender<Result<(), CameraState>>,
    on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce() + Send,
{
    autoreleasepool(|_| {
        let opened = match open(device_id, target, stop, Box::new(on_frame)) {
            Ok(opened) => opened,
            Err(error) => {
                let _ = open_result.send(Err(error));
                return;
            }
        };
        let _ = open_result.send(Ok(()));
        drop(open_result);
        let mut lost = false;
        while !stop.load(Ordering::Acquire) {
            if !opened.alive() {
                lost = true;
                break;
            }
            std::thread::sleep(POLL);
        }
        // Release the camera before `on_end` runs.
        drop(opened);
        if lost && !stop.load(Ordering::Acquire) {
            on_end();
        }
    });
}
