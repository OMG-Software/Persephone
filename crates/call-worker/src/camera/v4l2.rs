//! The V4L2 camera backend (Linux, `--camera=v4l2`, ADR-0049). The worker
//! opens the `/dev/video*` nodes itself. Landlock allows only the nodes that
//! existed when stage 1 ran, so the worker probes `/dev/video0` up to
//! `/dev/video63` by path and never lists `/dev`.
//!
//! Raw ioctls through `libc`, no V4L2 crate. A node is a camera when it has
//! the single-plane video-capture and streaming capabilities; metadata and
//! output nodes are skipped. The ID joins the node's driver, card (model
//! name) and bus information (port), so another camera model in the same
//! port has another ID and a saved choice finds nothing (`NotFound`). Two
//! cameras of the same model in the same port have the same ID: these ioctls
//! report no serial number. Formats:
//! NV12, YU12 (I420) and YUYV at most 2560x1440 (ADR-0070); no MJPEG. Buffers are mapped
//! read-only (`MMAP` streaming). Opening a node is the only side effect of
//! listing: nothing streams.
//!
//! The struct layouts are those of the 64-bit Linux ABI (x86_64 and aarch64);
//! the tests check their sizes and the ioctl numbers.

use std::ffi::CString;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use persephone_call_protocol::CameraState;

use super::CapturedFrame;
use super::yuyv::{self, Offer, PixelFormat};
use crate::containment::MAX_VIDEO_NODES;
use crate::send_tier::SendTier;

// --- The kernel ABI --------------------------------------------------------

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_FLAG_ERROR: u32 = 0x0000_0040;
const FRMSIZE_DISCRETE: u32 = 1;
const FRMIVAL_DISCRETE: u32 = 1;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const FOURCC_YUYV: u32 = fourcc(b'Y', b'U', b'Y', b'V');
const FOURCC_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
const FOURCC_YU12: u32 = fourcc(b'Y', b'U', b'1', b'2');

#[repr(C)]
#[allow(dead_code)] // kernel layout; some fields are never read
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[allow(dead_code)]
struct FmtDesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[allow(dead_code)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    /// The union: discrete uses the first two words (width, height); step-wise
    /// uses all six (min and max width, step, min and max height, step).
    size: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_frmivalenum`: the frame intervals of one format and size.
#[repr(C)]
#[allow(dead_code)]
struct FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    type_: u32,
    /// The union: discrete uses the first two words (numerator, denominator
    /// of one interval); step-wise uses all six (the least interval, the
    /// largest and the step, each as numerator and denominator).
    interval: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_streamparm` for a capture stream: the frame interval is
/// `numerator / denominator` seconds. The rest of the union is not used.
#[repr(C)]
#[allow(dead_code)]
struct StreamParm {
    type_: u32,
    capability: u32,
    capturemode: u32,
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
    rest: [u8; 160],
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format` on 64-bit: the union starts at offset 8.
#[repr(C)]
#[allow(dead_code)]
struct V4l2Format {
    type_: u32,
    _pad: u32,
    pix: PixFormat,
    rest: [u8; 152],
}

#[repr(C)]
#[allow(dead_code)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_buffer` on 64-bit Linux.
#[repr(C)]
#[allow(dead_code)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _pad0: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    /// The union `m`; `offset` is its first 32 bits (little endian).
    m: u64,
    length: u32,
    reserved2: u32,
    request_fd: i32,
    _pad1: u32,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<FmtDesc>() == 64);
    assert!(size_of::<FrmSizeEnum>() == 44);
    assert!(size_of::<FrmIvalEnum>() == 52);
    assert!(size_of::<StreamParm>() == 204);
    assert!(size_of::<V4l2Format>() == 208);
    assert!(size_of::<PixFormat>() == 48);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Buffer>() == 88);
};

/// `_IOC` for the Linux generic ioctl layout (x86_64 and aarch64).
const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr
}
const fn ior(nr: u32, size: usize) -> u32 {
    ioc(2, nr, size)
}
const fn iow(nr: u32, size: usize) -> u32 {
    ioc(1, nr, size)
}
const fn iowr(nr: u32, size: usize) -> u32 {
    ioc(3, nr, size)
}

const VIDIOC_QUERYCAP: u32 = ior(0, size_of::<Capability>());
const VIDIOC_ENUM_FMT: u32 = iowr(2, size_of::<FmtDesc>());
const VIDIOC_S_FMT: u32 = iowr(5, size_of::<V4l2Format>());
const VIDIOC_S_PARM: u32 = iowr(22, size_of::<StreamParm>());
const VIDIOC_REQBUFS: u32 = iowr(8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: u32 = iowr(9, size_of::<Buffer>());
const VIDIOC_QBUF: u32 = iowr(15, size_of::<Buffer>());
const VIDIOC_DQBUF: u32 = iowr(17, size_of::<Buffer>());
const VIDIOC_STREAMON: u32 = iow(18, size_of::<i32>());
const VIDIOC_STREAMOFF: u32 = iow(19, size_of::<i32>());
const VIDIOC_ENUM_FRAMESIZES: u32 = iowr(74, size_of::<FrmSizeEnum>());
const VIDIOC_ENUM_FRAMEINTERVALS: u32 = iowr(75, size_of::<FrmIvalEnum>());

// --- Limits ----------------------------------------------------------------

/// Formats and sizes read per camera, and offers kept.
const MAX_FORMATS: u32 = 32;
const MAX_SIZES: u32 = 64;
const MAX_OFFERS: usize = 256;
/// Capture buffers: asked for, and the most the driver may give.
const WANTED_BUFFERS: u32 = 4;
const MAX_BUFFERS: u32 = 16;
/// The largest buffer the worker maps. A 2560x1440 YUYV picture is 7.4 MB.
/// The driver gives at most [`MAX_BUFFERS`] of them, so the mappings stay
/// under 256 MB; a mapping is address space, not memory the worker fills.
const MAX_BUFFER_BYTES: u32 = 16 * 1024 * 1024;
/// How long one wait for a picture lasts, so a stop is seen quickly.
const POLL_MILLIS: i32 = 100;

/// Intervals read for one format and size.
const MAX_INTERVALS: u32 = 16;
/// The most bytes in one row that the worker takes: YUYV at the widest size,
/// with room for the driver's padding.
const MAX_STRIDE: u32 = 2 * yuyv::MAX_LONG_SIDE * 2;

/// Sizes tried on a camera that gives a range instead of a list.
const STANDARD_SIZES: [(u32, u32); 6] = [
    (640, 360),
    (640, 480),
    (1280, 720),
    (320, 240),
    (1920, 1080),
    (2560, 1440),
];

// --- Pure helpers ----------------------------------------------------------

/// Whether a node is a camera for the worker: single-plane video capture with
/// streaming. `device_caps` describes this node when the driver says so;
/// `capabilities` describes the whole device (metadata and output nodes of a
/// camera device share it).
fn is_capture_node(capabilities: u32, device_caps: u32) -> bool {
    let caps = if capabilities & CAP_DEVICE_CAPS != 0 {
        device_caps
    } else {
        capabilities
    };
    caps & CAP_VIDEO_CAPTURE != 0 && caps & CAP_STREAMING != 0
}

/// Text from a NUL-terminated fixed field: up to the first NUL, valid UTF-8,
/// no control character. `None` when it is empty or not clean.
fn field_text(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text = std::str::from_utf8(&bytes[..end]).ok()?;
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_owned())
}

fn pixel_format(fourcc: u32) -> Option<PixelFormat> {
    match fourcc {
        FOURCC_NV12 => Some(PixelFormat::Nv12),
        FOURCC_YU12 => Some(PixelFormat::I420),
        FOURCC_YUYV => Some(PixelFormat::Yuyv),
        _ => None,
    }
}

fn fourcc_of(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Nv12 => FOURCC_NV12,
        PixelFormat::I420 => FOURCC_YU12,
        PixelFormat::Yuyv => FOURCC_YUYV,
    }
}

/// The least bytes a row of the Y plane (or of YUYV) needs.
fn min_row_bytes(format: PixelFormat, width: u32) -> Option<u32> {
    match format {
        PixelFormat::Yuyv => width.checked_mul(2),
        PixelFormat::Nv12 | PixelFormat::I420 => Some(width),
    }
}

/// The standard sizes that a step-wise or continuous range holds. `step` 0
/// means any value.
fn range_sizes(size: [u32; 6]) -> Vec<(u32, u32)> {
    let [min_w, max_w, step_w, min_h, max_h, step_h] = size;
    let fits = |value: u32, min: u32, max: u32, step: u32| {
        value >= min && value <= max && (step == 0 || (value - min).is_multiple_of(step))
    };
    STANDARD_SIZES
        .into_iter()
        .filter(|&(w, h)| fits(w, min_w, max_w, step_w) && fits(h, min_h, max_h, step_h))
        .collect()
}

// --- System calls ------------------------------------------------------------

fn ioctl<T>(fd: RawFd, request: u32, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` is a valid, exclusive pointer to the struct that the
        // request number describes, and it outlives the call. `fd` is open.
        let result = unsafe { libc::ioctl(fd, request as _, std::ptr::from_mut(arg)) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn zeroed_struct<T>() -> T {
    // SAFETY: every struct of this module is plain integers and arrays of
    // integers, for which all-zero bytes are a valid value.
    unsafe { zeroed() }
}

/// Opens `/dev/video<index>` read-write and non-blocking. `None` when the node
/// is absent or not allowed.
fn open_node(index: u32) -> Option<OwnedFd> {
    let path = CString::new(format!("/dev/video{index}")).ok()?;
    // SAFETY: `path` is a NUL-terminated string.
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: `open` returned a new descriptor that nothing else owns.
    Some(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// The ID ([`camera_id`]) and name (card) of a capture node. A node without
/// a driver name, card or bus information is not listed: each field is part
/// of the ID, so an empty one would let other devices match it.
fn identify(fd: RawFd) -> Option<(String, String)> {
    let mut capability: Capability = zeroed_struct();
    ioctl(fd, VIDIOC_QUERYCAP, &mut capability).ok()?;
    if !is_capture_node(capability.capabilities, capability.device_caps) {
        return None;
    }
    let driver = field_text(&capability.driver)?;
    let bus_info = field_text(&capability.bus_info)?;
    let name = field_text(&capability.card)?;
    Some((camera_id(&driver, &name, &bus_info), name))
}

/// The camera ID: driver, card and bus information, in that order, joined by
/// `|`. A `\` or `|` inside a field is escaped with `\`, so two different
/// field sets never give the same ID. The bus information alone names a
/// port, not a device; with the driver and the card, a different camera
/// model plugged into the same port does not match a saved choice.
fn camera_id(driver: &str, card: &str, bus_info: &str) -> String {
    let escape = |field: &str| field.replace('\\', "\\\\").replace('|', "\\|");
    format!("{}|{}|{}", escape(driver), escape(card), escape(bus_info))
}

/// The capture nodes, as (ID, name). Streams nothing.
pub fn list() -> Vec<(String, String)> {
    (0..MAX_VIDEO_NODES)
        .filter_map(open_node)
        .filter_map(|fd| identify(fd.as_raw_fd()))
        .collect()
}

/// The formats and sizes a camera offers.
fn enumerate(fd: RawFd) -> Vec<Offer> {
    let mut offers = Vec::new();
    for format_index in 0..MAX_FORMATS {
        let mut desc: FmtDesc = zeroed_struct();
        desc.index = format_index;
        desc.type_ = BUF_TYPE_VIDEO_CAPTURE;
        if ioctl(fd, VIDIOC_ENUM_FMT, &mut desc).is_err() {
            break;
        }
        let Some(format) = pixel_format(desc.pixelformat) else {
            continue;
        };
        for size_index in 0..MAX_SIZES {
            let mut size: FrmSizeEnum = zeroed_struct();
            size.index = size_index;
            size.pixel_format = desc.pixelformat;
            if ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut size).is_err() {
                break;
            }
            let sizes = if size.type_ == FRMSIZE_DISCRETE {
                vec![(size.size[0], size.size[1])]
            } else {
                range_sizes(size.size)
            };
            for (width, height) in sizes {
                offers.push(Offer {
                    format,
                    width,
                    height,
                    fps: max_frame_rate(fd, desc.pixelformat, width, height),
                });
            }
            if size.type_ != FRMSIZE_DISCRETE || offers.len() >= MAX_OFFERS {
                break;
            }
        }
        if offers.len() >= MAX_OFFERS {
            break;
        }
    }
    offers
}

/// The fastest frame rate of a format and size, in frames per second rounded
/// down, or 0 when the driver does not say. A list of intervals gives its
/// shortest one; a range gives its least one.
fn max_frame_rate(fd: RawFd, pixel_format: u32, width: u32, height: u32) -> u32 {
    let mut fastest = 0;
    for index in 0..MAX_INTERVALS {
        let mut interval: FrmIvalEnum = zeroed_struct();
        interval.index = index;
        interval.pixel_format = pixel_format;
        interval.width = width;
        interval.height = height;
        if ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut interval).is_err() {
            break;
        }
        let [numerator, denominator, ..] = interval.interval;
        fastest = fastest.max(rate_of(numerator, denominator));
        if interval.type_ != FRMIVAL_DISCRETE {
            break;
        }
    }
    fastest
}

/// Frames per second, rounded down, of an interval of `numerator` over
/// `denominator` seconds. 0 for an interval of 0 seconds or no time base.
fn rate_of(numerator: u32, denominator: u32) -> u32 {
    denominator.checked_div(numerator).unwrap_or(0)
}

/// Asks for `fps` frames per second, at most 30. Does nothing for an unknown
/// rate. The answer is not checked: the stream runs at the interval of the
/// driver when it refuses.
fn set_frame_rate(fd: RawFd, fps: u32) {
    if fps == 0 {
        return;
    }
    let mut parm: StreamParm = zeroed_struct();
    parm.type_ = BUF_TYPE_VIDEO_CAPTURE;
    parm.numerator = 1;
    parm.denominator = fps.min(30);
    let _ = ioctl(fd, VIDIOC_S_PARM, &mut parm);
}

/// A read-only mapping of one driver buffer.
struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` came from a successful `mmap` that nothing
        // else unmaps.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// A running stream. Drop order matters: the stream stops, the mappings go,
/// then the descriptor closes.
struct Stream {
    on: bool,
    maps: Vec<Mapped>,
    fd: OwnedFd,
    format: PixelFormat,
    width: u32,
    height: u32,
    stride: u32,
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.on {
            let mut kind = BUF_TYPE_VIDEO_CAPTURE as i32;
            let _ = ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut kind);
        }
    }
}

fn queue(fd: RawFd, index: u32) -> io::Result<()> {
    let mut buffer: Buffer = zeroed_struct();
    buffer.type_ = BUF_TYPE_VIDEO_CAPTURE;
    buffer.memory = MEMORY_MMAP;
    buffer.index = index;
    ioctl(fd, VIDIOC_QBUF, &mut buffer)
}

/// Sets the format, maps the buffers and starts streaming.
fn start_stream(fd: OwnedFd, target: SendTier) -> Result<Stream, CameraState> {
    let raw = fd.as_raw_fd();
    let offers = enumerate(raw);
    let chosen = yuyv::choose(&offers, target).ok_or(CameraState::Unavailable)?;
    let offer = offers[chosen];
    let mut format: V4l2Format = zeroed_struct();
    format.type_ = BUF_TYPE_VIDEO_CAPTURE;
    format.pix.width = offer.width;
    format.pix.height = offer.height;
    format.pix.pixelformat = fourcc_of(offer.format);
    format.pix.field = 1; // V4L2_FIELD_NONE
    ioctl(raw, VIDIOC_S_FMT, &mut format).map_err(|_| CameraState::Unavailable)?;
    // Ask for the frame rate that the offer names, so the mode that was
    // chosen for its speed runs at it. The driver may refuse: it then keeps
    // its own interval.
    set_frame_rate(raw, offer.fps);
    // The driver may change the request; accept only what the policy accepts.
    let (width, height) = (format.pix.width, format.pix.height);
    let negotiated = pixel_format(format.pix.pixelformat).ok_or(CameraState::Unavailable)?;
    let row = min_row_bytes(negotiated, width).ok_or(CameraState::Unavailable)?;
    let stride = if format.pix.bytesperline == 0 {
        row
    } else {
        format.pix.bytesperline
    };
    if !yuyv::format_allowed(negotiated, width, height) || stride < row || stride > MAX_STRIDE {
        return Err(CameraState::Unavailable);
    }

    let mut request: RequestBuffers = zeroed_struct();
    request.count = WANTED_BUFFERS;
    request.type_ = BUF_TYPE_VIDEO_CAPTURE;
    request.memory = MEMORY_MMAP;
    ioctl(raw, VIDIOC_REQBUFS, &mut request).map_err(|_| CameraState::Unavailable)?;
    if request.count < 2 || request.count > MAX_BUFFERS {
        return Err(CameraState::Unavailable);
    }
    let mut stream = Stream {
        on: false,
        maps: Vec::new(),
        fd,
        format: negotiated,
        width,
        height,
        stride,
    };
    for index in 0..request.count {
        let mut buffer: Buffer = zeroed_struct();
        buffer.type_ = BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = MEMORY_MMAP;
        buffer.index = index;
        ioctl(raw, VIDIOC_QUERYBUF, &mut buffer).map_err(|_| CameraState::Unavailable)?;
        if buffer.length == 0 || buffer.length > MAX_BUFFER_BYTES {
            return Err(CameraState::Unavailable);
        }
        let len = buffer.length as usize;
        // The union's first 32 bits are the offset on a little-endian kernel.
        let offset = buffer.m as u32;
        // SAFETY: a read-only shared mapping of `len` bytes of the driver's
        // buffer at the offset the driver gave. The kernel checks both.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                raw,
                libc::off_t::from(offset),
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(CameraState::Unavailable);
        }
        stream.maps.push(Mapped { ptr, len });
    }
    for index in 0..request.count {
        queue(raw, index).map_err(|_| CameraState::Unavailable)?;
    }
    let mut kind = BUF_TYPE_VIDEO_CAPTURE as i32;
    ioctl(raw, VIDIOC_STREAMON, &mut kind).map_err(|_| CameraState::Unavailable)?;
    stream.on = true;
    Ok(stream)
}

/// The thread body of a capture. Reports the result of opening once, then
/// delivers pictures until `stop`, an error or an unplugged camera.
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
    let stream = match open(device_id, target) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = open_result.send(Err(error));
            return;
        }
    };
    let _ = open_result.send(Ok(()));
    drop(open_result);
    let failed = read_frames(&stream, stop, &mut on_frame);
    // Release the camera before `on_end` runs.
    drop(stream);
    if failed && !stop.load(Ordering::Acquire) {
        on_end();
    }
}

/// Finds the node with this ID and starts it. No other node is used.
fn open(device_id: &str, target: SendTier) -> Result<Stream, CameraState> {
    for index in 0..MAX_VIDEO_NODES {
        let Some(fd) = open_node(index) else {
            continue;
        };
        if identify(fd.as_raw_fd()).is_some_and(|(id, _)| id == device_id) {
            return start_stream(fd, target);
        }
    }
    Err(CameraState::NotFound)
}

/// Reads until `stop`. Returns true when the capture ended by an error.
fn read_frames<F: FnMut(CapturedFrame<'_>)>(
    stream: &Stream,
    stop: &AtomicBool,
    on_frame: &mut F,
) -> bool {
    let raw = stream.fd.as_raw_fd();
    let mut converted = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let mut wait = libc::pollfd {
            fd: raw,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` for the call.
        let ready = unsafe { libc::poll(&mut wait, 1, POLL_MILLIS) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return true;
        }
        if wait.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return true;
        }
        if ready == 0 {
            continue;
        }
        let mut buffer: Buffer = zeroed_struct();
        buffer.type_ = BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = MEMORY_MMAP;
        match ioctl(raw, VIDIOC_DQBUF, &mut buffer) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => return true,
        }
        let index = buffer.index;
        deliver(stream, &buffer, &mut converted, on_frame);
        if queue(raw, index).is_err() {
            return true;
        }
    }
    false
}

/// Hands one picture to the callback as NV12 or I420. A picture with the error
/// flag, a bad buffer index or too few bytes is skipped.
fn deliver<F: FnMut(CapturedFrame<'_>)>(
    stream: &Stream,
    buffer: &Buffer,
    converted: &mut Vec<u8>,
    on_frame: &mut F,
) {
    let Some(map) = stream.maps.get(buffer.index as usize) else {
        return;
    };
    if buffer.flags & BUF_FLAG_ERROR != 0 {
        return;
    }
    let used = match buffer.bytesused as usize {
        0 => map.len,
        used => used.min(map.len),
    };
    // SAFETY: the mapping holds `map.len` readable bytes and `used` is not
    // larger. The buffer was dequeued, so the driver does not write it until
    // it is queued again, which happens after this slice is gone.
    let bytes = unsafe { std::slice::from_raw_parts(map.ptr.cast::<u8>(), used) };
    let (width, height, stride) = (stream.width, stream.height, stream.stride);
    yuyv::deliver_frame(
        stream.format,
        width,
        height,
        stride,
        bytes,
        converted,
        on_frame,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN the V4L2 request numbers WHEN computed THEN they equal the
    /// values of the 64-bit Linux headers.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn given_request_numbers_then_they_match_the_kernel_headers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xC040_5602);
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xC014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xC058_5609);
        assert_eq!(VIDIOC_QBUF, 0xC058_560F);
        assert_eq!(VIDIOC_DQBUF, 0xC058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xC02C_564A);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xC034_564B);
        assert_eq!(VIDIOC_S_PARM, 0xC0CC_5616);
    }

    /// GIVEN the four-character codes WHEN built THEN they equal the kernel's.
    #[test]
    fn given_fourcc_then_it_matches_the_kernel() {
        assert_eq!(FOURCC_YUYV, 0x5659_5559);
        assert_eq!(FOURCC_NV12, 0x3231_564E);
        assert_eq!(FOURCC_YU12, 0x3231_5559);
        assert_eq!(pixel_format(FOURCC_NV12), Some(PixelFormat::Nv12));
        assert_eq!(pixel_format(FOURCC_YU12), Some(PixelFormat::I420));
        assert_eq!(pixel_format(FOURCC_YUYV), Some(PixelFormat::Yuyv));
        // MJPEG and H.264 are not taken.
        assert_eq!(pixel_format(fourcc(b'M', b'J', b'P', b'G')), None);
        assert_eq!(pixel_format(fourcc(b'H', b'2', b'6', b'4')), None);
    }

    /// GIVEN capability words WHEN checked THEN only a node with video capture
    /// and streaming is a camera, and the node's own caps win when given.
    #[test]
    fn given_capabilities_then_only_capture_nodes_are_cameras() {
        let both = CAP_VIDEO_CAPTURE | CAP_STREAMING;
        assert!(is_capture_node(both, 0));
        assert!(is_capture_node(CAP_DEVICE_CAPS | CAP_VIDEO_CAPTURE, both));
        // A metadata node of a camera: the device can capture, the node cannot.
        assert!(!is_capture_node(
            CAP_DEVICE_CAPS | both,
            0x0080_0000 | CAP_STREAMING
        ));
        // Capture without streaming (read only), and streaming without capture.
        assert!(!is_capture_node(CAP_VIDEO_CAPTURE, 0));
        assert!(!is_capture_node(CAP_STREAMING, 0));
        // Multi-plane capture (0x1000) is not taken.
        assert!(!is_capture_node(0x0000_1000 | CAP_STREAMING, 0));
        assert!(!is_capture_node(0, 0));
    }

    /// GIVEN fixed text fields WHEN read THEN they stop at the NUL and clean
    /// text only is taken.
    #[test]
    fn given_fixed_fields_then_only_clean_text_is_taken() {
        assert_eq!(
            field_text(b"usb-0000:00:14.0-1\0junk"),
            Some("usb-0000:00:14.0-1".into())
        );
        assert_eq!(field_text(b"no-nul"), Some("no-nul".into()));
        assert_eq!(field_text(b"\0abc"), None);
        assert_eq!(field_text(b""), None);
        assert_eq!(field_text(b"bad\x07bell"), None);
        assert_eq!(field_text(&[0xff, 0xfe, 0]), None);
    }

    /// GIVEN the same port WHEN another camera model or driver is plugged in
    /// THEN its ID differs; separators inside fields cannot make two field
    /// sets collide, and the ID fits the protocol's limit.
    #[test]
    fn given_camera_fields_then_the_id_names_the_device_not_only_the_port() {
        let port = "usb-0000:00:14.0-1";
        let saved = camera_id("uvcvideo", "HD Webcam C270", port);
        assert_eq!(saved, "uvcvideo|HD Webcam C270|usb-0000:00:14.0-1");
        assert_ne!(camera_id("uvcvideo", "Other Cam", port), saved);
        assert_ne!(camera_id("gspca", "HD Webcam C270", port), saved);
        assert_ne!(
            camera_id("uvcvideo", "HD Webcam C270", "usb-0000:00:14.0-2"),
            saved
        );
        assert_ne!(camera_id("a|b", "c", "d"), camera_id("a", "b|c", "d"));
        assert_ne!(camera_id("a\\", "|b", "c"), camera_id("a", "\\|b", "c"));
        assert_eq!(camera_id("a|b\\", "", "c"), "a\\|b\\\\||c");
        let widest = camera_id(&"|".repeat(16), &"|".repeat(32), &"|".repeat(32));
        assert!(widest.len() <= persephone_call_protocol::MAX_DEVICE_ID_BYTES);
    }

    /// GIVEN a size range WHEN the standard sizes are taken THEN only those
    /// inside the range and on the step remain.
    #[test]
    fn given_size_range_then_standard_sizes_inside_it_remain() {
        // Continuous range: step 1.
        assert_eq!(
            range_sizes([160, 1920, 1, 120, 1080, 1]),
            vec![
                (640, 360),
                (640, 480),
                (1280, 720),
                (320, 240),
                (1920, 1080)
            ]
        );
        // The range of a 4K camera also holds 2560x1440.
        assert!(range_sizes([160, 3840, 1, 120, 2160, 1]).contains(&(2560, 1440)));
        // Step 16 in width and 8 in height from 160x120: 640x360, 1280x720
        // and 1920x1080 fit; 640x480 fits too; 320x240 fits.
        assert_eq!(range_sizes([160, 1920, 16, 120, 1080, 8]).len(), 5);
        // Narrow range.
        assert_eq!(
            range_sizes([1000, 1920, 0, 600, 1080, 0]),
            vec![(1280, 720)]
        );
        // Off the step: 360 - 100 = 260 is not a multiple of 64.
        assert_eq!(range_sizes([640, 640, 0, 100, 600, 64]), Vec::new());
        // Range with nothing in it.
        assert_eq!(range_sizes([2000, 3000, 1, 2000, 3000, 1]), Vec::new());
    }

    /// GIVEN a format WHEN the least row length is asked THEN YUYV needs two
    /// bytes per pixel and the planar formats one.
    #[test]
    fn given_format_then_the_least_row_length_follows_it() {
        assert_eq!(min_row_bytes(PixelFormat::Yuyv, 640), Some(1280));
        assert_eq!(min_row_bytes(PixelFormat::Nv12, 640), Some(640));
        assert_eq!(min_row_bytes(PixelFormat::I420, 640), Some(640));
        assert_eq!(min_row_bytes(PixelFormat::Yuyv, u32::MAX), None);
    }

    /// GIVEN frame intervals WHEN the rate is read THEN it is the whole
    /// frames per second, rounded down, and a zero interval gives none.
    #[test]
    fn given_intervals_then_the_rate_rounds_down() {
        assert_eq!(rate_of(1, 30), 30);
        assert_eq!(rate_of(1001, 30000), 29);
        assert_eq!(rate_of(1, 5), 5);
        assert_eq!(rate_of(0, 30), 0);
        assert_eq!(rate_of(1, 0), 0);
    }

    /// GIVEN the widest picture WHEN its row is measured THEN it fits the
    /// stride limit, and so does its buffer.
    #[test]
    fn given_the_widest_picture_then_row_and_buffer_fit_the_limits() {
        let row = min_row_bytes(PixelFormat::Yuyv, yuyv::MAX_LONG_SIDE).expect("row");
        assert!(row <= MAX_STRIDE);
        let bytes = u64::from(row) * u64::from(yuyv::MAX_SHORT_SIDE);
        assert!(bytes <= u64::from(MAX_BUFFER_BYTES));
        assert!(u64::from(MAX_BUFFERS) * u64::from(MAX_BUFFER_BYTES) <= 256 << 20);
    }

    /// GIVEN a missing node WHEN opened THEN there is no descriptor. (A
    /// machine may have /dev/video0; node 63 is used to stay clear of it.)
    #[test]
    fn given_absent_node_when_opened_then_none() {
        assert!(open_node(MAX_VIDEO_NODES + 1000).is_none());
    }

    /// GIVEN no camera with an ID WHEN a capture starts THEN it reports
    /// NotFound and no other camera is used.
    #[test]
    fn given_unknown_id_when_opened_then_not_found() {
        assert_eq!(
            open("uvcvideo|No Such Camera|usb-none", SendTier::P360).err(),
            Some(CameraState::NotFound)
        );
    }
}
