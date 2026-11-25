//! The macOS screen backend (ADR-0064): ScreenCaptureKit of one display,
//! through `objc2`. macOS code is not compiled on the development machine;
//! CI builds and tests it, and a Mac must measure it.
//!
//! The core named the display by its CoreGraphics display ID and its size in
//! points. The worker reads the shareable content when the share starts and
//! takes only the display with that ID; one that is gone or has another size
//! is `NotFound`. The shareable content also lists the windows and
//! applications on screen: the worker reads only its displays and uses no
//! window from it. The code never shows a picker, never asks for the Screen
//! Recording permission (the core did, before it sent the command) and never
//! takes another display. A refusal by the system is `Refused`.
//!
//! The stream captures one display with no windows left out, no audio and
//! no microphone (the microphone switch exists from macOS 15 and is set only
//! there), as BGRA at most 1920x1080 and 5 frames a second. Its
//! delegate runs on a serial dispatch queue, checks each picture before it
//! reads a pixel and hands the locked pixel buffer to the callback; nothing
//! is copied here.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, available, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use persephone_call_protocol::{ScreenState, ScreenTarget};

use super::display::{
    Admit, Cause, FRAMES_PER_SECOND, cause_of, frame_fits, output_size, size_matches,
};
use super::{CapturedFrame, Ended, Format, Grant};

/// The longest wait for the list of displays, and for the stream to start.
/// Both together stay under the 10 s of `screen::OPEN_TIMEOUT`.
const STEP_WAIT: Duration = Duration::from_secs(4);
/// The longest wait for the stream to say that it stopped.
const STOP_WAIT: Duration = Duration::from_millis(1500);
/// How often a wait or the capture loop looks at its stop flag.
const POLL: Duration = Duration::from_millis(100);
/// Frames ScreenCaptureKit may hold for the handler (3 is its smallest).
const QUEUE_DEPTH: isize = 3;

/// `ended` values: the capture runs, the user or the display ended it, or it
/// failed.
const RUNNING: u8 = 0;
const SOURCE_GONE: u8 = 1;
const FAILED: u8 = 2;

/// What the delegate needs on its queue.
struct Sink {
    on_frame: Box<dyn FnMut(CapturedFrame<'_>) + Send>,
    admit: Admit,
}

struct Ivars {
    sink: Mutex<Sink>,
    ended: Arc<AtomicU8>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and the class has
    // no `Drop` and no other superclass duties.
    #[unsafe(super(NSObject))]
    #[name = "PersephoneScreenDelegate"]
    #[ivars = Ivars]
    struct ScreenDelegate;

    unsafe impl NSObjectProtocol for ScreenDelegate {}

    unsafe impl SCStreamOutput for ScreenDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.deliver(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for ScreenDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let state = match cause_of(error.code() as i64) {
                Cause::UserStopped => SOURCE_GONE,
                Cause::Denied | Cause::Other => FAILED,
            };
            // The first reason stands.
            let _ = self.ivars().ended.compare_exchange(
                RUNNING,
                state,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
);

impl ScreenDelegate {
    fn new(
        on_frame: Box<dyn FnMut(CapturedFrame<'_>) + Send>,
        ended: Arc<AtomicU8>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            sink: Mutex::new(Sink {
                on_frame,
                admit: Admit::default(),
            }),
            ended,
        });
        // SAFETY: `init` of `NSObject` is the designated initialiser, and the
        // ivars are set.
        unsafe { msg_send![super(this), init] }
    }

    /// Gives one BGRA picture to the callback. A picture that is not BGRA,
    /// not a size the worker takes, too soon after the last or not whole is
    /// dropped before any pixel is read. A frame with no image buffer (an
    /// idle frame) is dropped too.
    fn deliver(&self, sample: &CMSampleBuffer) {
        let Ok(mut sink) = self.ivars().sink.lock() else {
            return;
        };
        // SAFETY: `sample` is a live sample buffer for the whole callback.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let pixels = &*image;
        if CVPixelBufferGetPixelFormatType(pixels) != kCVPixelFormatType_32BGRA
            || CVPixelBufferGetPlaneCount(pixels) != 0
        {
            return;
        }
        let (width, height) = (
            CVPixelBufferGetWidth(pixels),
            CVPixelBufferGetHeight(pixels),
        );
        let stride = CVPixelBufferGetBytesPerRow(pixels);
        let (Ok(width32), Ok(height32), Ok(stride32)) = (
            u32::try_from(width),
            u32::try_from(height),
            u32::try_from(stride),
        ) else {
            return;
        };
        if !frame_fits(width, height)
            || CapturedFrame::expected_len(width32, height32, stride32).is_none()
            || !sink.admit.due(Instant::now())
        {
            return;
        }
        let Some(length) = CapturedFrame::expected_len(width32, height32, stride32) else {
            return;
        };
        let flags = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: `pixels` is a live pixel buffer. The unlock below balances
        // a successful lock.
        if unsafe { CVPixelBufferLockBaseAddress(pixels, flags) } != kCVReturnSuccess {
            return;
        }
        let base = CVPixelBufferGetBaseAddress(pixels)
            .cast::<u8>()
            .cast_const();
        if !base.is_null() {
            // SAFETY: The buffer is locked, so `base` points to `height`
            // rows of `stride` bytes, which is `length` (checked above with
            // the stride and the size the buffer reports). The slice ends
            // before the unlock and the callback keeps nothing.
            let bytes = unsafe { std::slice::from_raw_parts(base, length) };
            if let Some(frame) =
                CapturedFrame::checked(Format::Bgra, width32, height32, stride32, bytes)
            {
                (sink.on_frame)(frame);
            }
        }
        // SAFETY: Balances the successful lock above. Nothing reads the
        // pixels after it.
        unsafe { CVPixelBufferUnlockBaseAddress(pixels, flags) };
    }
}

/// The result of a block that the framework runs once on a queue of its own.
fn wait<T>(receiver: &Receiver<T>, stop: &AtomicBool, limit: Duration) -> Option<T> {
    let deadline = Instant::now() + limit;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        match receiver.recv_timeout(POLL.min(limit)) {
            Ok(value) => return Some(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
    None
}

fn cause_of_error(error: *mut NSError) -> Cause {
    // SAFETY: The framework passes a live `NSError` or null for the length
    // of the block; the reference ends with this call.
    unsafe { error.as_ref() }.map_or(Cause::Other, |error| cause_of(error.code() as i64))
}

fn state_for(cause: Cause) -> ScreenState {
    match cause {
        Cause::Denied => ScreenState::Refused,
        Cause::UserStopped | Cause::Other => ScreenState::Unavailable,
    }
}

/// The shareable content, handed from the framework's queue to the capture
/// thread.
struct Content(Retained<SCShareableContent>);

// SAFETY: The content is never changed after the framework made it, and it
// goes to exactly one thread, which is the only one that uses it.
unsafe impl Send for Content {}

/// The display the core named by its ID, with its size in points.
fn find_display(
    wanted: u32,
    width: u32,
    height: u32,
    stop: &AtomicBool,
) -> Result<Retained<SCDisplay>, ScreenState> {
    let (sender, receiver) = mpsc::channel::<Result<Content, Cause>>();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: `content` is a live object or null for the block's length;
            // `retain` takes its own reference.
            let answer = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(Content(content)),
                None => Err(cause_of_error(error)),
            };
            let _ = sender.send(answer);
        },
    );
    // SAFETY: The framework copies the block, so it outlives this call; it
    // runs once on a queue of the framework's.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    let content = match wait(&receiver, stop, STEP_WAIT) {
        Some(Ok(Content(content))) => content,
        Some(Err(cause)) => return Err(state_for(cause)),
        None => return Err(ScreenState::Unavailable),
    };
    // SAFETY: `content` is live; the list and each display are read only.
    // Only the displays are read: the windows and applications that the
    // content also lists are not used.
    let displays = unsafe { content.displays() };
    for display in displays.to_vec() {
        // SAFETY: `display` is a live `SCDisplay`.
        let (id, shown_width, shown_height) =
            unsafe { (display.displayID(), display.width(), display.height()) };
        if id == wanted {
            return if size_matches(shown_width as i64, shown_height as i64, width, height) {
                Ok(display)
            } else {
                Err(ScreenState::NotFound)
            };
        }
    }
    Err(ScreenState::NotFound)
}

/// An open capture. `Drop` stops the stream, removes the delegate and waits
/// until its queue is idle, so the display is released and no callback runs
/// after it.
struct Opened {
    stream: Retained<SCStream>,
    queue: DispatchRetained<DispatchQueue>,
    // Held because the stream keeps no strong reference to its delegate.
    delegate: Retained<ScreenDelegate>,
    ended: Arc<AtomicU8>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        let (sender, receiver) = mpsc::channel::<()>();
        let done = RcBlock::new(move |_error: *mut NSError| {
            let _ = sender.send(());
        });
        // SAFETY: `stream` is live. The framework copies the block. A stream
        // that never started, or already stopped, answers with an error,
        // which is not used.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
        let _ = receiver.recv_timeout(STOP_WAIT);
        // SAFETY: `stream` and `delegate` are live. After the removal no new
        // callback starts.
        let _ = unsafe {
            self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.delegate),
                SCStreamOutputType::Screen,
            )
        };
        // An empty block on the serial queue runs after every callback that
        // was already queued.
        self.queue.exec_sync(|| {});
    }
}

fn open(
    id: u32,
    width: u32,
    height: u32,
    stop: &AtomicBool,
    on_frame: Box<dyn FnMut(CapturedFrame<'_>) + Send>,
) -> Result<Opened, ScreenState> {
    let (out_width, out_height) = output_size(width, height).ok_or(ScreenState::Unavailable)?;
    let display = find_display(id, width, height, stop)?;
    if stop.load(Ordering::Acquire) {
        return Err(ScreenState::Unavailable);
    }
    let no_windows: Retained<NSArray<SCWindow>> = NSArray::new();
    // SAFETY: `display` and `no_windows` are live. The filter shows the whole
    // display, with no window left out.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &no_windows,
        )
    };
    // SAFETY: A new object that this function owns; every setter takes a
    // plain value. No audio, no microphone, BGRA, 5 frames a second.
    let config = unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(out_width as usize);
        config.setHeight(out_height as usize);
        config.setMinimumFrameInterval(CMTime::new(1, FRAMES_PER_SECOND));
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setShowsCursor(true);
        config.setCapturesAudio(false);
        // `captureMicrophone` exists from macOS 15. An older system has no
        // such selector, and sending it would raise an exception; there a
        // stream never captures the microphone.
        if available!(macos = 15.0) {
            config.setCaptureMicrophone(false);
        }
        config.setQueueDepth(QUEUE_DEPTH);
        config
    };
    let ended = Arc::new(AtomicU8::new(RUNNING));
    let delegate = ScreenDelegate::new(on_frame, ended.clone());
    // SAFETY: `filter`, `config` and `delegate` are live.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*delegate)),
        )
    };
    let queue = DispatchQueue::new("persephone.screen.frames", DispatchQueueAttr::SERIAL);
    let opened = Opened {
        stream,
        queue,
        delegate,
        ended,
    };
    // SAFETY: Every object is live. `Opened::drop` removes the output and
    // waits for the queue on every path from here.
    unsafe {
        opened.stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*opened.delegate),
            SCStreamOutputType::Screen,
            Some(&opened.queue),
        )
    }
    .map_err(|_| ScreenState::Unavailable)?;
    let (sender, receiver) = mpsc::channel::<Option<Cause>>();
    let started = RcBlock::new(move |error: *mut NSError| {
        let _ = sender.send((!error.is_null()).then(|| cause_of_error(error)));
    });
    // SAFETY: `stream` is configured. The framework copies the block, which
    // runs once. The first frame is not awaited: an idle screen sends none.
    unsafe {
        opened
            .stream
            .startCaptureWithCompletionHandler(Some(&started))
    };
    match wait(&receiver, stop, STEP_WAIT) {
        Some(None) => Ok(opened),
        Some(Some(cause)) => Err(state_for(cause)),
        None => Err(ScreenState::Unavailable),
    }
}

/// The thread body of a capture. Reports the result of opening once, then
/// waits for `stop`, the end of the source or an error. The delegate queue
/// delivers the pictures.
pub fn capture<F, E>(
    target: ScreenTarget,
    _grant: Grant,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), ScreenState>>,
    on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce(Ended) + Send,
{
    let ScreenTarget::Display { id, width, height } = target else {
        let _ = open_result.send(Err(ScreenState::Unavailable));
        return;
    };
    autoreleasepool(|_| {
        let opened = match open(id, width, height, stop, Box::new(on_frame)) {
            Ok(opened) => opened,
            Err(state) => {
                let _ = open_result.send(Err(state));
                return;
            }
        };
        let _ = open_result.send(Ok(()));
        drop(open_result);
        let ended = loop {
            if stop.load(Ordering::Acquire) {
                break None;
            }
            match opened.ended.load(Ordering::Acquire) {
                SOURCE_GONE => break Some(Ended::Source),
                FAILED => break Some(Ended::Failed),
                _ => std::thread::sleep(POLL),
            }
        };
        // Release the display before the end is reported.
        drop(opened);
        if let Some(ended) = ended
            && !stop.load(Ordering::Acquire)
        {
            on_end(ended);
        }
    });
}

pub fn accepts(target: &ScreenTarget) -> bool {
    matches!(target, ScreenTarget::Display { .. })
}
