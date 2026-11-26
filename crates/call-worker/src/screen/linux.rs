//! The Linux screen backend (ADR-0053): PipeWire over the ScreenCast portal.
//!
//! The core ran the portal (`CreateSession`, `SelectSources`, `Start`,
//! `OpenPipeWireRemote`), keeps the portal session open for the whole share,
//! and sent the PipeWire remote over the camera channel with kind `s`. The
//! command carries the node ID the portal named. The worker connects over
//! the remote, reads that one node, and never talks to D-Bus. The remote
//! shows only what the user chose in the portal's own picker.
//!
//! Formats: BGRx, RGBx, BGRA and RGBA; others are refused. Buffers must be
//! shared memory (`MemPtr` or `MemFd`): the stream asks for nothing else,
//! and a buffer of another type is refused. The PipeWire loop of one
//! connection runs on the calling thread. The code shares the connection and
//! pod helpers of the portal camera (`camera/linux_portal.rs`).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use persephone_call_protocol::{ScreenState, ScreenTarget};
use pipewire as pw;
use pw::main_loop::MainLoopWeak;
use pw::properties::properties;
use pw::spa;
use spa::buffer::{ChunkFlags, DataType};
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle, SpaTypes};

use super::{CapturedFrame, Ended, Format, Grant, capture_size_ok};
use crate::camera::linux_portal::{
    STOP_POLL, STREAM_START_TIMEOUT, buffers_pod, chunk_range, connect, pod_bytes, quit,
};

/// The widest picture row the worker takes, in bytes.
const MAX_STRIDE: u32 = 4 * super::MAX_CAPTURE_SIDE;

pub fn accepts(target: &ScreenTarget) -> bool {
    matches!(target, ScreenTarget::Portal { .. })
}

/// The pixel format of a negotiated SPA video format.
fn from_spa(format: VideoFormat) -> Option<Format> {
    if format == VideoFormat::BGRx || format == VideoFormat::BGRA {
        Some(Format::Bgra)
    } else if format == VideoFormat::RGBx || format == VideoFormat::RGBA {
        Some(Format::Rgba)
    } else {
        None
    }
}

/// The picture the stream agreed on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Negotiated {
    format: Format,
    width: u32,
    height: u32,
}

/// Accepts a negotiated format only when the policy accepts it.
fn accept(format: VideoFormat, size: Rectangle) -> Option<Negotiated> {
    let format = from_spa(format)?;
    capture_size_ok(size.width, size.height).then_some(Negotiated {
        format,
        width: size.width,
        height: size.height,
    })
}

/// The bytes per row: the chunk's stride when it has one, else the least a
/// row needs. `None` when it is too short for the picture or too wide.
fn row_stride(chunk_stride: i32, negotiated: Negotiated) -> Option<u32> {
    let least = negotiated.width.checked_mul(4)?;
    let stride = if chunk_stride <= 0 {
        least
    } else {
        u32::try_from(chunk_stride).ok()?
    };
    (least..=MAX_STRIDE).contains(&stride).then_some(stride)
}

/// The capture stream's properties. The stream links only to the chosen
/// node (the connect call names its ID) and never reconnects to another.
fn stream_properties() -> pw::properties::PropertiesBox {
    properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_DONT_RECONNECT => "true",
        "node.dont-fallback" => "true",
    }
}

/// The capture stream's connect flags: linked to the target at once, mapped
/// buffers, and no reconnection to another node.
fn stream_flags() -> pw::stream::StreamFlags {
    pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::DONT_RECONNECT
}

/// The format request: raw video in BGRx, RGBx, BGRA or RGBA, from 2x2 up
/// to the capture limit, any frame rate up to 30.
fn format_pod() -> Option<Vec<u8>> {
    let id = |value: u32| Value::Id(Id(value));
    let formats = [
        VideoFormat::BGRx,
        VideoFormat::BGRA,
        VideoFormat::RGBx,
        VideoFormat::RGBA,
    ];
    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.as_raw(),
                id(MediaType::Video.as_raw()),
            ),
            Property::new(
                FormatProperties::MediaSubtype.as_raw(),
                id(MediaSubtype::Raw.as_raw()),
            ),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(formats[0].as_raw()),
                        alternatives: formats.iter().map(|f| Id(f.as_raw())).collect(),
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle {
                            width: 1920,
                            height: 1080,
                        },
                        min: Rectangle {
                            width: 2,
                            height: 2,
                        },
                        max: Rectangle {
                            width: super::MAX_CAPTURE_SIDE,
                            height: super::MAX_CAPTURE_SIDE,
                        },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 10, denom: 1 },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction { num: 30, denom: 1 },
                    },
                ))),
            ),
        ],
    };
    pod_bytes(&Value::Object(object))
}

/// State shared by the callbacks of one capture, which all run on the loop's
/// thread.
struct Shared {
    open_result: Option<Sender<Result<(), ScreenState>>>,
    streaming: bool,
    /// How the stream ended after it had started.
    ended: Option<Ended>,
    /// The capture loop has ended; later callbacks change nothing.
    closed: bool,
}

impl Shared {
    fn report(&mut self, result: Result<(), ScreenState>) {
        if let Some(sender) = self.open_result.take() {
            let _ = sender.send(result);
        }
    }

    /// The stream reached `Streaming`.
    fn stream_started(&mut self) {
        if !self.closed {
            self.streaming = true;
            self.report(Ok(()));
        }
    }

    /// The stream failed or ended. A started stream records how.
    fn stream_ended(&mut self, how: Ended) {
        if self.streaming && self.ended.is_none() {
            self.ended = Some(how);
        }
    }
}

/// What the stream callbacks own.
struct StreamData<F> {
    shared: Rc<RefCell<Shared>>,
    mainloop: MainLoopWeak,
    negotiated: Option<Negotiated>,
    on_frame: F,
}

impl<F> StreamData<F> {
    /// Ends the loop. The opening fails, or the started stream ends.
    fn fail(&self, how: Ended) {
        let closed = {
            let mut shared = self.shared.borrow_mut();
            shared.stream_ended(how);
            shared.closed
        };
        if !closed {
            quit(&self.mainloop);
        }
    }
}

impl<F: FnMut(CapturedFrame<'_>)> StreamData<F> {
    fn handle_format(&mut self, stream: &pw::stream::Stream, param: &Pod) {
        let Ok((media_type, subtype)) = spa::param::format_utils::parse_format(param) else {
            return;
        };
        if media_type != MediaType::Video || subtype != MediaSubtype::Raw {
            return;
        }
        let mut info = VideoInfoRaw::new();
        if info.parse(param).is_err() {
            return self.fail(Ended::Failed);
        }
        let Some(negotiated) = accept(info.format(), info.size()) else {
            return self.fail(Ended::Failed);
        };
        self.negotiated = Some(negotiated);
        // Shared memory only.
        if let Some(bytes) = buffers_pod()
            && let Some(pod) = Pod::from_bytes(&bytes)
        {
            let _ = stream.update_params(&mut [pod]);
        } else {
            self.fail(Ended::Failed);
        }
    }

    fn handle_process(&mut self, stream: &pw::stream::Stream) {
        if self.shared.borrow().closed {
            return;
        }
        while let Some(mut buffer) = stream.dequeue_buffer() {
            let Some(negotiated) = self.negotiated else {
                continue;
            };
            let datas = buffer.datas_mut();
            let [data] = datas else {
                continue;
            };
            if data.type_() != DataType::MemPtr && data.type_() != DataType::MemFd {
                // A buffer of another type is refused, not read.
                return self.fail(Ended::Failed);
            }
            let (offset, size, stride, flags) = {
                let chunk = data.chunk();
                (chunk.offset(), chunk.size(), chunk.stride(), chunk.flags())
            };
            if flags.contains(ChunkFlags::CORRUPTED) {
                continue;
            }
            let Some(stride) = row_stride(stride, negotiated) else {
                continue;
            };
            let Some(bytes) = data.data() else {
                continue;
            };
            let Some(range) = chunk_range(offset, size, bytes.len()) else {
                continue;
            };
            if let Some(frame) = CapturedFrame::checked(
                negotiated.format,
                negotiated.width,
                negotiated.height,
                stride,
                &bytes[range],
            ) {
                (self.on_frame)(frame);
            }
        }
    }
}

/// The thread body of a capture. Connects over the descriptor from the core,
/// reports the result of opening once, then delivers pictures until `stop`,
/// an error or the end of the portal's stream.
pub fn capture<F, E>(
    target: ScreenTarget,
    grant: Grant,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), ScreenState>>,
    on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce(Ended) + Send,
{
    let ScreenTarget::Portal { node } = target else {
        let _ = open_result.send(Err(ScreenState::Unavailable));
        return;
    };
    let shared = Rc::new(RefCell::new(Shared {
        open_result: Some(open_result),
        streaming: false,
        ended: None,
        closed: false,
    }));
    run(node, grant.remote, stop, &shared, on_frame);
    let ended = {
        let mut shared = shared.borrow_mut();
        // An open that was never reported fails now. A remote without the
        // node, or a portal that refused, shows as a stream that never starts.
        shared.report(Err(ScreenState::Unavailable));
        shared.ended
    };
    if let Some(how) = ended
        && !stop.load(Ordering::Acquire)
    {
        on_end(how);
    }
}

fn run<F: FnMut(CapturedFrame<'_>) + 'static>(
    node: u32,
    remote: Option<std::os::fd::OwnedFd>,
    stop: &Arc<AtomicBool>,
    shared: &Rc<RefCell<Shared>>,
    on_frame: F,
) {
    let Ok(remote) = connect(remote) else {
        return;
    };
    let Ok(stream) =
        pw::stream::StreamBox::new(&remote.core, "persephone-screen", stream_properties())
    else {
        return;
    };
    let data = StreamData {
        shared: shared.clone(),
        mainloop: remote.mainloop.downgrade(),
        negotiated: None,
        on_frame,
    };
    let Ok(_listener) = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, new| match new {
            pw::stream::StreamState::Streaming => data.shared.borrow_mut().stream_started(),
            pw::stream::StreamState::Error(_) => data.fail(Ended::Failed),
            // The portal session closed or the node went: the source ended.
            pw::stream::StreamState::Unconnected if data.shared.borrow().streaming => {
                data.fail(Ended::Source);
            }
            _ => {}
        })
        .param_changed(|stream, data, id, param| {
            if id == ParamType::Format.as_raw()
                && let Some(param) = param
            {
                data.handle_format(stream, param);
            }
        })
        .process(|stream, data| data.handle_process(stream))
        .register()
    else {
        return;
    };
    let Some(format) = format_pod() else {
        return;
    };
    let Some(format) = Pod::from_bytes(&format) else {
        return;
    };
    if stream
        .connect(Direction::Input, Some(node), stream_flags(), &mut [format])
        .is_err()
    {
        return;
    }

    // One timer ends the loop on a stop request or when the stream does not
    // start in time.
    let started = Instant::now();
    let (timer_stop, timer_shared) = (stop.clone(), shared.clone());
    let timer_loop = remote.mainloop.downgrade();
    let timer = remote.mainloop.loop_().add_timer(move |_| {
        let waited_too_long = {
            let shared = timer_shared.borrow();
            !shared.streaming && started.elapsed() > STREAM_START_TIMEOUT
        };
        if timer_stop.load(Ordering::Acquire) || waited_too_long {
            quit(&timer_loop);
        }
    });
    let _ = timer.update_timer(Some(STOP_POLL), Some(STOP_POLL));
    remote.mainloop.run();
    shared.borrow_mut().closed = true;
    let _ = stream.disconnect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Rectangle {
        Rectangle { width, height }
    }

    /// GIVEN each pixel format a portal may offer WHEN it is negotiated THEN
    /// the four 32-bit formats pass with their byte order and others do not.
    #[test]
    fn given_pixel_formats_when_negotiated_then_only_the_four_pass() {
        for (format, want) in [
            (VideoFormat::BGRx, Format::Bgra),
            (VideoFormat::BGRA, Format::Bgra),
            (VideoFormat::RGBx, Format::Rgba),
            (VideoFormat::RGBA, Format::Rgba),
        ] {
            let got = accept(format, size(1920, 1080)).expect("accepted");
            assert_eq!((got.format, got.width, got.height), (want, 1920, 1080));
        }
        for format in [
            VideoFormat::NV12,
            VideoFormat::I420,
            VideoFormat::YUY2,
            VideoFormat::RGB,
        ] {
            assert!(accept(format, size(1920, 1080)).is_none());
        }
    }

    /// GIVEN sizes over the capture limit WHEN negotiated THEN they are
    /// refused.
    #[test]
    fn given_oversize_screens_when_negotiated_then_they_are_refused() {
        assert!(accept(VideoFormat::BGRx, size(3840, 2160)).is_some());
        assert!(accept(VideoFormat::BGRx, size(5120, 2880)).is_none());
        assert!(accept(VideoFormat::BGRx, size(7681, 100)).is_none());
        assert!(accept(VideoFormat::BGRx, size(1, 1)).is_none());
    }

    /// GIVEN a chunk stride WHEN checked THEN a missing one means tight rows
    /// and one shorter than a row, or wider than the limit, is refused.
    #[test]
    fn given_chunk_strides_when_checked_then_short_and_wide_rows_are_refused() {
        let negotiated = accept(VideoFormat::BGRx, size(1920, 1080)).unwrap();
        assert_eq!(row_stride(0, negotiated), Some(7680));
        assert_eq!(row_stride(-4, negotiated), Some(7680));
        assert_eq!(row_stride(8192, negotiated), Some(8192));
        assert_eq!(row_stride(7676, negotiated), None);
        assert_eq!(row_stride(i32::MAX, negotiated), None);
    }

    /// GIVEN the capture stream's setup THEN it never reconnects or falls
    /// back to another node, and it links at once with mapped buffers.
    #[test]
    fn given_the_capture_stream_then_it_cannot_fall_back_to_another_node() {
        let properties = stream_properties();
        assert_eq!(properties.get("node.dont-fallback"), Some("true"));
        assert_eq!(properties.get("node.dont-reconnect"), Some("true"));
        let flags = stream_flags();
        assert!(flags.contains(pw::stream::StreamFlags::DONT_RECONNECT));
        assert!(flags.contains(pw::stream::StreamFlags::AUTOCONNECT));
        assert!(flags.contains(pw::stream::StreamFlags::MAP_BUFFERS));
    }

    /// GIVEN a started stream WHEN it ends by the source or by failure THEN
    /// the first reason is kept; before the start nothing is recorded.
    #[test]
    fn given_stream_ends_when_recorded_then_the_first_reason_is_kept() {
        let mut shared = Shared {
            open_result: None,
            streaming: false,
            ended: None,
            closed: false,
        };
        shared.stream_ended(Ended::Failed);
        assert_eq!(shared.ended, None);
        shared.stream_started();
        shared.stream_ended(Ended::Source);
        shared.stream_ended(Ended::Failed);
        assert_eq!(shared.ended, Some(Ended::Source));
    }

    /// GIVEN a remote of the wrong target kind WHEN a capture starts THEN it
    /// is unavailable.
    #[test]
    fn given_a_monitor_target_when_captured_on_linux_then_it_is_unavailable() {
        assert!(!accepts(&ScreenTarget::Monitor {
            name: persephone_call_protocol::MonitorName::new(br"\\.\DISPLAY1").expect("a name"),
            width: 1920,
            height: 1080,
            rect: persephone_call_protocol::DesktopRect::new(0, 0, 1920, 1080)
                .expect("a rectangle"),
        }));
        assert!(!accepts(&ScreenTarget::Display {
            id: 1,
            width: 1920,
            height: 1080
        }));
        assert!(accepts(&ScreenTarget::Portal { node: 5 }));
    }
}
