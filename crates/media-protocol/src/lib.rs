use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MIB: u64 = 1024 * 1024;
pub const APP_ADMITTED_LIMIT: u64 = 356 * MIB;
pub const ENCODED_IN_FLIGHT_LIMIT: u64 = 40 * MIB;
pub const WORKER_OUTPUT_LIMIT: u64 = 64 * MIB;
pub const RETAINED_PIXELS_LIMIT: u64 = 64 * MIB;
pub const BOUNDED_VIEWS_LIMIT: u64 = 16 * MIB;
pub const CONTROL_QUEUE_LIMIT: u64 = 16 * MIB;
/// The SDK's copies of one image or file being sent (ADR-0031).
pub const OUTGOING_SEND_LIMIT: u64 = 100 * MIB;
pub const RESERVED_HEADROOM: u64 = 56 * MIB;

pub const MAX_DIMENSION: u32 = 8192;
pub const MAX_PIXELS: u64 = 16_000_000;
pub const RGBA_BYTES_PER_PIXEL: u64 = 4;

const FRAME_HEADER_LEN: usize = 17;
const CONTROL_FRAME_LIMIT: u64 = 128 * 1024;
const ENCODED_FRAME_LIMIT: u64 = 20 * MIB;
const PIXEL_FRAME_LIMIT: u64 = 64 * MIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetClass {
    EncodedInFlight,
    WorkerOutput,
    RetainedPixels,
    BoundedViews,
    ControlQueue,
    OutgoingSend,
}

impl BudgetClass {
    const fn limit(self) -> u64 {
        match self {
            Self::EncodedInFlight => ENCODED_IN_FLIGHT_LIMIT,
            Self::WorkerOutput => WORKER_OUTPUT_LIMIT,
            Self::RetainedPixels => RETAINED_PIXELS_LIMIT,
            Self::BoundedViews => BOUNDED_VIEWS_LIMIT,
            Self::ControlQueue => CONTROL_QUEUE_LIMIT,
            Self::OutgoingSend => OUTGOING_SEND_LIMIT,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum AdmissionError {
    Empty,
    ClassLimit,
    AggregateLimit,
    ArithmeticOverflow,
}

struct Counter {
    limit: u64,
    used: AtomicU64,
}

impl Counter {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    fn acquire(self: &Arc<Self>, bytes: u64) -> Result<Permit, AdmissionError> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let next = current
                .checked_add(bytes)
                .ok_or(AdmissionError::ArithmeticOverflow)?;
            if next > self.limit {
                return Err(AdmissionError::AggregateLimit);
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Permit {
                        counter: Arc::clone(self),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

/// An RAII byte reservation. Dropping it releases the exact reserved amount.
pub struct Permit {
    counter: Arc<Counter>,
    bytes: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let previous = self.counter.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

/// Shared app-level and per-class byte admission. Reserve a permit before allocating or queueing.
pub struct ResourceBudgets {
    aggregate: Arc<Counter>,
    classes: [Arc<Counter>; 6],
}

impl Default for ResourceBudgets {
    fn default() -> Self {
        Self::new(APP_ADMITTED_LIMIT)
    }
}

impl ResourceBudgets {
    pub fn new(aggregate_limit: u64) -> Self {
        Self {
            aggregate: Arc::new(Counter::new(aggregate_limit)),
            classes: [
                Arc::new(Counter::new(ENCODED_IN_FLIGHT_LIMIT)),
                Arc::new(Counter::new(WORKER_OUTPUT_LIMIT)),
                Arc::new(Counter::new(RETAINED_PIXELS_LIMIT)),
                Arc::new(Counter::new(BOUNDED_VIEWS_LIMIT)),
                Arc::new(Counter::new(CONTROL_QUEUE_LIMIT)),
                Arc::new(Counter::new(OUTGOING_SEND_LIMIT)),
            ],
        }
    }

    pub fn try_acquire(
        &self,
        class: BudgetClass,
        bytes: u64,
    ) -> Result<BudgetPermit, AdmissionError> {
        if bytes == 0 {
            return Err(AdmissionError::Empty);
        }
        if bytes > class.limit() {
            return Err(AdmissionError::ClassLimit);
        }

        let aggregate = self.aggregate.acquire(bytes)?;
        let class_permit = match self.classes[class.index()].acquire(bytes) {
            Ok(permit) => permit,
            Err(_) => {
                drop(aggregate);
                return Err(AdmissionError::ClassLimit);
            }
        };
        Ok(BudgetPermit {
            _aggregate: aggregate,
            _class: class_permit,
            bytes,
        })
    }

    pub fn aggregate_used(&self) -> u64 {
        self.aggregate.used()
    }

    pub fn class_used(&self, class: BudgetClass) -> u64 {
        self.classes[class.index()].used()
    }
}

impl BudgetClass {
    const fn index(self) -> usize {
        match self {
            Self::EncodedInFlight => 0,
            Self::WorkerOutput => 1,
            Self::RetainedPixels => 2,
            Self::BoundedViews => 3,
            Self::ControlQueue => 4,
            Self::OutgoingSend => 5,
        }
    }
}

pub struct BudgetPermit {
    _aggregate: Permit,
    _class: Permit,
    bytes: u64,
}

impl BudgetPermit {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FrameKind {
    Control = 1,
    EncodedImage = 2,
    RgbaPixels = 3,
}

impl TryFrom<u8> for FrameKind {
    type Error = FrameError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Control),
            2 => Ok(Self::EncodedImage),
            3 => Ok(Self::RgbaPixels),
            _ => Err(FrameError::UnknownKind(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameHeader {
    pub kind: FrameKind,
    pub length: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub enum FrameError {
    Io(io::ErrorKind),
    UnknownKind(u8),
    EmptyPayload,
    TooLarge,
    UnexpectedDimensions,
    ZeroDimension,
    DimensionLimit,
    PixelCountLimit,
    PixelLengthMismatch,
    LengthDoesNotFitPlatform,
    AllocationFailure,
    Budget(AdmissionError),
}

impl FrameHeader {
    /// Decode the fixed header: kind byte, big-endian u64 body length, and two big-endian u32 fields.
    pub fn decode(bytes: [u8; FRAME_HEADER_LEN]) -> Result<Self, FrameError> {
        let kind = FrameKind::try_from(bytes[0])?;
        let length = u64::from_be_bytes(bytes[1..9].try_into().expect("fixed header slice"));
        let width = u32::from_be_bytes(bytes[9..13].try_into().expect("fixed header slice"));
        let height = u32::from_be_bytes(bytes[13..17].try_into().expect("fixed header slice"));

        if length == 0 {
            return Err(FrameError::EmptyPayload);
        }

        match kind {
            FrameKind::Control => {
                if width != 0 || height != 0 {
                    return Err(FrameError::UnexpectedDimensions);
                }
                if length > CONTROL_FRAME_LIMIT {
                    return Err(FrameError::TooLarge);
                }
            }
            FrameKind::EncodedImage => {
                if width != 0 || height != 0 {
                    return Err(FrameError::UnexpectedDimensions);
                }
                if length > ENCODED_FRAME_LIMIT {
                    return Err(FrameError::TooLarge);
                }
            }
            FrameKind::RgbaPixels => {
                if width == 0 || height == 0 {
                    return Err(FrameError::ZeroDimension);
                }
                if width > MAX_DIMENSION || height > MAX_DIMENSION {
                    return Err(FrameError::DimensionLimit);
                }
                let pixel_count = u64::from(width)
                    .checked_mul(u64::from(height))
                    .ok_or(FrameError::PixelCountLimit)?;
                if pixel_count > MAX_PIXELS {
                    return Err(FrameError::PixelCountLimit);
                }
                let expected_length = pixel_count
                    .checked_mul(RGBA_BYTES_PER_PIXEL)
                    .ok_or(FrameError::PixelLengthMismatch)?;
                if length != expected_length {
                    return Err(FrameError::PixelLengthMismatch);
                }
                if length > PIXEL_FRAME_LIMIT {
                    return Err(FrameError::TooLarge);
                }
            }
        }

        usize::try_from(length).map_err(|_| FrameError::LengthDoesNotFitPlatform)?;
        Ok(Self {
            kind,
            length,
            width,
            height,
        })
    }

    pub fn encode(self) -> [u8; FRAME_HEADER_LEN] {
        let mut bytes = [0; FRAME_HEADER_LEN];
        bytes[0] = self.kind as u8;
        bytes[1..9].copy_from_slice(&self.length.to_be_bytes());
        bytes[9..13].copy_from_slice(&self.width.to_be_bytes());
        bytes[13..17].copy_from_slice(&self.height.to_be_bytes());
        bytes
    }

    const fn budget_class(self) -> BudgetClass {
        match self.kind {
            FrameKind::Control => BudgetClass::ControlQueue,
            FrameKind::EncodedImage => BudgetClass::EncodedInFlight,
            FrameKind::RgbaPixels => BudgetClass::WorkerOutput,
        }
    }
}

pub struct ReceivedFrame {
    header: FrameHeader,
    body: Vec<u8>,
    _permit: BudgetPermit,
}

impl ReceivedFrame {
    pub fn header(&self) -> FrameHeader {
        self.header
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Read only the fixed header first. Rejects lengths/dimensions before body allocation or read.
pub fn read_frame<R: Read>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<ReceivedFrame, FrameError> {
    let mut wire_header = [0; FRAME_HEADER_LEN];
    reader
        .read_exact(&mut wire_header)
        .map_err(|error| FrameError::Io(error.kind()))?;
    let header = FrameHeader::decode(wire_header)?;
    let permit = budgets
        .try_acquire(header.budget_class(), header.length)
        .map_err(FrameError::Budget)?;
    let length =
        usize::try_from(header.length).map_err(|_| FrameError::LengthDoesNotFitPlatform)?;
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|_| FrameError::AllocationFailure)?;
    body.resize(length, 0);
    reader
        .read_exact(&mut body)
        .map_err(|error| FrameError::Io(error.kind()))?;
    Ok(ReceivedFrame {
        header,
        body,
        _permit: permit,
    })
}

/// Writes one frame. The body length must match the header, and the header
/// must pass the same validation a receiver applies.
pub fn write_frame<W: Write>(writer: &mut W, header: FrameHeader, body: &[u8]) -> io::Result<()> {
    let wire = header.encode();
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    if u64::try_from(body.len()).ok() != Some(header.length) {
        return Err(invalid("frame body length does not match header"));
    }
    FrameHeader::decode(wire).map_err(|_| invalid("frame header fails receiver validation"))?;
    writer.write_all(&wire)?;
    writer.write_all(body)?;
    writer.flush()
}

/// Version byte of the worker failure message.
pub const WORKER_FAILURE_VERSION: u8 = 1;
const WORKER_FAILURE_LEN: usize = 6;

/// Why a media worker returned no pixels. Sent as a 6-byte control frame body:
/// version, tag and a big-endian 32-bit detail. There is deliberately no free
/// text, so nothing derived from the untrusted input can travel back as a
/// message string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerFailure {
    /// The worker did not observe its required containment; the detail is a
    /// bitmask of the checks that failed. No input was read.
    ContainmentCheck(u32),
    BadRequest,
    ModuleRejected,
    InputTooLarge,
    Instantiation,
    FuelExhausted,
    Trap,
    /// The guest decoder refused the input with this status.
    GuestRejected(i32),
    NoOutput,
    InvalidOutput,
}

impl WorkerFailure {
    pub fn encode(self) -> [u8; WORKER_FAILURE_LEN] {
        let (tag, detail): (u8, u32) = match self {
            Self::ContainmentCheck(mask) => (1, mask),
            Self::BadRequest => (2, 0),
            Self::ModuleRejected => (3, 0),
            Self::InputTooLarge => (4, 0),
            Self::Instantiation => (5, 0),
            Self::FuelExhausted => (6, 0),
            Self::Trap => (7, 0),
            Self::GuestRejected(status) => (8, status as u32),
            Self::NoOutput => (9, 0),
            Self::InvalidOutput => (10, 0),
        };
        let mut bytes = [0; WORKER_FAILURE_LEN];
        bytes[0] = WORKER_FAILURE_VERSION;
        bytes[1] = tag;
        bytes[2..6].copy_from_slice(&detail.to_be_bytes());
        bytes
    }

    pub fn decode(body: &[u8]) -> Option<Self> {
        let bytes: [u8; WORKER_FAILURE_LEN] = body.try_into().ok()?;
        if bytes[0] != WORKER_FAILURE_VERSION {
            return None;
        }
        let detail = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        let no_detail = |failure| (detail == 0).then_some(failure);
        match bytes[1] {
            1 => Some(Self::ContainmentCheck(detail)),
            2 => no_detail(Self::BadRequest),
            3 => no_detail(Self::ModuleRejected),
            4 => no_detail(Self::InputTooLarge),
            5 => no_detail(Self::Instantiation),
            6 => no_detail(Self::FuelExhausted),
            7 => no_detail(Self::Trap),
            8 => Some(Self::GuestRejected(detail as i32)),
            9 => no_detail(Self::NoOutput),
            10 => no_detail(Self::InvalidOutput),
            _ => None,
        }
    }

    /// The control frame header for this message.
    pub fn header() -> FrameHeader {
        FrameHeader {
            kind: FrameKind::Control,
            length: WORKER_FAILURE_LEN as u64,
            width: 0,
            height: 0,
        }
    }
}

/// Version byte of the worker request message.
pub const WORKER_REQUEST_VERSION: u8 = 1;
/// Version byte of the pixel information message.
pub const PIXEL_INFO_VERSION: u8 = 1;
const WORKER_REQUEST_LEN: usize = 2;
const PIXEL_INFO_LEN: usize = 2;

/// The operation a media worker runs on the encoded image that follows.
///
/// A request is two frames: a control frame with this 2-byte body (version,
/// operation), then one `EncodedImage` frame. A decode replies with an
/// `RgbaPixels` frame followed by a [`PixelInfo`] control frame; a strip
/// replies with one `EncodedImage` frame. Either may instead reply with one
/// [`WorkerFailure`] control frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkerOperation {
    /// Decode to RGBA pixels and report the EXIF orientation.
    Decode = 1,
    /// Remove metadata and return the stripped encoded file.
    Strip = 2,
}

impl WorkerOperation {
    pub fn encode(self) -> [u8; WORKER_REQUEST_LEN] {
        [WORKER_REQUEST_VERSION, self as u8]
    }

    pub fn decode(body: &[u8]) -> Option<Self> {
        match body {
            [WORKER_REQUEST_VERSION, 1] => Some(Self::Decode),
            [WORKER_REQUEST_VERSION, 2] => Some(Self::Strip),
            _ => None,
        }
    }

    /// The control frame header for this message.
    pub fn header() -> FrameHeader {
        FrameHeader {
            kind: FrameKind::Control,
            length: WORKER_REQUEST_LEN as u64,
            width: 0,
            height: 0,
        }
    }
}

/// Facts about decoded pixels that the 17-byte `RgbaPixels` header cannot
/// carry. The worker sends it as a control frame directly after the pixel
/// frame, so the pixel header keeps its meaning for every other user. Body:
/// version and the EXIF orientation (1 to 8). The pixels are not rotated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelInfo {
    orientation: u8,
}

impl PixelInfo {
    /// Returns `None` for an orientation outside 1 to 8.
    pub fn new(orientation: u8) -> Option<Self> {
        (1..=8)
            .contains(&orientation)
            .then_some(Self { orientation })
    }

    pub fn orientation(self) -> u8 {
        self.orientation
    }

    pub fn encode(self) -> [u8; PIXEL_INFO_LEN] {
        [PIXEL_INFO_VERSION, self.orientation]
    }

    pub fn decode(body: &[u8]) -> Option<Self> {
        match body {
            [PIXEL_INFO_VERSION, orientation] => Self::new(*orientation),
            _ => None,
        }
    }

    /// The control frame header for this message.
    pub fn header() -> FrameHeader {
        FrameHeader {
            kind: FrameKind::Control,
            length: PIXEL_INFO_LEN as u64,
            width: 0,
            height: 0,
        }
    }
}

/// Version byte of the worker readiness message.
pub const WORKER_READY_VERSION: u8 = 1;
const WORKER_READY_LEN: usize = 2;
const WORKER_READY: [u8; WORKER_READY_LEN] = [WORKER_READY_VERSION, 0];

/// The first frame of a Linux or macOS worker: its containment is in place
/// and it has read nothing yet. The broker then checks that containment from
/// outside the worker and sends the request only if the check passes. Body:
/// version and a zero byte, which no request or [`PixelInfo`] body uses. The
/// Windows worker does not send it, because its containment is applied
/// before it runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerReady;

impl WorkerReady {
    pub fn encode(self) -> [u8; WORKER_READY_LEN] {
        WORKER_READY
    }

    /// Whether `body` is exactly the readiness message.
    pub fn matches(body: &[u8]) -> bool {
        body == WORKER_READY
    }

    /// The control frame header for this message.
    pub fn header() -> FrameHeader {
        FrameHeader {
            kind: FrameKind::Control,
            length: WORKER_READY_LEN as u64,
            width: 0,
            height: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Barrier;
    use std::thread;

    fn header(kind: FrameKind, length: u64, width: u32, height: u32) -> [u8; FRAME_HEADER_LEN] {
        FrameHeader {
            kind,
            length,
            width,
            height,
        }
        .encode()
    }

    #[test]
    fn accounting_categories_fit_under_the_total_and_keep_headroom() {
        let class_sum = ENCODED_IN_FLIGHT_LIMIT
            + WORKER_OUTPUT_LIMIT
            + RETAINED_PIXELS_LIMIT
            + BOUNDED_VIEWS_LIMIT
            + CONTROL_QUEUE_LIMIT
            + OUTGOING_SEND_LIMIT;
        assert_eq!(class_sum + RESERVED_HEADROOM, APP_ADMITTED_LIMIT);
        assert!(class_sum <= APP_ADMITTED_LIMIT);
    }

    #[test]
    fn permits_reject_over_limit_and_release_on_drop() {
        let budgets = ResourceBudgets::new(8);
        let first = budgets.try_acquire(BudgetClass::ControlQueue, 6).unwrap();
        assert_eq!(budgets.aggregate_used(), 6);
        assert_eq!(
            budgets.try_acquire(BudgetClass::BoundedViews, 3).err(),
            Some(AdmissionError::AggregateLimit)
        );
        drop(first);
        assert_eq!(budgets.aggregate_used(), 0);
        let second = budgets.try_acquire(BudgetClass::BoundedViews, 8).unwrap();
        assert_eq!(second.bytes(), 8);
    }

    #[test]
    fn checked_counter_addition_rejects_integer_overflow() {
        let counter = Arc::new(Counter::new(u64::MAX));
        let held = counter.acquire(u64::MAX).unwrap();
        assert_eq!(
            counter.acquire(1).err(),
            Some(AdmissionError::ArithmeticOverflow)
        );
        drop(held);
        assert_eq!(counter.used(), 0);
    }

    #[test]
    fn class_limit_is_checked_before_aggregate_admission() {
        let budgets = ResourceBudgets::new(APP_ADMITTED_LIMIT);
        assert_eq!(
            budgets
                .try_acquire(BudgetClass::EncodedInFlight, ENCODED_IN_FLIGHT_LIMIT + 1)
                .err(),
            Some(AdmissionError::ClassLimit)
        );
        assert_eq!(budgets.aggregate_used(), 0);
        assert_eq!(
            budgets.try_acquire(BudgetClass::ControlQueue, 0).err(),
            Some(AdmissionError::Empty)
        );
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn concurrent_permits_never_exceed_shared_aggregate_limit() {
        let limit = 64;
        let budgets = Arc::new(ResourceBudgets::new(limit));
        let barrier = Arc::new(Barrier::new(33));
        let mut workers = Vec::new();
        for _ in 0..32 {
            let budgets = Arc::clone(&budgets);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                budgets.try_acquire(BudgetClass::BoundedViews, 3).ok()
            }));
        }
        barrier.wait();
        let permits = workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(permits.len(), 21);
        assert_eq!(budgets.aggregate_used(), 63);
        assert!(budgets.aggregate_used() <= limit);
    }

    #[test]
    fn frame_rejects_oversized_length_after_header_before_body_read() {
        let mut wire = header(FrameKind::EncodedImage, ENCODED_FRAME_LIMIT + 1, 0, 0).to_vec();
        wire.extend_from_slice(b"must not be consumed");
        let mut input = Cursor::new(wire);
        let budgets = ResourceBudgets::default();
        assert_eq!(
            read_frame(&mut input, &budgets).err(),
            Some(FrameError::TooLarge)
        );
        assert_eq!(input.position(), FRAME_HEADER_LEN as u64);
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn each_frame_kind_accepts_its_exact_limit_and_rejects_one_byte_more() {
        for (kind, limit, width, height, next_error) in [
            (
                FrameKind::Control,
                CONTROL_FRAME_LIMIT,
                0,
                0,
                FrameError::TooLarge,
            ),
            (
                FrameKind::EncodedImage,
                ENCODED_FRAME_LIMIT,
                0,
                0,
                FrameError::TooLarge,
            ),
            (
                FrameKind::RgbaPixels,
                4000 * 4000 * 4,
                4000,
                4000,
                FrameError::PixelCountLimit,
            ),
        ] {
            assert!(FrameHeader::decode(header(kind, limit, width, height)).is_ok());
            let oversize = if kind == FrameKind::RgbaPixels {
                header(kind, limit + 4, 4001, 4000)
            } else {
                header(kind, limit + 1, width, height)
            };
            assert_eq!(FrameHeader::decode(oversize), Err(next_error));
        }
    }

    #[test]
    fn frame_rejects_zero_and_mismatched_pixel_metadata_before_body_read() {
        for (length, width, height, expected) in [
            (4, 0, 1, FrameError::ZeroDimension),
            (4, 1, 0, FrameError::ZeroDimension),
            (3, 1, 1, FrameError::PixelLengthMismatch),
            (4, 2, 1, FrameError::PixelLengthMismatch),
        ] {
            let mut wire = header(FrameKind::RgbaPixels, length, width, height).to_vec();
            wire.extend_from_slice(b"payload");
            let mut input = Cursor::new(wire);
            assert_eq!(
                read_frame(&mut input, &ResourceBudgets::default()).err(),
                Some(expected)
            );
            assert_eq!(input.position(), FRAME_HEADER_LEN as u64);
        }
    }

    #[test]
    fn dimensions_enforce_maxima_and_exact_rgba_size() {
        let max_safe = FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: 4000 * 4000 * RGBA_BYTES_PER_PIXEL,
            width: 4000,
            height: 4000,
        };
        assert_eq!(FrameHeader::decode(max_safe.encode()), Ok(max_safe));

        let too_many_pixels = header(FrameKind::RgbaPixels, 8192 * 2049 * 4, 8192, 2049);
        assert_eq!(
            FrameHeader::decode(too_many_pixels),
            Err(FrameError::PixelCountLimit)
        );

        let too_wide = header(FrameKind::RgbaPixels, 8193 * 4, 8193, 1);
        assert_eq!(
            FrameHeader::decode(too_wide),
            Err(FrameError::DimensionLimit)
        );
    }

    #[test]
    fn partial_body_read_releases_reserved_bytes_and_frame_drop_releases_them() {
        let mut short_body = header(FrameKind::Control, 4, 0, 0).to_vec();
        short_body.extend_from_slice(b"ab");
        let mut input = Cursor::new(short_body);
        let budgets = ResourceBudgets::default();
        assert_eq!(
            read_frame(&mut input, &budgets).err(),
            Some(FrameError::Io(io::ErrorKind::UnexpectedEof))
        );
        assert_eq!(budgets.aggregate_used(), 0);

        let mut full = header(FrameKind::Control, 4, 0, 0).to_vec();
        full.extend_from_slice(b"data");
        let mut input = Cursor::new(full);
        let bounded_budgets = ResourceBudgets::new(4);
        let frame = read_frame(&mut input, &bounded_budgets).unwrap();
        assert_eq!(frame.header().kind, FrameKind::Control);
        assert_eq!(frame.body(), b"data");
        assert_eq!(bounded_budgets.aggregate_used(), 4);
        assert_eq!(
            bounded_budgets
                .try_acquire(BudgetClass::BoundedViews, 1)
                .err(),
            Some(AdmissionError::AggregateLimit)
        );
        assert_eq!(bounded_budgets.aggregate_used(), 4);
        drop(frame);
        assert_eq!(bounded_budgets.aggregate_used(), 0);

        let mut another = header(FrameKind::Control, 4, 0, 0).to_vec();
        another.extend_from_slice(b"data");
        let mut input = Cursor::new(another);
        let frame = read_frame(&mut input, &budgets).unwrap();
        assert_eq!(budgets.aggregate_used(), 4);
        drop(frame);
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_worker_failures_when_encoded_then_decode_round_trips_strictly() {
        for failure in [
            WorkerFailure::ContainmentCheck(0b1011),
            WorkerFailure::BadRequest,
            WorkerFailure::ModuleRejected,
            WorkerFailure::InputTooLarge,
            WorkerFailure::Instantiation,
            WorkerFailure::FuelExhausted,
            WorkerFailure::Trap,
            WorkerFailure::GuestRejected(-3),
            WorkerFailure::NoOutput,
            WorkerFailure::InvalidOutput,
        ] {
            assert_eq!(WorkerFailure::decode(&failure.encode()), Some(failure));
        }
        let mut wrong_version = WorkerFailure::Trap.encode();
        wrong_version[0] = 2;
        let mut stray_detail = WorkerFailure::Trap.encode();
        stray_detail[5] = 1;
        let mut unknown_tag = WorkerFailure::Trap.encode();
        unknown_tag[1] = 99;
        for body in [
            &wrong_version[..],
            &stray_detail,
            &unknown_tag,
            &[1, 7, 0, 0, 0],
            &[],
        ] {
            assert_eq!(WorkerFailure::decode(body), None);
        }
    }

    #[test]
    fn given_worker_requests_when_encoded_then_decode_round_trips_strictly() {
        for operation in [WorkerOperation::Decode, WorkerOperation::Strip] {
            assert_eq!(
                WorkerOperation::decode(&operation.encode()),
                Some(operation)
            );
        }
        assert_eq!(WorkerOperation::header().length, 2);
        for body in [&[1u8, 0][..], &[1, 3], &[2, 1], &[1], &[1, 1, 0], &[]] {
            assert_eq!(WorkerOperation::decode(body), None, "{body:?}");
        }
    }

    #[test]
    fn given_pixel_info_then_only_versioned_orientations_one_to_eight_are_accepted() {
        for orientation in 1..=8 {
            let info = PixelInfo::new(orientation).unwrap();
            assert_eq!(PixelInfo::decode(&info.encode()), Some(info));
            assert_eq!(info.orientation(), orientation);
        }
        assert_eq!(PixelInfo::new(0), None);
        assert_eq!(PixelInfo::new(9), None);
        for body in [&[1u8, 0][..], &[1, 9], &[2, 1], &[1], &[1, 1, 0], &[]] {
            assert_eq!(PixelInfo::decode(body), None, "{body:?}");
        }
        assert!(FrameHeader::decode(PixelInfo::header().encode()).is_ok());
    }

    #[test]
    fn given_worker_ready_then_only_its_exact_body_matches_and_no_other_message_decodes_it() {
        let ready = WorkerReady.encode();
        assert!(WorkerReady::matches(&ready));
        assert_eq!(WorkerReady::header().length, ready.len() as u64);
        assert!(FrameHeader::decode(WorkerReady::header().encode()).is_ok());
        for body in [&[1u8, 1][..], &[2, 0], &[1], &[1, 0, 0], &[]] {
            assert!(!WorkerReady::matches(body), "{body:?}");
        }
        assert_eq!(WorkerOperation::decode(&ready), None);
        assert_eq!(PixelInfo::decode(&ready), None);
        assert_eq!(WorkerFailure::decode(&ready), None);
    }

    #[test]
    fn given_frames_when_written_then_reader_accepts_them_and_mismatches_are_refused() {
        let budgets = ResourceBudgets::default();
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            WorkerFailure::header(),
            &WorkerFailure::NoOutput.encode(),
        )
        .unwrap();
        let pixels = FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: 8,
            width: 1,
            height: 2,
        };
        write_frame(&mut wire, pixels, &[9; 8]).unwrap();
        let mut reader = Cursor::new(wire);
        let control = read_frame(&mut reader, &budgets).unwrap();
        assert_eq!(
            WorkerFailure::decode(control.body()),
            Some(WorkerFailure::NoOutput)
        );
        let frame = read_frame(&mut reader, &budgets).unwrap();
        assert_eq!(
            (frame.header().width, frame.header().height, frame.body()),
            (1, 2, &[9u8; 8][..])
        );

        let mut sink = Vec::new();
        assert!(write_frame(&mut sink, pixels, &[0; 7]).is_err());
        let bad_dims = FrameHeader { width: 3, ..pixels };
        assert!(write_frame(&mut sink, bad_dims, &[0; 8]).is_err());
        assert!(sink.is_empty(), "nothing is written for a refused frame");
    }
}
