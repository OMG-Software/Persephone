//! The PipeWire camera backend (Linux, `--camera=portal`, ADR-0049).
//!
//! The core asked the camera portal for access and sent the PipeWire remote it
//! got (`OpenPipeWireRemote`) over the camera channel (descriptor 3). Each
//! listing and each capture takes the newest descriptor from the channel and
//! connects to it. That remote shows only the camera nodes the portal allows.
//! The worker never opens a device node or the session bus. A call without a
//! descriptor finds no camera.
//!
//! Only nodes of media class `Video/Source` count. The ID is the node's
//! `node.name`. Formats: NV12, I420 and YUY2 (YUYV) at most 1280x720;
//! no MJPEG. Buffers must be shared memory (`MemPtr` or `MemFd`): the stream
//! asks for nothing else, and a buffer of another type is refused. The
//! PipeWire loop of one connection runs on the calling thread.

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use persephone_call_protocol::CameraState;
use pipewire as pw;
use pw::main_loop::{MainLoopRc, MainLoopWeak};
use pw::properties::properties;
use pw::spa;
use spa::buffer::{ChunkFlags, DataType};
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle, SpaTypes};

use super::yuyv::{self, PixelFormat};
use super::{CapturedFrame, MAX_CAPTURE_SIDE};
use crate::video::pick_camera;

/// `SPA_LOG_LEVEL_NONE`: the library logs nothing (the worker writes no stderr).
const SPA_LOG_LEVEL_NONE: u32 = 0;
/// The longest wait for the remote to answer, and for a stream to start.
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(6);
/// How often a running capture looks at the stop flag.
const STOP_POLL: Duration = Duration::from_millis(100);
/// Camera nodes read from one remote.
const MAX_NODES: usize = 64;
/// Buffers asked for (range), and the widest picture row the worker takes.
const BUFFERS_MIN: i32 = 2;
const BUFFERS_DEFAULT: i32 = 4;
const BUFFERS_MAX: i32 = 8;
const MAX_STRIDE: u32 = 2 * MAX_CAPTURE_SIDE;

/// One camera node of the portal's remote.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Node {
    /// The global object ID in this connection.
    object: u32,
    /// `node.name`: the camera ID.
    name: String,
    /// `node.description`, else `node.nick`, else the name.
    description: String,
}

/// The node of a registry object that counts as a camera: a node with media
/// class `Video/Source` and a name. `get` reads a property.
fn camera_node<'a>(object: u32, get: impl Fn(&str) -> Option<&'a str>) -> Option<Node> {
    if get("media.class") != Some("Video/Source") {
        return None;
    }
    let name = get("node.name").filter(|name| !name.is_empty())?;
    let description = get("node.description")
        .or_else(|| get("node.nick"))
        .filter(|text| !text.is_empty())
        .unwrap_or(name);
    Some(Node {
        object,
        name: name.to_owned(),
        description: description.to_owned(),
    })
}

/// A PipeWire connection over the portal's descriptor.
struct Remote {
    mainloop: MainLoopRc,
    // Kept alive for the core.
    _context: pw::context::ContextRc,
    core: pw::core::CoreRc,
}

/// Connects over the newest descriptor of the camera channel.
fn connect() -> Result<Remote, CameraState> {
    let fd = crate::camera_channel::take_handle().ok_or(CameraState::Unavailable)?;
    pw::init();
    // SAFETY: sets the library's log level; no pointers are involved.
    unsafe { pw::sys::pw_log_set_level(SPA_LOG_LEVEL_NONE) };
    let mainloop = MainLoopRc::new(None).map_err(|_| CameraState::Unavailable)?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|_| CameraState::Unavailable)?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|_| CameraState::Unavailable)?;
    Ok(Remote {
        mainloop,
        _context: context,
        core,
    })
}

fn quit(mainloop: &MainLoopWeak) {
    if let Some(mainloop) = mainloop.upgrade() {
        mainloop.quit();
    }
}

/// Reads the camera nodes. Waits for the remote's answer to a sync request, so
/// every node that exists now has been announced.
fn enumerate(remote: &Remote) -> Result<Vec<Node>, CameraState> {
    let registry = remote
        .core
        .get_registry()
        .map_err(|_| CameraState::Unavailable)?;
    let nodes = Rc::new(RefCell::new(Vec::<Node>::new()));
    let seen = nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props else {
                return;
            };
            let Some(node) = camera_node(global.id, |key| props.get(key)) else {
                return;
            };
            let mut seen = seen.borrow_mut();
            if seen.len() < MAX_NODES {
                seen.push(node);
            }
        })
        .register();

    let done = Rc::new(Cell::new(false));
    let pending = remote.core.sync(0).map_err(|_| CameraState::Unavailable)?;
    let (done_flag, done_loop) = (done.clone(), remote.mainloop.downgrade());
    let error_loop = remote.mainloop.downgrade();
    let _core_listener = remote
        .core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_flag.set(true);
                quit(&done_loop);
            }
        })
        .error(move |_, _, _, _| quit(&error_loop))
        .register();
    let timeout_loop = remote.mainloop.downgrade();
    let timer = remote
        .mainloop
        .loop_()
        .add_timer(move |_| quit(&timeout_loop));
    let _ = timer.update_timer(Some(ROUNDTRIP_TIMEOUT), None);
    remote.mainloop.run();
    if !done.get() {
        return Err(CameraState::Unavailable);
    }
    Ok(nodes.take())
}

/// The cameras of the portal's remote, as (`node.name`, description). Empty
/// when no descriptor came from the core or the remote does not answer.
pub fn list() -> Vec<(String, String)> {
    let Ok(remote) = connect() else {
        return Vec::new();
    };
    let nodes = enumerate(&remote).unwrap_or_default();
    nodes
        .into_iter()
        .map(|node| (node.name, node.description))
        .collect()
}

/// The pixel format of a negotiated SPA video format.
fn from_spa(format: VideoFormat) -> Option<PixelFormat> {
    if format == VideoFormat::NV12 {
        Some(PixelFormat::Nv12)
    } else if format == VideoFormat::I420 {
        Some(PixelFormat::I420)
    } else if format == VideoFormat::YUY2 {
        Some(PixelFormat::Yuyv)
    } else {
        None
    }
}

fn to_spa(format: PixelFormat) -> VideoFormat {
    match format {
        PixelFormat::Nv12 => VideoFormat::NV12,
        PixelFormat::I420 => VideoFormat::I420,
        PixelFormat::Yuyv => VideoFormat::YUY2,
    }
}

/// The picture the stream agreed on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Negotiated {
    format: PixelFormat,
    width: u32,
    height: u32,
}

/// Accepts a negotiated format only when the policy accepts it.
fn accept(format: VideoFormat, size: Rectangle) -> Option<Negotiated> {
    let format = from_spa(format)?;
    let (width, height) = (size.width, size.height);
    let ok = yuyv::size_allowed(width, height)
        && (format != PixelFormat::Yuyv || width.is_multiple_of(2));
    ok.then_some(Negotiated {
        format,
        width,
        height,
    })
}

/// The bytes per row of the Y plane: the chunk's stride when it has one, else
/// the least a row needs. `None` when it is too short for the picture or too
/// wide.
fn row_stride(chunk_stride: i32, negotiated: Negotiated) -> Option<u32> {
    let least = match negotiated.format {
        PixelFormat::Yuyv => negotiated.width.checked_mul(2)?,
        PixelFormat::Nv12 | PixelFormat::I420 => negotiated.width,
    };
    let stride = if chunk_stride <= 0 {
        least
    } else {
        u32::try_from(chunk_stride).ok()?
    };
    (least..=MAX_STRIDE).contains(&stride).then_some(stride)
}

/// The valid bytes of a buffer: `size` bytes at `offset`, inside the mapped
/// area. `None` for an empty or overrunning chunk.
fn chunk_range(offset: u32, size: u32, mapped: usize) -> Option<std::ops::Range<usize>> {
    let start = offset as usize;
    let end = start.checked_add(size as usize)?;
    (size > 0 && end <= mapped).then_some(start..end)
}

fn pod_bytes(value: &Value) -> Option<Vec<u8>> {
    PodSerializer::serialize(Cursor::new(Vec::new()), value)
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}

/// The format request: raw video in NV12, I420 or YUY2, from 2x2 up to
/// 1280x720, any frame rate to 60.
fn format_pod() -> Option<Vec<u8>> {
    let id = |value: u32| Value::Id(Id(value));
    let formats = [PixelFormat::Nv12, PixelFormat::I420, PixelFormat::Yuyv];
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
                        default: Id(to_spa(formats[0]).as_raw()),
                        alternatives: formats.iter().map(|f| Id(to_spa(*f).as_raw())).collect(),
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle {
                            width: 640,
                            height: 360,
                        },
                        min: Rectangle {
                            width: 2,
                            height: 2,
                        },
                        max: Rectangle {
                            width: 1280,
                            height: 720,
                        },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 30, denom: 1 },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction { num: 60, denom: 1 },
                    },
                ))),
            ),
        ],
    };
    pod_bytes(&Value::Object(object))
}

/// The buffer request: one block, shared memory only (`MemPtr` or `MemFd`),
/// no DMA buffers.
fn buffers_pod() -> Option<Vec<u8>> {
    let shared_memory = (1_u32 << DataType::MemPtr.as_raw()) | (1_u32 << DataType::MemFd.as_raw());
    let object = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: BUFFERS_DEFAULT,
                        min: BUFFERS_MIN,
                        max: BUFFERS_MAX,
                    },
                ))),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: shared_memory as i32,
                        flags: Vec::new(),
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
    open_result: Option<Sender<Result<(), CameraState>>>,
    streaming: bool,
    /// The stream failed or ended after it had started.
    ended: bool,
}

impl Shared {
    fn report(&mut self, result: Result<(), CameraState>) {
        if let Some(sender) = self.open_result.take() {
            let _ = sender.send(result);
        }
    }
}

/// What the stream callbacks own.
struct StreamData<F> {
    shared: Rc<RefCell<Shared>>,
    mainloop: MainLoopWeak,
    negotiated: Option<Negotiated>,
    converted: Vec<u8>,
    on_frame: F,
}

impl<F> StreamData<F> {
    /// Ends the loop. Before the stream started, the opening fails.
    fn fail(&self) {
        {
            let mut shared = self.shared.borrow_mut();
            if shared.streaming {
                shared.ended = true;
            } else {
                shared.report(Err(CameraState::Unavailable));
            }
        }
        quit(&self.mainloop);
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
            return self.fail();
        }
        let Some(negotiated) = accept(info.format(), info.size()) else {
            return self.fail();
        };
        self.negotiated = Some(negotiated);
        // Shared memory only.
        if let Some(bytes) = buffers_pod()
            && let Some(pod) = Pod::from_bytes(&bytes)
        {
            let _ = stream.update_params(&mut [pod]);
        } else {
            self.fail();
        }
    }

    fn handle_process(&mut self, stream: &pw::stream::Stream) {
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
                return self.fail();
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
            yuyv::deliver_frame(
                negotiated.format,
                negotiated.width,
                negotiated.height,
                stride,
                &bytes[range],
                &mut self.converted,
                &mut self.on_frame,
            );
        }
    }
}

/// The thread body of a capture. Connects over the descriptor from the core,
/// finds the camera by `node.name` (`""`: the first one), reports the result
/// of opening once, then delivers pictures until `stop`, an error or the
/// camera's removal.
pub fn capture<F, E>(
    device_id: &str,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), CameraState>>,
    on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce() + Send,
{
    let shared = Rc::new(RefCell::new(Shared {
        open_result: Some(open_result),
        streaming: false,
        ended: false,
    }));
    run(device_id, stop, &shared, on_frame);
    let (ended, never_opened) = {
        let mut shared = shared.borrow_mut();
        let never_opened = shared.open_result.is_some();
        shared.report(Err(CameraState::Unavailable));
        (shared.ended, never_opened)
    };
    if ended && !never_opened && !stop.load(Ordering::Acquire) {
        on_end();
    }
}

fn run<F: FnMut(CapturedFrame<'_>) + 'static>(
    device_id: &str,
    stop: &Arc<AtomicBool>,
    shared: &Rc<RefCell<Shared>>,
    on_frame: F,
) {
    let Ok(remote) = connect() else {
        return;
    };
    let Ok(nodes) = enumerate(&remote) else {
        return;
    };
    let wanted = (!device_id.is_empty()).then_some(device_id);
    let picked = pick_camera(wanted, nodes.iter().map(|node| node.name.as_str()));
    let node = match picked {
        Ok(name) => nodes.iter().find(|node| node.name == name),
        Err(state) => {
            shared.borrow_mut().report(Err(state));
            return;
        }
    };
    let Some(node) = node else {
        return;
    };
    let Ok(stream) = pw::stream::StreamBox::new(
        &remote.core,
        "persephone-camera",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Camera",
        },
    ) else {
        return;
    };
    let data = StreamData {
        shared: shared.clone(),
        mainloop: remote.mainloop.downgrade(),
        negotiated: None,
        converted: Vec::new(),
        on_frame,
    };
    let Ok(_listener) = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, new| match new {
            pw::stream::StreamState::Streaming => {
                let mut shared = data.shared.borrow_mut();
                shared.streaming = true;
                shared.report(Ok(()));
            }
            pw::stream::StreamState::Error(_) => data.fail(),
            pw::stream::StreamState::Unconnected if data.shared.borrow().streaming => data.fail(),
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
    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    if stream
        .connect(Direction::Input, Some(node.object), flags, &mut [format])
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
    let _ = stream.disconnect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    /// GIVEN registry objects WHEN checked THEN only a named Video/Source
    /// node is a camera, and its description falls back to the nick and the
    /// name.
    #[test]
    fn given_registry_objects_then_only_video_sources_are_cameras() {
        let camera = [
            ("media.class", "Video/Source"),
            ("node.name", "v4l2_input.pci-0000_00_14.0-usb-0_1"),
            ("node.description", "Integrated Camera"),
        ];
        let node = camera_node(42, props(&camera)).expect("camera");
        assert_eq!(node.object, 42);
        assert_eq!(node.name, "v4l2_input.pci-0000_00_14.0-usb-0_1");
        assert_eq!(node.description, "Integrated Camera");

        let nick = [
            ("media.class", "Video/Source"),
            ("node.name", "cam"),
            ("node.nick", "Nick"),
        ];
        assert_eq!(
            camera_node(1, props(&nick)).expect("camera").description,
            "Nick"
        );
        let bare = [("media.class", "Video/Source"), ("node.name", "cam")];
        assert_eq!(
            camera_node(1, props(&bare)).expect("camera").description,
            "cam"
        );

        for other in [
            vec![("media.class", "Audio/Source"), ("node.name", "mic")],
            vec![("media.class", "Video/Sink"), ("node.name", "screen")],
            vec![("media.class", "Stream/Input/Video"), ("node.name", "app")],
            vec![("media.class", "Video/Source")],
            vec![("media.class", "Video/Source"), ("node.name", "")],
            vec![("node.name", "x")],
        ] {
            assert!(camera_node(1, props(&other)).is_none(), "{other:?}");
        }
    }

    /// GIVEN negotiated formats WHEN checked THEN only NV12, I420 and YUY2 up
    /// to 1280x720 pass, and an odd-width YUY2 does not.
    #[test]
    fn given_negotiated_formats_then_only_the_policy_ones_pass() {
        let size = |width, height| Rectangle { width, height };
        for (format, expected) in [
            (VideoFormat::NV12, PixelFormat::Nv12),
            (VideoFormat::I420, PixelFormat::I420),
            (VideoFormat::YUY2, PixelFormat::Yuyv),
        ] {
            assert_eq!(
                accept(format, size(1280, 720)),
                Some(Negotiated {
                    format: expected,
                    width: 1280,
                    height: 720
                })
            );
        }
        assert_eq!(accept(VideoFormat::NV12, size(1920, 1080)), None);
        assert_eq!(accept(VideoFormat::NV12, size(0, 0)), None);
        assert_eq!(accept(VideoFormat::YUY2, size(641, 360)), None);
        assert!(accept(VideoFormat::NV12, size(641, 360)).is_some());
        // Not accepted formats: RGB, MJPEG-like encoded, UYVY.
        assert_eq!(accept(VideoFormat::RGB, size(640, 360)), None);
        assert_eq!(accept(VideoFormat::Encoded, size(640, 360)), None);
        assert_eq!(accept(VideoFormat::UYVY, size(640, 360)), None);
    }

    /// GIVEN chunk strides WHEN checked THEN a missing stride becomes the least
    /// row length and a short or huge one is refused.
    #[test]
    fn given_chunk_strides_then_short_and_huge_ones_are_refused() {
        let nv12 = Negotiated {
            format: PixelFormat::Nv12,
            width: 640,
            height: 360,
        };
        let yuyv = Negotiated {
            format: PixelFormat::Yuyv,
            ..nv12
        };
        assert_eq!(row_stride(0, nv12), Some(640));
        assert_eq!(row_stride(-1, nv12), Some(640));
        assert_eq!(row_stride(704, nv12), Some(704));
        assert_eq!(row_stride(639, nv12), None);
        assert_eq!(row_stride(0, yuyv), Some(1280));
        assert_eq!(row_stride(1279, yuyv), None);
        assert_eq!(row_stride(MAX_STRIDE as i32 + 1, nv12), None);
        assert_eq!(row_stride(i32::MAX, nv12), None);
    }

    /// GIVEN chunk offsets and sizes WHEN checked THEN only a range inside the
    /// mapped area passes.
    #[test]
    fn given_chunks_then_only_ranges_inside_the_mapping_pass() {
        assert_eq!(chunk_range(0, 100, 100), Some(0..100));
        assert_eq!(chunk_range(16, 84, 100), Some(16..100));
        assert_eq!(chunk_range(0, 101, 100), None);
        assert_eq!(chunk_range(17, 84, 100), None);
        assert_eq!(chunk_range(0, 0, 100), None);
        assert_eq!(
            chunk_range(u32::MAX, u32::MAX, usize::MAX),
            Some(u32::MAX as usize..u32::MAX as usize * 2)
        );
        assert_eq!(chunk_range(u32::MAX, u32::MAX, 10), None);
    }

    /// GIVEN the request pods WHEN built THEN they serialise to well-formed
    /// objects, and the format request never names MJPEG or DMA buffers.
    #[test]
    fn given_request_pods_then_they_are_well_formed() {
        let format = format_pod().expect("format pod");
        let pod = Pod::from_bytes(&format).expect("pod");
        assert!(pod.is_object());
        let buffers = buffers_pod().expect("buffers pod");
        let pod = Pod::from_bytes(&buffers).expect("pod");
        assert!(pod.is_object());
        // The data-type mask holds MemPtr and MemFd and not DmaBuf.
        let mask = (1_u32 << DataType::MemPtr.as_raw()) | (1_u32 << DataType::MemFd.as_raw());
        assert_eq!(mask & (1_u32 << DataType::DmaBuf.as_raw()), 0);
    }

    /// GIVEN no descriptor from the core WHEN cameras are listed or a capture
    /// starts THEN there are none and the capture reports Unavailable.
    #[test]
    fn given_no_handle_then_no_cameras_and_unavailable() {
        assert!(list().is_empty());
        let (tx, rx) = std::sync::mpsc::channel();
        capture("cam", &Arc::new(AtomicBool::new(false)), tx, |_| {}, || {});
        assert_eq!(rx.recv().expect("result"), Err(CameraState::Unavailable));
    }
}
