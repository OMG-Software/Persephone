//! Sound server device names on Linux (ADR-0049).
//!
//! libwebrtc's PulseAudio module gives no stable device IDs, only positions
//! (`index:N`), which are never listed or selected (ADR-0043). This module
//! asks the sound server for its sink and source names through the
//! PulseAudio client library that libwebrtc already loads. It uses one
//! short-lived connection, before containment stage 2, and only reads.
//!
//! A call also opens a [`Session`] before stage 2 and keeps it. It moves the
//! call's own streams (this process ID only) to another microphone or
//! speaker on that one connection, so it needs no new socket after stage 2.
//!
//! The library is opened with `dlopen` (as libwebrtc does), so the worker
//! has no build or link dependency on it. A missing library, a failed
//! connection or a slow server gives `None`; the caller then keeps the
//! system default only.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use persephone_call_protocol::{FailureCode, MAX_PULSE_NAME_BYTES};

use crate::mapping::{
    OwnStream, SoundKnown, SoundNow, SoundTarget, own_streams, plan_sound_move, routes_kept,
};

/// The longest the whole query may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest one wait of the main loop, in microseconds.
const STEP_MICROSECONDS: c_int = 100_000;
/// The most entries kept from one list. The wire keeps fewer.
const MAX_ENTRIES: usize = 64;
/// The most description bytes copied; the wire cuts it further.
const MAX_DESCRIPTION_BYTES: usize = 1024;

// pa_context_flags_t: never start a server (the worker cannot execute).
const PA_CONTEXT_NOAUTOSPAWN: c_int = 1;
// pa_context_state_t.
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
// PA_INVALID_INDEX: a source that is not the monitor of a sink.
const PA_INVALID_INDEX: u32 = u32::MAX;
const PA_CHANNELS_MAX: usize = 32;

/// The leading fields of `pa_sink_info` (pulse/introspect.h). Only these
/// are read, through a pointer the library gives.
#[repr(C)]
struct SinkInfoPrefix {
    name: *const c_char,
    index: u32,
    description: *const c_char,
}

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct ChannelMap {
    channels: u8,
    map: [c_int; PA_CHANNELS_MAX],
}

#[repr(C)]
struct CVolume {
    channels: u8,
    values: [u32; PA_CHANNELS_MAX],
}

/// The leading fields of `pa_source_info` up to `monitor_of_sink`
/// (pulse/introspect.h). This part of the struct has not changed since
/// PulseAudio 0.9.
#[repr(C)]
struct SourceInfoPrefix {
    name: *const c_char,
    index: u32,
    description: *const c_char,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    owner_module: u32,
    volume: CVolume,
    mute: c_int,
    monitor_of_sink: u32,
}

type Mainloop = c_void;
type MainloopApi = c_void;
type Context = c_void;
type Operation = c_void;
type SinkCallback = unsafe extern "C" fn(*mut Context, *const SinkInfoPrefix, c_int, *mut c_void);
type SourceCallback =
    unsafe extern "C" fn(*mut Context, *const SourceInfoPrefix, c_int, *mut c_void);

/// The library functions used. They stay valid: the library is never closed.
struct Api {
    mainloop_new: unsafe extern "C" fn() -> *mut Mainloop,
    mainloop_get_api: unsafe extern "C" fn(*mut Mainloop) -> *mut MainloopApi,
    mainloop_prepare: unsafe extern "C" fn(*mut Mainloop, c_int) -> c_int,
    mainloop_poll: unsafe extern "C" fn(*mut Mainloop) -> c_int,
    mainloop_dispatch: unsafe extern "C" fn(*mut Mainloop) -> c_int,
    mainloop_free: unsafe extern "C" fn(*mut Mainloop),
    context_new: unsafe extern "C" fn(*mut MainloopApi, *const c_char) -> *mut Context,
    context_connect:
        unsafe extern "C" fn(*mut Context, *const c_char, c_int, *const c_void) -> c_int,
    context_get_state: unsafe extern "C" fn(*const Context) -> c_int,
    context_disconnect: unsafe extern "C" fn(*mut Context),
    context_unref: unsafe extern "C" fn(*mut Context),
    get_sink_info_list:
        unsafe extern "C" fn(*mut Context, SinkCallback, *mut c_void) -> *mut Operation,
    get_source_info_list:
        unsafe extern "C" fn(*mut Context, SourceCallback, *mut c_void) -> *mut Operation,
    operation_cancel: unsafe extern "C" fn(*mut Operation),
    operation_unref: unsafe extern "C" fn(*mut Operation),
}

/// Looks up one symbol as a function pointer of type `F`.
///
/// # Safety
///
/// `F` must be the C signature of `name` in libpulse.
pub(crate) unsafe fn symbol<F: Copy>(handle: *mut c_void, name: &CStr) -> Option<F> {
    // SAFETY: the handle is open and the name is a C string.
    let pointer = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if pointer.is_null() {
        return None;
    }
    // SAFETY: F is a function pointer of the same size, as the caller states.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&pointer) })
}

/// The PulseAudio client library, or `None` when it is not installed. The
/// handle is never closed, so symbols from it stay valid. The device tests
/// (`sound_test.rs`) use it too.
pub(crate) fn library() -> Option<*mut c_void> {
    // SAFETY: a fixed library name; the handle is never closed.
    let handle =
        unsafe { libc::dlopen(c"libpulse.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    (!handle.is_null()).then_some(handle)
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let handle = library()?;
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
                context_disconnect: symbol(handle, c"pa_context_disconnect")?,
                context_unref: symbol(handle, c"pa_context_unref")?,
                get_sink_info_list: symbol(handle, c"pa_context_get_sink_info_list")?,
                get_source_info_list: symbol(handle, c"pa_context_get_source_info_list")?,
                operation_cancel: symbol(handle, c"pa_operation_cancel")?,
                operation_unref: symbol(handle, c"pa_operation_unref")?,
            })
        }
    })
    .as_ref()
}

/// The sound server's sinks (speakers) and sources (microphones) as
/// (name, description) pairs. Monitor sources are left out of `sources`: they
/// are never listed or chosen. They are kept apart in `monitors` as
/// (name, device index) pairs, so a recording stream on a monitor still has a
/// known route. The index lists hold the server's device index of each
/// entry, in the same order.
#[derive(Debug, Default)]
pub struct Lists {
    pub sinks: Vec<(String, String)>,
    pub sources: Vec<(String, String)>,
    pub monitors: Vec<(String, u32)>,
    pub sink_indexes: Vec<u32>,
    pub source_indexes: Vec<u32>,
}

impl Lists {
    /// Every source that a recording stream may be on, with its device
    /// index: the microphones and the monitors.
    fn recording_devices(&self) -> Vec<(u32, &str)> {
        let mics = self
            .source_indexes
            .iter()
            .copied()
            .zip(self.sources.iter().map(|(name, _)| name.as_str()));
        let monitors = self
            .monitors
            .iter()
            .map(|(name, index)| (*index, name.as_str()));
        mics.chain(monitors).collect()
    }

    /// Every sink that a playback stream may be on, with its device index.
    fn playout_devices(&self) -> Vec<(u32, &str)> {
        self.sink_indexes
            .iter()
            .copied()
            .zip(self.sinks.iter().map(|(name, _)| name.as_str()))
            .collect()
    }
}

/// One list as the callbacks fill it. The library calls back through a
/// pointer while `run` reads the state, so every access goes through a
/// shared reference and cells; no `&mut` is held across a library call.
#[derive(Default)]
struct Collector {
    entries: RefCell<Vec<(String, String)>>,
    /// The device index of each entry, in the same order.
    indexes: RefCell<Vec<u32>>,
    /// Monitor sources (name, device index), kept for stream routes only.
    monitors: RefCell<Vec<(String, u32)>>,
    done: Cell<bool>,
    failed: Cell<bool>,
}

impl Collector {
    /// Copies one entry. A name that is not short UTF-8 is left out; the
    /// caller checks the name rule again.
    ///
    /// # Safety
    ///
    /// Each pointer is null or a C string that lives for the call.
    unsafe fn add(&self, name: *const c_char, description: *const c_char, index: u32) {
        if self.entries.borrow().len() >= MAX_ENTRIES {
            return;
        }
        // SAFETY: a C string from the library, as the caller states.
        let Some(name) = (unsafe { short_name(name) }) else {
            return;
        };
        let description = if description.is_null() {
            String::new()
        } else {
            // SAFETY: a C string from the library, as the caller states.
            let bytes = unsafe { CStr::from_ptr(description) }.to_bytes();
            String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_DESCRIPTION_BYTES)]).into_owned()
        };
        self.entries
            .borrow_mut()
            .push((name.to_owned(), description));
        self.indexes.borrow_mut().push(index);
    }

    /// Copies the name and index of one monitor source, with the same name
    /// rule and the same limit as [`Collector::add`]. A monitor is never
    /// listed.
    ///
    /// # Safety
    ///
    /// `name` is null or a C string that lives for the call.
    unsafe fn add_monitor(&self, name: *const c_char, index: u32) {
        if self.monitors.borrow().len() >= MAX_ENTRIES {
            return;
        }
        // SAFETY: a C string from the library, as the caller states.
        if let Some(name) = unsafe { short_name(name) } {
            self.monitors.borrow_mut().push((name.to_owned(), index));
        }
    }

    fn end(&self, eol: c_int) {
        self.done.set(true);
        self.failed.set(eol < 0);
    }

    /// A pointer for the library's `userdata`.
    fn userdata(&self) -> *mut c_void {
        std::ptr::from_ref(self).cast_mut().cast()
    }
}

/// A device name from the library, or `None` when it is null, too long or
/// not UTF-8.
///
/// # Safety
///
/// `name` is null or a C string that lives as long as the result is used.
unsafe fn short_name<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        return None;
    }
    // SAFETY: a C string from the library, as the caller states.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    if bytes.len() > MAX_PULSE_NAME_BYTES {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

unsafe extern "C" fn on_sink(
    _context: *mut Context,
    info: *const SinkInfoPrefix,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the Collector that `query` passes and keeps alive.
    let collector = unsafe { &*userdata.cast::<Collector>() };
    if eol != 0 {
        collector.end(eol);
    } else if !info.is_null() {
        // SAFETY: the library gives a valid pa_sink_info for this call.
        unsafe { collector.add((*info).name, (*info).description, (*info).index) };
    }
}

unsafe extern "C" fn on_source(
    _context: *mut Context,
    info: *const SourceInfoPrefix,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the Collector that `query` passes and keeps alive.
    let collector = unsafe { &*userdata.cast::<Collector>() };
    if eol != 0 {
        collector.end(eol);
    } else if !info.is_null() {
        // SAFETY: the library gives a valid pa_source_info for this call.
        let info = unsafe { &*info };
        if info.monitor_of_sink == PA_INVALID_INDEX {
            // SAFETY: as above.
            unsafe { collector.add(info.name, info.description, info.index) };
        } else {
            // SAFETY: as above.
            unsafe { collector.add_monitor(info.name, info.index) };
        }
    }
}

/// Runs the main loop once for at most one step. `false` on a loop error.
fn step(api: &Api, mainloop: *mut Mainloop) -> bool {
    // SAFETY: the main loop is live and used on this thread only.
    unsafe {
        (api.mainloop_prepare)(mainloop, STEP_MICROSECONDS) >= 0
            && (api.mainloop_poll)(mainloop) >= 0
            && (api.mainloop_dispatch)(mainloop) >= 0
    }
}

/// Asks the sound server for its device names. `None` when the library or
/// the server is not there, a list fails, or the time runs out. It blocks
/// for at most about [`QUERY_TIMEOUT`]. Call it before containment stage 2:
/// it opens a new sound server socket.
pub fn query() -> Option<Lists> {
    let api = api()?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    // SAFETY: plain constructor.
    let mainloop = unsafe { (api.mainloop_new)() };
    if mainloop.is_null() {
        return None;
    }
    let sinks = Collector::default();
    let sources = Collector::default();
    // SAFETY: the main loop is live; the name is a C string.
    let context =
        unsafe { (api.context_new)((api.mainloop_get_api)(mainloop), c"Persephone".as_ptr()) };
    let mut operations: [*mut Operation; 2] = [std::ptr::null_mut(); 2];
    let complete = !context.is_null()
        && run(
            api,
            mainloop,
            context,
            deadline,
            &sinks,
            &sources,
            &mut operations,
        );
    // SAFETY: each pointer is null or owned here. A cancelled operation
    // never calls back, so the collectors are not used after this.
    unsafe {
        for operation in operations.into_iter().filter(|op| !op.is_null()) {
            (api.operation_cancel)(operation);
            (api.operation_unref)(operation);
        }
        if !context.is_null() {
            (api.context_disconnect)(context);
            (api.context_unref)(context);
        }
        (api.mainloop_free)(mainloop);
    }
    complete.then_some(Lists {
        sinks: sinks.entries.into_inner(),
        sources: sources.entries.into_inner(),
        monitors: sources.monitors.into_inner(),
        sink_indexes: sinks.indexes.into_inner(),
        source_indexes: sources.indexes.into_inner(),
    })
}

/// Connects the context and waits until it is ready. `false` on a failed
/// connection or when the time runs out.
fn connect_ready(
    api: &Api,
    mainloop: *mut Mainloop,
    context: *mut Context,
    deadline: Instant,
) -> bool {
    // SAFETY: the context is live. No server name: the client
    // configuration and environment choose it, as for libwebrtc.
    if unsafe {
        (api.context_connect)(
            context,
            std::ptr::null(),
            PA_CONTEXT_NOAUTOSPAWN,
            std::ptr::null(),
        )
    } < 0
    {
        return false;
    }
    loop {
        // SAFETY: the context is live.
        match unsafe { (api.context_get_state)(context) } {
            PA_CONTEXT_READY => return true,
            PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => return false,
            _ => {}
        }
        if Instant::now() >= deadline || !step(api, mainloop) {
            return false;
        }
    }
}

/// Connects and reads both lists. `true` only when both lists ended well.
fn run(
    api: &Api,
    mainloop: *mut Mainloop,
    context: *mut Context,
    deadline: Instant,
    sinks: &Collector,
    sources: &Collector,
    operations: &mut [*mut Operation; 2],
) -> bool {
    if !connect_ready(api, mainloop, context, deadline) {
        return false;
    }
    // SAFETY: the context is ready; each collector outlives its operation
    // (`query` cancels the operations before the collectors go).
    unsafe {
        operations[0] = (api.get_sink_info_list)(context, on_sink, sinks.userdata());
        operations[1] = (api.get_source_info_list)(context, on_source, sources.userdata());
    }
    if operations.iter().any(|op| op.is_null()) {
        return false;
    }
    while !(sinks.done.get() && sources.done.get()) {
        if Instant::now() >= deadline || !step(api, mainloop) {
            return false;
        }
    }
    !(sinks.failed.get() || sources.failed.get())
}

// ---------------------------------------------------------------------------
// The call's own session (ADR-0049, amendment 2026-11-27)
// ---------------------------------------------------------------------------
//
// One connection opened before containment stage 2 and kept for the call. It
// moves this process's own streams to another microphone or speaker. Stage 2
// forbids a new socket, so every later operation runs on this one context.
// The main loop is the blocking `pa_mainloop`, which is not thread safe:
// the lock lets one operation run at a time, each on a blocking thread, with
// a deadline. The event loop never waits on it.

/// The longest one operation (a snapshot or a move) may take.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest a whole change (all its operations, and an undo) may take.
/// The event loop waits for no longer.
pub const CHANGE_TIMEOUT: Duration = Duration::from_secs(5);
/// The time that the caller waits beyond [`CHANGE_TIMEOUT`] for the answer.
/// The task's own deadlines (`Deadlines::starting_at` the caller's start)
/// end at `CHANGE_TIMEOUT`, so a task that keeps them is always heard. A
/// timeout after this grace means that the blocking thread did not run in
/// time.
pub const REPORT_GRACE: Duration = Duration::from_millis(250);
/// The part of [`CHANGE_TIMEOUT`] kept for the rollback. Forward moves and
/// reads must end by `CHANGE_TIMEOUT - ROLLBACK_RESERVE`; the rollback and
/// its check use the whole time. A move that has no time left is not sent.
const ROLLBACK_RESERVE: Duration = Duration::from_millis(1500);
/// The most streams read from one listing; own streams are picked after.
const MAX_STREAMS: usize = 4096;
/// The property that holds the process ID of a stream's program.
const PROCESS_ID_KEY: &CStr = c"application.process.id";

type ServerCallback = unsafe extern "C" fn(*mut Context, *const ServerInfoPrefix, *mut c_void);
type StreamCallback = unsafe extern "C" fn(*mut Context, *const c_void, c_int, *mut c_void);
type SuccessCallback = unsafe extern "C" fn(*mut Context, c_int, *mut c_void);

/// The leading fields of `pa_server_info` up to `default_source_name`
/// (pulse/introspect.h).
#[repr(C)]
struct ServerInfoPrefix {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: SampleSpec,
    default_sink_name: *const c_char,
    default_source_name: *const c_char,
}

/// The leading fields of `pa_sink_input_info` up to `proplist`.
#[repr(C)]
struct SinkInputPrefix {
    index: u32,
    name: *const c_char,
    owner_module: u32,
    client: u32,
    sink: u32,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    volume: CVolume,
    buffer_usec: u64,
    sink_usec: u64,
    resample_method: *const c_char,
    driver: *const c_char,
    mute: c_int,
    proplist: *const c_void,
}

/// The leading fields of `pa_source_output_info` up to `proplist`.
#[repr(C)]
struct SourceOutputPrefix {
    index: u32,
    name: *const c_char,
    owner_module: u32,
    client: u32,
    source: u32,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    buffer_usec: u64,
    source_usec: u64,
    resample_method: *const c_char,
    driver: *const c_char,
    proplist: *const c_void,
}

/// The extra library functions the session needs. A missing one makes the
/// session unavailable, and nothing else.
struct SessionApi {
    get_server_info:
        unsafe extern "C" fn(*mut Context, ServerCallback, *mut c_void) -> *mut Operation,
    get_sink_input_info_list:
        unsafe extern "C" fn(*mut Context, StreamCallback, *mut c_void) -> *mut Operation,
    get_source_output_info_list:
        unsafe extern "C" fn(*mut Context, StreamCallback, *mut c_void) -> *mut Operation,
    move_sink_input_by_name: unsafe extern "C" fn(
        *mut Context,
        u32,
        *const c_char,
        SuccessCallback,
        *mut c_void,
    ) -> *mut Operation,
    move_source_output_by_name: unsafe extern "C" fn(
        *mut Context,
        u32,
        *const c_char,
        SuccessCallback,
        *mut c_void,
    ) -> *mut Operation,
    proplist_gets: unsafe extern "C" fn(*const c_void, *const c_char) -> *const c_char,
}

fn session_api() -> Option<&'static SessionApi> {
    static API: OnceLock<Option<SessionApi>> = OnceLock::new();
    API.get_or_init(|| {
        let handle = library()?;
        // SAFETY: each type below is the C signature from pulse/*.h.
        unsafe {
            Some(SessionApi {
                get_server_info: symbol(handle, c"pa_context_get_server_info")?,
                get_sink_input_info_list: symbol(handle, c"pa_context_get_sink_input_info_list")?,
                get_source_output_info_list: symbol(
                    handle,
                    c"pa_context_get_source_output_info_list",
                )?,
                move_sink_input_by_name: symbol(handle, c"pa_context_move_sink_input_by_name")?,
                move_source_output_by_name: symbol(
                    handle,
                    c"pa_context_move_source_output_by_name",
                )?,
                proplist_gets: symbol(handle, c"pa_proplist_gets")?,
            })
        }
    })
    .as_ref()
}

/// The process ID in a `application.process.id` value. Only plain digits.
fn parse_process_id(value: &[u8]) -> Option<u32> {
    if value.is_empty() || value.len() > 10 || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// A short name from the library, or `None` when it is null, too long or
/// not UTF-8.
///
/// # Safety
///
/// `name` is null or a C string that lives for the call.
unsafe fn copy_name(name: *const c_char) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // SAFETY: a C string from the library, as the caller states.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    if bytes.len() > MAX_PULSE_NAME_BYTES {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// The server's default names, as its callback fills them.
#[derive(Default)]
struct ServerDefaults {
    sink: RefCell<Option<String>>,
    source: RefCell<Option<String>>,
    done: Cell<bool>,
}

unsafe extern "C" fn on_server_info(
    _context: *mut Context,
    info: *const ServerInfoPrefix,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the ServerDefaults that `Inner::defaults` passes
    // and keeps alive until the operation is cancelled.
    let state = unsafe { &*userdata.cast::<ServerDefaults>() };
    if !info.is_null() {
        // SAFETY: the library gives a valid pa_server_info for this call.
        unsafe {
            *state.sink.borrow_mut() = copy_name((*info).default_sink_name);
            *state.source.borrow_mut() = copy_name((*info).default_source_name);
        }
    }
    state.done.set(true);
}

/// One stream as `(index, owner process ID, device index)`.
type RawStream = (u32, Option<u32>, Option<u32>);

/// A stream listing of [`RawStream`] entries.
#[derive(Default)]
struct StreamList {
    entries: RefCell<Vec<RawStream>>,
    done: Cell<bool>,
    failed: Cell<bool>,
}

impl StreamList {
    /// Copies one stream.
    ///
    /// # Safety
    ///
    /// `proplist` is null or a property list that lives for the call.
    unsafe fn add(&self, index: u32, device: u32, proplist: *const c_void) {
        if self.entries.borrow().len() >= MAX_STREAMS {
            return;
        }
        let owner = session_api()
            .filter(|_| !proplist.is_null())
            .and_then(|api| {
                // SAFETY: the property list is live, as the caller states.
                let value = unsafe { (api.proplist_gets)(proplist, PROCESS_ID_KEY.as_ptr()) };
                // SAFETY: null or a C string owned by the property list.
                (!value.is_null()).then(|| unsafe { CStr::from_ptr(value) }.to_bytes())
            })
            .and_then(parse_process_id);
        let device = (device != PA_INVALID_INDEX).then_some(device);
        self.entries.borrow_mut().push((index, owner, device));
    }

    fn userdata(&self) -> *mut c_void {
        std::ptr::from_ref(self).cast_mut().cast()
    }
}

unsafe extern "C" fn on_sink_input(
    _context: *mut Context,
    info: *const c_void,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: userdata is the StreamList that `Inner::streams` passes and
    // keeps alive until the operation is cancelled.
    let list = unsafe { &*userdata.cast::<StreamList>() };
    if eol != 0 {
        list.done.set(true);
        list.failed.set(eol < 0);
    } else if !info.is_null() {
        // SAFETY: the library gives a valid pa_sink_input_info.
        let info = unsafe { &*info.cast::<SinkInputPrefix>() };
        // SAFETY: the property list lives for this call.
        unsafe { list.add(info.index, info.sink, info.proplist) };
    }
}

unsafe extern "C" fn on_source_output(
    _context: *mut Context,
    info: *const c_void,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: as in `on_sink_input`.
    let list = unsafe { &*userdata.cast::<StreamList>() };
    if eol != 0 {
        list.done.set(true);
        list.failed.set(eol < 0);
    } else if !info.is_null() {
        // SAFETY: the library gives a valid pa_source_output_info.
        let info = unsafe { &*info.cast::<SourceOutputPrefix>() };
        // SAFETY: the property list lives for this call.
        unsafe { list.add(info.index, info.source, info.proplist) };
    }
}

/// The answer to one move.
#[derive(Default)]
struct Success {
    done: Cell<bool>,
    ok: Cell<bool>,
}

unsafe extern "C" fn on_success(_context: *mut Context, success: c_int, userdata: *mut c_void) {
    // SAFETY: userdata is the Success that `Inner::move_one` passes and keeps
    // alive until the operation is cancelled.
    let state = unsafe { &*userdata.cast::<Success>() };
    state.ok.set(success != 0);
    state.done.set(true);
}

/// The own streams of one listing with the name of the device each is on.
/// `devices` pairs each device index of the same kind with its name.
fn route_streams(
    streams: Vec<RawStream>,
    own_process: u32,
    devices: &[(u32, &str)],
) -> Vec<OwnStream> {
    own_streams(streams, own_process, devices)
}

/// The connection of one call. Every use holds the session lock.
struct Inner {
    api: &'static Api,
    ext: &'static SessionApi,
    mainloop: *mut Mainloop,
    context: *mut Context,
}

// SAFETY: the main loop and the context are used by one thread at a time (the
// session lock), and neither has thread affinity.
unsafe impl Send for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: both are owned here and not used again. Disconnecting
        // closes the existing socket and opens nothing.
        unsafe {
            (self.api.context_disconnect)(self.context);
            (self.api.context_unref)(self.context);
            (self.api.mainloop_free)(self.mainloop);
        }
    }
}

/// Which kind of own stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Recording,
    Playout,
}

impl Kind {
    /// The place of this kind in a pair of per-kind values.
    fn slot(self) -> usize {
        match self {
            Self::Recording => 0,
            Self::Playout => 1,
        }
    }

    /// The name used in diagnostic notes.
    fn label(self) -> &'static str {
        match self {
            Self::Recording => "microphone",
            Self::Playout => "speaker",
        }
    }
}

/// How far one operation got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// The request never went to the server: no ready context, no time left
    /// or the library refused it.
    NotSent,
    /// The request went out and no answer came in time. The server may still
    /// act on it: cancelling an operation only drops its callback.
    TimedOut,
    /// The answer came.
    Done,
}

/// The result of one move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveOutcome {
    /// The server accepted the move.
    Moved,
    /// The server refused it, or it was never sent: the stream did not move.
    Rejected,
    /// The request went out and no answer came: the stream may have moved.
    Unknown,
}

/// The own streams and the device names, as one read shows them.
struct Snapshot {
    sources: Vec<String>,
    sinks: Vec<String>,
    /// The monitor sources, only to find a call-start default.
    monitors: Vec<String>,
    recording: Vec<OwnStream>,
    playout: Vec<OwnStream>,
}

impl Snapshot {
    fn streams(&self, kind: Kind) -> &[OwnStream] {
        match kind {
            Kind::Recording => &self.recording,
            Kind::Playout => &self.playout,
        }
    }
}

/// What a change needs from the sound server. `Inner` is the real one; the
/// tests use a fake.
trait Server {
    /// Reads the device names and the own streams with their routes.
    fn snapshot(&self, deadline: Instant) -> Option<Snapshot>;
    /// Moves one stream to the device with this name.
    fn move_stream(&self, deadline: Instant, kind: Kind, index: u32, name: &str) -> MoveOutcome;
}

impl Inner {
    /// Starts one operation with `start` and runs the main loop until `done`
    /// or the deadline, then cancels it, so its callback never runs after
    /// the caller's state is gone. Nothing is started when no time is left.
    fn drive(
        &self,
        deadline: Instant,
        start: impl FnOnce(&Self) -> *mut Operation,
        done: impl Fn() -> bool,
    ) -> Run {
        // SAFETY: the context is live.
        if unsafe { (self.api.context_get_state)(self.context) } != PA_CONTEXT_READY {
            return Run::NotSent;
        }
        let now = Instant::now();
        if now >= deadline {
            return Run::NotSent;
        }
        // One operation never runs longer than `OPERATION_TIMEOUT`.
        let deadline = deadline.min(now + OPERATION_TIMEOUT);
        let operation = start(self);
        if operation.is_null() {
            return Run::NotSent;
        }
        while !done() && Instant::now() < deadline && step(self.api, self.mainloop) {}
        // SAFETY: the operation is owned here.
        unsafe {
            (self.api.operation_cancel)(operation);
            (self.api.operation_unref)(operation);
        }
        if done() { Run::Done } else { Run::TimedOut }
    }

    /// The server's default microphone and speaker names now. `None` when
    /// the query failed; `Some` with `None` names when the server has no
    /// default.
    fn defaults(&self, deadline: Instant) -> Option<(Option<String>, Option<String>)> {
        let state = ServerDefaults::default();
        let run = self.drive(
            deadline,
            // SAFETY: the context is live; the state outlives the operation.
            |me| unsafe { (me.ext.get_server_info)(me.context, on_server_info, state_ptr(&state)) },
            || state.done.get(),
        );
        (run == Run::Done).then(|| (state.source.into_inner(), state.sink.into_inner()))
    }

    /// The sound server's streams of one kind as
    /// `(index, process ID, device index)`.
    fn streams(&self, deadline: Instant, kind: Kind) -> Option<Vec<RawStream>> {
        let list = StreamList::default();
        let run = self.drive(
            deadline,
            // SAFETY: the context is live; the list outlives the operation.
            |me| unsafe {
                match kind {
                    Kind::Recording => (me.ext.get_source_output_info_list)(
                        me.context,
                        on_source_output,
                        list.userdata(),
                    ),
                    Kind::Playout => (me.ext.get_sink_input_info_list)(
                        me.context,
                        on_sink_input,
                        list.userdata(),
                    ),
                }
            },
            || list.done.get(),
        );
        (run == Run::Done && !list.failed.get()).then(|| list.entries.into_inner())
    }

    /// The device lists, as the one-shot query reads them, on this context.
    fn lists(&self, deadline: Instant) -> Option<Lists> {
        let sinks = Collector::default();
        let sources = Collector::default();
        let sinks_ok = self.drive(
            deadline,
            // SAFETY: the context is live; the collector outlives the call.
            |me| unsafe { (me.api.get_sink_info_list)(me.context, on_sink, sinks.userdata()) },
            || sinks.done.get(),
        ) == Run::Done;
        let sources_ok = sinks_ok
            && self.drive(
                deadline,
                // SAFETY: as above.
                |me| unsafe {
                    (me.api.get_source_info_list)(me.context, on_source, sources.userdata())
                },
                || sources.done.get(),
            ) == Run::Done;
        (sources_ok && !sinks.failed.get() && !sources.failed.get()).then(|| Lists {
            sinks: sinks.entries.into_inner(),
            sources: sources.entries.into_inner(),
            monitors: sources.monitors.into_inner(),
            sink_indexes: sinks.indexes.into_inner(),
            source_indexes: sources.indexes.into_inner(),
        })
    }
}

impl Server for Inner {
    fn snapshot(&self, deadline: Instant) -> Option<Snapshot> {
        let lists = self.lists(deadline)?;
        let recording = self.streams(deadline, Kind::Recording)?;
        let playout = self.streams(deadline, Kind::Playout)?;
        let own = std::process::id();
        // Monitors are known for routes, but never listed as choices.
        let recording = route_streams(recording, own, &lists.recording_devices());
        let playout = route_streams(playout, own, &lists.playout_devices());
        let monitors = lists.monitors.into_iter().map(|(name, _)| name).collect();
        Some(Snapshot {
            recording,
            playout,
            monitors,
            sources: lists.sources.into_iter().map(|(name, _)| name).collect(),
            sinks: lists.sinks.into_iter().map(|(name, _)| name).collect(),
        })
    }

    fn move_stream(&self, deadline: Instant, kind: Kind, index: u32, name: &str) -> MoveOutcome {
        let Ok(name) = std::ffi::CString::new(name) else {
            return MoveOutcome::Rejected;
        };
        let state = Success::default();
        let userdata = std::ptr::from_ref(&state).cast_mut().cast();
        let run = self.drive(
            deadline,
            // SAFETY: the context is live; the name and the state outlive the
            // operation.
            |me| unsafe {
                match kind {
                    Kind::Recording => (me.ext.move_source_output_by_name)(
                        me.context,
                        index,
                        name.as_ptr(),
                        on_success,
                        userdata,
                    ),
                    Kind::Playout => (me.ext.move_sink_input_by_name)(
                        me.context,
                        index,
                        name.as_ptr(),
                        on_success,
                        userdata,
                    ),
                }
            },
            || state.done.get(),
        );
        match run {
            Run::NotSent => MoveOutcome::Rejected,
            Run::TimedOut => MoveOutcome::Unknown,
            Run::Done if state.ok.get() => MoveOutcome::Moved,
            Run::Done => MoveOutcome::Rejected,
        }
    }
}

fn state_ptr(state: &ServerDefaults) -> *mut c_void {
    std::ptr::from_ref(state).cast_mut().cast()
}

/// The targets that wait for a stream. The server's own route of a stream is
/// the truth, as the server moves streams by itself when a device goes away:
/// nothing here caches where the streams are.
#[derive(Default)]
struct Devices {
    /// At most one waiting target per kind (microphone, speaker; see
    /// [`Kind::slot`]), from `SoundTarget::Wait`.
    pending: [Option<String>; 2],
    /// Per kind: an earlier try of the waiting target left the routes of
    /// this kind not verified as before (a move failed and could not be
    /// undone). The last try then reports `AudioDevice`, never `DeviceKept`,
    /// even when the routes it reads are unchanged since the earlier try.
    lost: [bool; 2],
    /// Per kind: a try moved a stream of this kind to its waiting target
    /// since the last accepted change. A failure of the other kind at the
    /// last try is then `AudioDevice`, not `DeviceKept`: one route of the
    /// change moved, and `DeviceKept` says that the call runs on the
    /// devices it had.
    applied: [bool; 2],
    /// Counts the accepted changes. A try runs only for the change that was
    /// current when it was scheduled, so a try that runs late never touches
    /// the targets of a newer change.
    generation: u64,
}

/// The deadlines of one change. Forward moves must end by `forward`, so
/// the rollback still has time up to `full`. The forward time is in three
/// parts: the routes are read by `read`, the microphone's moves end by
/// `first`, and the speaker's by `forward` ([`Deadlines::forward_for`]). So
/// neither a slow read nor a slow microphone move uses up a later step's
/// time.
#[derive(Clone, Copy)]
struct Deadlines {
    read: Instant,
    first: Instant,
    forward: Instant,
    full: Instant,
}

impl Deadlines {
    /// The deadlines of a change that starts at `start`.
    fn starting_at(start: Instant) -> Self {
        let full = start + CHANGE_TIMEOUT;
        let forward = full.checked_sub(ROLLBACK_RESERVE).unwrap_or(start);
        let part = forward.saturating_duration_since(start) / 3;
        Self {
            read: start + part,
            first: start + part * 2,
            forward,
            full,
        }
    }

    /// The end of the forward moves of `kind`. The microphone goes first and
    /// has the second third of the forward time, after the read, so a slow
    /// microphone move never uses up the speaker's: each kind gets its own
    /// bounded try.
    fn forward_for(&self, kind: Kind) -> Instant {
        match kind {
            Kind::Recording => self.first,
            Kind::Playout => self.forward,
        }
    }
}

/// The call's sound server connection and what it knows of the devices.
pub struct Session {
    inner: Mutex<Inner>,
    /// The server's default microphone and speaker when the call started.
    default_source: Option<String>,
    default_sink: Option<String>,
    /// Whether the defaults query worked. `false` is unknown, not "none".
    defaults_known: bool,
    devices: Mutex<Devices>,
    /// Whether a target waits, and the generation of the accepted change.
    /// Read without the lock, so the event loop never waits for a running
    /// change to ask.
    waiting: AtomicBool,
    generation: AtomicU64,
}

impl Session {
    /// Opens the connection and reads the defaults. `None` when the library
    /// or a function is missing, the server does not answer, or the time runs
    /// out. A defaults query that fails does not end the session: the
    /// defaults are then unknown, and [`Session::switch`] refuses every
    /// change. It blocks for at most about [`OPERATION_TIMEOUT`]. Call it
    /// before containment stage 2: it opens a new sound server socket.
    pub fn open() -> Option<Self> {
        let (api, ext) = (api()?, session_api()?);
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        // SAFETY: plain constructor.
        let mainloop = unsafe { (api.mainloop_new)() };
        if mainloop.is_null() {
            return None;
        }
        // SAFETY: the main loop is live; the name is a C string.
        let context =
            unsafe { (api.context_new)((api.mainloop_get_api)(mainloop), c"Persephone".as_ptr()) };
        if context.is_null() {
            // SAFETY: owned here.
            unsafe { (api.mainloop_free)(mainloop) };
            return None;
        }
        // From here `Inner` owns both and frees them.
        let inner = Inner {
            api,
            ext,
            mainloop,
            context,
        };
        if !connect_ready(api, mainloop, context, deadline) {
            return None;
        }
        let defaults = inner.defaults(deadline);
        let defaults_known = defaults.is_some();
        let (default_source, default_sink) = defaults.unwrap_or((None, None));
        Some(Self {
            inner: Mutex::new(inner),
            default_source,
            default_sink,
            defaults_known,
            devices: Mutex::new(Devices::default()),
            waiting: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        })
    }

    /// Whether a target waits for a stream.
    pub fn has_pending(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }

    /// The generation of the accepted change. The caller reads it when it
    /// schedules a try and gives it to [`Session::apply_pending`].
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Moves the call's own streams to the chosen microphone and speaker
    /// (ADR-0059). `started` is when the caller began the change; the
    /// deadlines count from it, so a task that starts late or waits for the
    /// lock finds them passed and sends nothing. The whole change takes at
    /// most [`CHANGE_TIMEOUT`] from `started`; call it on a blocking thread.
    /// With unknown defaults, every change is `Unsupported`. Both choices
    /// are checked before either stream moves. A kind with no own stream yet
    /// waits for one and counts as done (`apply_pending` moves it later). A
    /// stream that cannot move stays where it was. `DeviceKept` when every
    /// move is undone and the server shows every own stream on its old
    /// device, `AudioDevice` when not.
    pub fn switch(
        &self,
        started: Instant,
        recording: Option<&str>,
        playout: Option<&str>,
    ) -> Result<(), FailureCode> {
        if !self.defaults_known {
            return Err(FailureCode::Unsupported);
        }
        let deadlines = Deadlines::starting_at(started);
        let inner = self.inner.lock().map_err(|_| FailureCode::AudioDevice)?;
        let mut devices = self.devices.lock().map_err(|_| FailureCode::AudioDevice)?;
        let result = run_change(
            &*inner,
            (self.default_source.as_deref(), self.default_sink.as_deref()),
            &mut devices,
            (recording, playout),
            deadlines,
        );
        self.publish(&devices);
        result
    }

    /// Moves the streams of each kind that has a waiting target and an own
    /// stream now. Gives whether a target still waits, and the moves that
    /// failed. A kind without a stream is not a failure. A failed target
    /// stays for the next try, except when `last` is set (the last try of a
    /// series): then it is dropped, and the caller reports the failure. The
    /// last try also reports a kind whose routes it could not read. A try
    /// for an older `generation` than the accepted change does nothing. Same
    /// start, bound and thread rule as [`Session::switch`].
    pub fn apply_pending(&self, started: Instant, last: bool, generation: u64) -> PendingRun {
        let deadlines = Deadlines::starting_at(started);
        let (Ok(inner), Ok(mut devices)) = (self.inner.lock(), self.devices.lock()) else {
            return PendingRun::default();
        };
        let run = run_pending(&*inner, &mut devices, (last, generation), deadlines);
        self.publish(&devices);
        run
    }

    /// Copies what the event loop reads without the lock.
    fn publish(&self, devices: &Devices) {
        self.waiting.store(
            devices.pending.iter().any(Option::is_some),
            Ordering::Relaxed,
        );
        self.generation.store(devices.generation, Ordering::Relaxed);
    }
}

/// The result of one try of the waiting targets.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PendingRun {
    /// Whether a target still waits.
    pub waiting: bool,
    /// The kinds (`"microphone"`, `"speaker"`) whose move was refused or
    /// lost, with the code to report. On the last try, also a kind whose
    /// routes could not be read. A kind whose stream is not open yet is not
    /// here: it only waits.
    pub moves_failed: Vec<(&'static str, FailureCode)>,
}

/// Both kinds, in the order of their slots.
const KINDS: [Kind; 2] = [Kind::Recording, Kind::Playout];

/// `DeviceKept` when the streams are back on their old device, else
/// `AudioDevice`.
fn failure(kept: bool) -> FailureCode {
    if kept {
        FailureCode::DeviceKept
    } else {
        FailureCode::AudioDevice
    }
}

/// Moves each own stream of `kind` that is not on `name`, in order, and
/// stops at the first failure. `touched` gets each stream that moved or may
/// have moved, with the device it was on, so the caller can undo it. A
/// stream whose request was refused did not move and is not listed. `true`
/// when every stream is on `name` now.
fn move_to(
    server: &impl Server,
    deadline: Instant,
    kind: Kind,
    name: &str,
    streams: &[OwnStream],
    touched: &mut Vec<OwnStream>,
) -> bool {
    for stream in streams
        .iter()
        .filter(|stream| stream.device.as_deref() != Some(name))
    {
        match server.move_stream(deadline, kind, stream.index, name) {
            MoveOutcome::Moved => touched.push(stream.clone()),
            MoveOutcome::Unknown => {
                touched.push(stream.clone());
                return false;
            }
            MoveOutcome::Rejected => return false,
        }
    }
    true
}

/// Moves each touched stream back to the device it was on. A stream whose
/// old device was not known cannot go back. Every stream is tried. `true`
/// when all went back.
fn undo(server: &impl Server, deadline: Instant, kind: Kind, touched: &[OwnStream]) -> bool {
    touched.iter().fold(true, |all, stream| {
        let back = stream.device.as_deref().is_some_and(|old| {
            server.move_stream(deadline, kind, stream.index, old) == MoveOutcome::Moved
        });
        all && back
    })
}

/// One change of microphone and speaker. The new targets are local: they
/// replace `devices.pending` only when every immediate move worked, so a
/// refused change leaves the earlier accepted state alone.
fn run_change(
    server: &impl Server,
    defaults: (Option<&str>, Option<&str>),
    devices: &mut Devices,
    choice: (Option<&str>, Option<&str>),
    deadlines: Deadlines,
) -> Result<(), FailureCode> {
    let snapshot = server
        .snapshot(deadlines.read)
        .ok_or(FailureCode::AudioDevice)?;
    let source_names: Vec<&str> = snapshot.sources.iter().map(String::as_str).collect();
    let sink_names: Vec<&str> = snapshot.sinks.iter().map(String::as_str).collect();
    let monitor_names: Vec<&str> = snapshot.monitors.iter().map(String::as_str).collect();
    let plan = plan_sound_move(
        choice.0,
        choice.1,
        &SoundKnown {
            default_source: defaults.0,
            default_sink: defaults.1,
        },
        &SoundNow {
            sources: &source_names,
            sinks: &sink_names,
            monitors: &monitor_names,
            own_recording: &snapshot.recording,
            own_playout: &snapshot.playout,
        },
    )?;
    let targets = [(Kind::Recording, &plan.source), (Kind::Playout, &plan.sink)];
    let mut touched: [Vec<OwnStream>; 2] = [Vec::new(), Vec::new()];
    let mut moved = true;
    for (kind, target) in targets {
        if let SoundTarget::Move(name) = target {
            moved = moved
                && move_to(
                    server,
                    deadlines.forward_for(kind),
                    kind,
                    name,
                    snapshot.streams(kind),
                    &mut touched[kind.slot()],
                );
        }
    }
    if moved {
        // Commit: every immediate move worked. The answer reports them, so
        // a later try reports only what it moves itself.
        for (kind, target) in targets {
            let slot = kind.slot();
            devices.pending[slot] = match target {
                SoundTarget::Wait(name) => Some(name.clone()),
                SoundTarget::Move(_) | SoundTarget::Keep => None,
            };
            devices.lost[slot] = false;
            devices.applied[slot] = false;
        }
        devices.generation = devices.generation.wrapping_add(1);
        return Ok(());
    }
    // Roll back every kind that moved or may have moved, then read the
    // routes again: only the server's answer shows that the streams are
    // back. A move whose reply timed out may have happened.
    let back_playout = undo(server, deadlines.full, Kind::Playout, &touched[1]);
    let back_recording = undo(server, deadlines.full, Kind::Recording, &touched[0]);
    let kept = back_playout
        && back_recording
        && server.snapshot(deadlines.full).is_some_and(|now| {
            routes_kept(&snapshot.recording, &now.recording)
                && routes_kept(&snapshot.playout, &now.playout)
        });
    Err(failure(kept))
}

/// One try of the waiting targets. Same commit rule as [`run_change`].
/// `try_of` is whether this is the last try, and the generation of the
/// change that it was scheduled for: a try of an older change returns at
/// once and changes nothing, so it never drops a newer target.
fn run_pending(
    server: &impl Server,
    devices: &mut Devices,
    try_of: (bool, u64),
    deadlines: Deadlines,
) -> PendingRun {
    let (last, generation) = try_of;
    let mut run = PendingRun {
        waiting: devices.pending.iter().any(Option::is_some),
        moves_failed: Vec::new(),
    };
    if !run.waiting || devices.generation != generation {
        return run;
    }
    // The read has its own part of the forward time, so a slow read never
    // leaves a move unsent for want of time.
    let Some(snapshot) = server.snapshot(deadlines.read) else {
        // The routes cannot be read. An earlier try waits silently; the last
        // try reports every waiting kind, as the outcome is not known.
        if last {
            for kind in KINDS {
                let slot = kind.slot();
                if devices.pending[slot].take().is_some() {
                    devices.lost[slot] = false;
                    run.moves_failed
                        .push((kind.label(), FailureCode::AudioDevice));
                }
            }
        }
        run.waiting = devices.pending.iter().any(Option::is_some);
        return run;
    };
    // First the forward moves of every kind, each under the forward
    // deadline, so the rollback of one kind never uses up the forward time
    // of the other. Each result is the streams that moved (or may have
    // moved) and whether every stream is on the target now.
    let mut tries: [Option<(Vec<OwnStream>, bool)>; 2] = [None, None];
    for kind in KINDS {
        let slot = kind.slot();
        let streams = snapshot.streams(kind);
        // No stream yet: the target waits and nothing failed.
        let Some(name) = devices.pending[slot]
            .as_deref()
            .filter(|_| !streams.is_empty())
        else {
            continue;
        };
        let mut touched = Vec::new();
        let moved = move_to(
            server,
            deadlines.forward_for(kind),
            kind,
            name,
            streams,
            &mut touched,
        );
        // A stream that moved to its target changes the call's routes,
        // also when the other kind fails in this try. So does a stream that
        // an earlier try left on its target (a move that timed out but
        // happened, and could not be undone: `lost`), although this try
        // moves nothing.
        devices.applied[slot] |= moved && (!touched.is_empty() || devices.lost[slot]);
        tries[slot] = Some((touched, moved));
    }
    // Then the commit and the rollback of each kind, under the full
    // deadline.
    for kind in KINDS {
        let slot = kind.slot();
        let Some((touched, moved)) = tries[slot].take() else {
            continue;
        };
        if moved {
            devices.pending[slot] = None;
            devices.lost[slot] = false;
            continue;
        }
        let streams = snapshot.streams(kind);
        let back = undo(server, deadlines.full, kind, &touched);
        let kept = back
            && server
                .snapshot(deadlines.full)
                .is_some_and(|now| routes_kept(streams, now.streams(kind)));
        if last {
            // The routes that were read may already be split by an earlier
            // try: the kind is then lost, whatever they show now. A route of
            // the other kind that a try moved is not the call's old device.
            let other = 1 - slot;
            let code = failure(kept && !devices.lost[slot] && !devices.applied[other]);
            devices.pending[slot] = None;
            devices.lost[slot] = false;
            run.moves_failed.push((kind.label(), code));
        } else {
            devices.lost[slot] |= !kept;
            run.moves_failed.push((kind.label(), failure(kept)));
        }
    }
    run.waiting = devices.pending.iter().any(Option::is_some);
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn given_the_c_headers_then_the_info_prefixes_have_their_offsets() {
        // GIVEN the C layout of pa_sink_info and pa_source_info on 64-bit
        // Linux (measured with offsetof against pulse/introspect.h), WHEN
        // the Rust prefixes are laid out, THEN each read field is in place.
        assert_eq!(offset_of!(SinkInfoPrefix, name), 0);
        assert_eq!(offset_of!(SinkInfoPrefix, description), 16);
        assert_eq!(offset_of!(SourceInfoPrefix, name), 0);
        assert_eq!(offset_of!(SourceInfoPrefix, description), 16);
        assert_eq!(offset_of!(SourceInfoPrefix, monitor_of_sink), 308);
    }

    #[test]
    fn given_the_c_headers_then_the_session_prefixes_have_their_offsets() {
        // GIVEN the C layout of pa_server_info, pa_sink_input_info and
        // pa_source_output_info on 64-bit Linux (computed from
        // pulse/introspect.h with the C alignment rules, and checked live on
        // PipeWire in the contained sound tests), WHEN the Rust prefixes are
        // laid out, THEN each read field is in place.
        assert_eq!(offset_of!(ServerInfoPrefix, default_sink_name), 48);
        assert_eq!(offset_of!(ServerInfoPrefix, default_source_name), 56);
        assert_eq!(offset_of!(SinkInputPrefix, index), 0);
        assert_eq!(offset_of!(SinkInputPrefix, proplist), 344);
        assert_eq!(offset_of!(SourceOutputPrefix, index), 0);
        assert_eq!(offset_of!(SourceOutputPrefix, proplist), 208);
        assert_eq!(offset_of!(SinkInputPrefix, sink), 24);
        assert_eq!(offset_of!(SourceOutputPrefix, source), 24);
    }

    #[test]
    fn given_process_id_values_then_only_plain_numbers_parse() {
        // GIVEN property values, WHEN parsed, THEN only 1 to 10 digits that
        // fit a u32 give an ID.
        assert_eq!(parse_process_id(b"4242"), Some(4242));
        assert_eq!(parse_process_id(b""), None);
        assert_eq!(parse_process_id(b"-1"), None);
        assert_eq!(parse_process_id(b"12a"), None);
        assert_eq!(parse_process_id(b" 1"), None);
        assert_eq!(parse_process_id(b"99999999999"), None);
        assert_eq!(parse_process_id(b"4294967296"), None);
    }

    #[test]
    fn given_a_stream_without_properties_when_listed_then_it_has_no_owner() {
        // GIVEN a stream with a null property list, WHEN copied, THEN it is
        // kept with no owner, so it is never picked as the call's own.
        let list = StreamList::default();
        // SAFETY: a null property list is allowed.
        unsafe { list.add(5, PA_INVALID_INDEX, std::ptr::null()) };
        assert_eq!(list.entries.into_inner(), vec![(5, None, None)]);
    }

    #[test]
    fn given_library_entries_when_copied_then_long_or_bad_names_are_left_out() {
        // GIVEN names that are fine, too long, not UTF-8 and null, WHEN
        // copied, THEN only the good one stays, with its description.
        let collector = Collector::default();
        let long = std::ffi::CString::new("a".repeat(MAX_PULSE_NAME_BYTES + 1)).unwrap();
        // SAFETY: each pointer is null or a live C string.
        unsafe {
            collector.add(c"vmic".as_ptr(), c"Virtual mic".as_ptr(), 1);
            collector.add(long.as_ptr(), c"Long".as_ptr(), 2);
            collector.add(c"\xff\xfe".as_ptr(), c"Bad".as_ptr(), 3);
            collector.add(std::ptr::null(), c"Null".as_ptr(), 4);
            collector.add(c"nodesc".as_ptr(), std::ptr::null(), 5);
        }
        assert_eq!(
            collector.entries.into_inner(),
            vec![
                ("vmic".to_owned(), "Virtual mic".to_owned()),
                ("nodesc".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn given_many_entries_when_copied_then_at_most_the_limit_is_kept() {
        // GIVEN more entries than MAX_ENTRIES, WHEN copied, THEN the rest
        // are dropped and the list still ends normally.
        let collector = Collector::default();
        for _ in 0..MAX_ENTRIES + 3 {
            // SAFETY: live C strings.
            unsafe { collector.add(c"s".as_ptr(), c"d".as_ptr(), 0) };
        }
        collector.end(1);
        assert_eq!(collector.entries.borrow().len(), MAX_ENTRIES);
        assert!(collector.done.get() && !collector.failed.get());
        let failed = Collector::default();
        failed.end(-1);
        assert!(failed.done.get() && failed.failed.get());
    }

    #[test]
    fn given_a_monitor_source_when_listed_then_it_is_left_out() {
        // GIVEN a monitor source and a real source, WHEN the callback gets
        // them, THEN only the real one is listed.
        let collector = Collector::default();
        let mut source = SourceInfoPrefix {
            name: c"sink.monitor".as_ptr(),
            index: 0,
            description: c"Monitor".as_ptr(),
            sample_spec: SampleSpec {
                format: 0,
                rate: 0,
                channels: 0,
            },
            channel_map: ChannelMap {
                channels: 0,
                map: [0; PA_CHANNELS_MAX],
            },
            owner_module: 0,
            volume: CVolume {
                channels: 0,
                values: [0; PA_CHANNELS_MAX],
            },
            mute: 0,
            monitor_of_sink: 7,
        };
        let userdata = collector.userdata();
        // SAFETY: a live struct, live strings and a live collector.
        unsafe {
            on_source(std::ptr::null_mut(), &source, 0, userdata);
            source.name = c"vmic".as_ptr();
            source.description = c"Mic".as_ptr();
            source.monitor_of_sink = PA_INVALID_INDEX;
            on_source(std::ptr::null_mut(), &source, 0, userdata);
            on_source(std::ptr::null_mut(), std::ptr::null(), 1, userdata);
        }
        assert!(collector.done.get() && !collector.failed.get());
        assert_eq!(
            collector.entries.into_inner(),
            vec![("vmic".to_owned(), "Mic".to_owned())]
        );
        // The monitor is kept apart, with its device index, for routes.
        assert_eq!(
            collector.monitors.into_inner(),
            vec![("sink.monitor".to_owned(), 0)]
        );
    }

    // ----- a fake sound server for the change logic -----

    /// A sound server in memory. `refuse` and `silent` name moves (by stream
    /// index and target) that the server refuses, or that it does but never
    /// answers. `drift` moves a stream by itself at the second read.
    #[derive(Default)]
    struct Fake {
        recording: RefCell<Vec<OwnStream>>,
        playout: RefCell<Vec<OwnStream>>,
        refuse: Vec<(u32, &'static str)>,
        silent: Vec<(u32, &'static str)>,
        drift: Option<(Kind, u32, &'static str)>,
        /// Whether a read fails.
        unreadable: Cell<bool>,
        /// Whether the first read takes all of its time.
        slow_read: Cell<bool>,
        reads: Cell<usize>,
        sent: RefCell<Vec<(Kind, u32, String)>>,
    }

    fn at(index: u32, device: &str) -> OwnStream {
        OwnStream {
            index,
            device: Some(device.to_owned()),
        }
    }

    impl Fake {
        fn with(recording: Vec<OwnStream>, playout: Vec<OwnStream>) -> Self {
            Self {
                recording: RefCell::new(recording),
                playout: RefCell::new(playout),
                ..Self::default()
            }
        }

        fn place(&self, kind: Kind, index: u32, name: &str) {
            let streams = match kind {
                Kind::Recording => &self.recording,
                Kind::Playout => &self.playout,
            };
            for stream in streams.borrow_mut().iter_mut().filter(|s| s.index == index) {
                stream.device = Some(name.to_owned());
            }
        }

        fn route(&self, kind: Kind, index: u32) -> Option<String> {
            let streams = match kind {
                Kind::Recording => self.recording.borrow(),
                Kind::Playout => self.playout.borrow(),
            };
            streams
                .iter()
                .find(|s| s.index == index)
                .and_then(|s| s.device.clone())
        }
    }

    impl Server for Fake {
        fn snapshot(&self, deadline: Instant) -> Option<Snapshot> {
            self.reads.set(self.reads.get() + 1);
            if self.unreadable.get() {
                return None;
            }
            if self.slow_read.get() && self.reads.get() == 1 {
                // A real read ends by its deadline, so it answers then.
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            if self.reads.get() == 2
                && let Some((kind, index, name)) = self.drift
            {
                self.place(kind, index, name);
            }
            Some(Snapshot {
                sources: vec!["mic-a".to_owned(), "mic-b".to_owned()],
                sinks: vec!["spk-a".to_owned(), "spk-b".to_owned()],
                monitors: vec!["spk-a.monitor".to_owned()],
                recording: self.recording.borrow().clone(),
                playout: self.playout.borrow().clone(),
            })
        }

        fn move_stream(
            &self,
            deadline: Instant,
            kind: Kind,
            index: u32,
            name: &str,
        ) -> MoveOutcome {
            if deadline <= Instant::now() {
                return MoveOutcome::Rejected;
            }
            self.sent.borrow_mut().push((kind, index, name.to_owned()));
            if self.refuse.iter().any(|(i, n)| *i == index && *n == name) {
                return MoveOutcome::Rejected;
            }
            self.place(kind, index, name);
            if self.silent.iter().any(|(i, n)| *i == index && *n == name) {
                return MoveOutcome::Unknown;
            }
            MoveOutcome::Moved
        }
    }

    fn deadlines() -> Deadlines {
        Deadlines::starting_at(Instant::now())
    }

    const DEFAULTS: (Option<&str>, Option<&str>) = (Some("mic-a"), Some("spk-a"));

    #[test]
    fn given_a_change_then_the_forward_moves_end_before_the_rollback_reserve() {
        // GIVEN a change that starts now, WHEN its deadlines are set, THEN
        // the forward moves end the reserve before the whole change does.
        let limits = deadlines();
        assert_eq!(limits.full - limits.forward, ROLLBACK_RESERVE);
        assert!(ROLLBACK_RESERVE < CHANGE_TIMEOUT);
        // The read, the microphone and the speaker each have a third of the
        // forward time, in that order, so no step uses up a later one's.
        let start = limits.full - CHANGE_TIMEOUT;
        let part = (limits.forward - start) / 3;
        assert_eq!(limits.read - start, part);
        assert_eq!(limits.first - limits.read, part);
        assert!(limits.forward - limits.first >= part);
        assert_eq!(limits.forward_for(Kind::Recording), limits.first);
        assert_eq!(limits.forward_for(Kind::Playout), limits.forward);
    }

    #[test]
    fn given_a_read_that_uses_all_its_time_then_both_kinds_still_move() {
        // GIVEN waiting targets of both kinds and a route read that takes
        // all of its time, WHEN the last try runs, THEN each move is still
        // sent within its own time and both waits end. GIVEN the same slow
        // read WHEN a change moves both kinds at once THEN both move too.
        let limits = || {
            let now = Instant::now();
            Deadlines {
                read: now + Duration::from_millis(50),
                first: now + Duration::from_secs(3),
                forward: now + Duration::from_secs(4),
                full: now + Duration::from_secs(5),
            }
        };
        let server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        server.slow_read.set(true);
        let mut devices = Devices {
            pending: [Some("mic-b".to_owned()), Some("spk-b".to_owned())],
            ..Devices::default()
        };
        let run = run_pending(&server, &mut devices, (true, 0), limits());
        assert_eq!(
            run,
            PendingRun {
                waiting: false,
                moves_failed: Vec::new(),
            }
        );
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-b"));
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-b"));

        let server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        server.slow_read.set(true);
        let mut devices = Devices::default();
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-b"), Some("pulse:spk-b")),
            limits(),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-b"));
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-b"));
    }

    #[test]
    fn given_a_microphone_move_out_of_time_then_the_speaker_still_moves() {
        // GIVEN waiting targets of both kinds and a microphone whose forward
        // time has run out, WHEN the last try runs, THEN the speaker move is
        // still sent within its own time and its wait ends; the microphone is
        // reported, as AudioDevice because the speaker route moved.
        let server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        let mut devices = Devices {
            pending: [Some("mic-b".to_owned()), Some("spk-b".to_owned())],
            ..Devices::default()
        };
        let limits = Deadlines {
            read: Instant::now(),
            first: Instant::now() - Duration::from_secs(1),
            forward: Instant::now() + Duration::from_secs(5),
            full: Instant::now() + Duration::from_secs(6),
        };
        let run = run_pending(&server, &mut devices, (true, 0), limits);
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-b"));
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-a"));
        assert_eq!(
            run,
            PendingRun {
                waiting: false,
                moves_failed: vec![("microphone", FailureCode::AudioDevice)],
            }
        );
    }

    #[test]
    fn given_an_earlier_move_not_undone_then_a_last_speaker_failure_is_audio_device() {
        // GIVEN waiting targets of both kinds, an earlier try whose
        // microphone move timed out but happened and could not be undone,
        // and a speaker that the server refuses, WHEN the last try finds the
        // microphone on its target and moves nothing, THEN the speaker's
        // failure is AudioDevice: the call does not run on the devices it
        // had.
        let server = Fake {
            silent: vec![(1, "mic-b")],
            refuse: vec![(1, "mic-a"), (2, "spk-b")],
            ..Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")])
        };
        let mut devices = Devices {
            pending: [Some("mic-b".to_owned()), Some("spk-b".to_owned())],
            ..Devices::default()
        };
        let first = run_pending(&server, &mut devices, (false, 0), deadlines());
        assert_eq!(
            first.moves_failed,
            vec![
                ("microphone", FailureCode::AudioDevice),
                ("speaker", FailureCode::DeviceKept),
            ]
        );
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-b"));
        assert!(devices.lost[Kind::Recording.slot()]);

        let last = run_pending(&server, &mut devices, (true, 0), deadlines());
        assert_eq!(
            last,
            PendingRun {
                waiting: false,
                moves_failed: vec![("speaker", FailureCode::AudioDevice)],
            }
        );
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-b"));
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-a"));
    }

    #[test]
    fn given_no_time_left_when_a_move_is_asked_then_it_is_not_sent() {
        // GIVEN a forward deadline that has passed, WHEN a change must move
        // a stream, THEN the move is not sent, nothing is touched, and the
        // routes read again show the call kept its devices.
        let server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        let mut devices = Devices::default();
        let limits = Deadlines {
            read: Instant::now(),
            first: Instant::now() - Duration::from_secs(1),
            forward: Instant::now() - Duration::from_secs(1),
            full: Instant::now() + Duration::from_secs(5),
        };
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-b"), None),
            limits,
        );
        assert_eq!(result, Err(FailureCode::DeviceKept));
        assert!(server.sent.borrow().is_empty());
    }

    #[test]
    fn given_a_good_change_then_both_streams_move_and_the_targets_are_committed() {
        // GIVEN a microphone stream and no speaker stream, WHEN both devices
        // change, THEN the microphone moves, the speaker target waits, and
        // both are recorded.
        let server = Fake::with(vec![at(1, "mic-a")], vec![]);
        let mut devices = Devices::default();
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-b"), Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-b"));
        assert_eq!(devices.pending[0], None);
        assert_eq!(devices.pending[1].as_deref(), Some("spk-b"));
    }

    #[test]
    fn given_a_rejected_move_when_a_new_target_waits_then_the_old_state_stays() {
        // GIVEN an earlier accepted waiting speaker, and a change whose
        // microphone move the server refuses while it would also queue a new
        // speaker target, WHEN the change runs, THEN it fails as kept, the
        // earlier waiting target stays, and the new one is not stored.
        let mut server = Fake::with(vec![at(1, "mic-a")], vec![]);
        server.refuse = vec![(1, "mic-b")];
        let mut devices = Devices::default();
        devices.pending[1] = Some("spk-old".to_owned());
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-b"), Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(result, Err(FailureCode::DeviceKept));
        assert_eq!(devices.pending[1].as_deref(), Some("spk-old"));
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-a"));
    }

    #[test]
    fn given_a_move_with_no_reply_then_it_is_rolled_back_like_a_move_that_worked() {
        // GIVEN a speaker move that the server did but never answered, after
        // a microphone move that worked, WHEN the change runs, THEN both
        // streams go back and the routes read again show it: DeviceKept.
        let mut server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        server.silent = vec![(2, "spk-b")];
        let mut devices = Devices::default();
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-b"), Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(result, Err(FailureCode::DeviceKept));
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-a"));
        assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-a"));
        assert!(
            server
                .sent
                .borrow()
                .contains(&(Kind::Playout, 2, "spk-a".to_owned()))
        );
    }

    #[test]
    fn given_a_failed_rollback_then_the_answer_is_a_lost_device_and_a_later_request_waits() {
        // GIVEN a speaker move with no reply, and a server that refuses the
        // move back, WHEN the change runs, THEN the answer is AudioDevice.
        let mut server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        server.silent = vec![(2, "spk-b")];
        server.refuse = vec![(2, "spk-a")];
        let mut devices = Devices::default();
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (None, Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(result, Err(FailureCode::AudioDevice));
        // WHEN the stream is gone and the old speaker is asked for, THEN the
        // target waits.
        server.playout.borrow_mut().clear();
        assert_eq!(
            run_change(
                &server,
                DEFAULTS,
                &mut devices,
                (None, Some("pulse:spk-a")),
                deadlines()
            ),
            Ok(())
        );
        assert_eq!(devices.pending[1].as_deref(), Some("spk-a"));
    }

    #[test]
    fn given_a_stream_that_the_server_moved_elsewhere_then_kept_is_not_reported() {
        // GIVEN a refused speaker move and a microphone stream that the
        // server moves by itself while the change runs, WHEN the routes are
        // read again, THEN they differ from the start, so the answer is
        // AudioDevice and not DeviceKept.
        let mut server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        server.refuse = vec![(2, "spk-b")];
        server.drift = Some((Kind::Recording, 1, "mic-b"));
        let mut devices = Devices::default();
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (None, Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(result, Err(FailureCode::AudioDevice));
    }

    #[test]
    fn given_a_stream_that_the_server_moved_to_the_target_then_no_move_is_sent() {
        // GIVEN a cache that says the call is on the old speaker, and a
        // stream that the server already shows on the chosen one, WHEN the
        // change runs, THEN nothing is sent.
        let server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-b")]);
        let mut devices = Devices::default();
        let result = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (None, Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(result, Ok(()));
        assert!(server.sent.borrow().is_empty());
        assert_eq!(devices.pending, [None, None]);
    }

    #[test]
    fn given_a_waiting_target_when_every_try_is_refused_then_the_last_one_reports_it() {
        // GIVEN a speaker target that waits and a stream that the server
        // refuses to move, WHEN a try that is not the last runs, THEN the
        // failure is listed and the target stays; WHEN the last try runs,
        // THEN the failure is listed and the target is dropped.
        let mut server = Fake::with(vec![at(1, "mic-a")], vec![at(2, "spk-a")]);
        server.refuse = vec![(2, "spk-b")];
        let mut devices = Devices::default();
        devices.pending[1] = Some("spk-b".to_owned());
        let early = run_pending(&server, &mut devices, (false, 0), deadlines());
        assert_eq!(
            early,
            PendingRun {
                waiting: true,
                moves_failed: vec![("speaker", FailureCode::DeviceKept)],
            }
        );
        let last = run_pending(&server, &mut devices, (true, 0), deadlines());
        assert_eq!(
            last,
            PendingRun {
                waiting: false,
                moves_failed: vec![("speaker", FailureCode::DeviceKept)],
            }
        );
        assert_eq!(devices.pending, [None, None]);
    }

    #[test]
    fn given_a_waiting_target_and_no_stream_yet_then_nothing_fails_and_it_keeps_waiting() {
        // GIVEN a speaker target and no playback stream, WHEN the last try
        // runs, THEN it is no failure and the target still waits.
        let server = Fake::with(vec![at(1, "mic-a")], vec![]);
        let mut devices = Devices::default();
        devices.pending[1] = Some("spk-b".to_owned());
        let run = run_pending(&server, &mut devices, (true, 0), deadlines());
        assert_eq!(
            run,
            PendingRun {
                waiting: true,
                moves_failed: Vec::new(),
            }
        );
    }

    #[test]
    fn given_a_waiting_target_and_a_stream_then_it_moves_and_the_wait_ends() {
        // GIVEN a speaker target and a playback stream that opened, WHEN a
        // try runs, THEN the stream moves and nothing waits.
        let server = Fake::with(vec![], vec![at(2, "spk-a")]);
        let mut devices = Devices::default();
        devices.pending[1] = Some("spk-b".to_owned());
        let run = run_pending(&server, &mut devices, (false, 0), deadlines());
        assert_eq!(run, PendingRun::default());
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-b"));
        assert_eq!(devices.pending, [None, None]);
    }

    #[test]
    fn given_a_waiting_target_and_a_stream_that_opened_on_it_then_nothing_moves_and_the_wait_ends()
    {
        // GIVEN a speaker target and a playback stream that opened on that
        // device by itself, WHEN a try runs, THEN no move is sent and the
        // target is cleared.
        let server = Fake::with(vec![], vec![at(2, "spk-b")]);
        let mut devices = Devices::default();
        devices.pending[1] = Some("spk-b".to_owned());
        let run = run_pending(&server, &mut devices, (false, 0), deadlines());
        assert_eq!(run, PendingRun::default());
        assert!(server.sent.borrow().is_empty());
        assert_eq!(devices.pending, [None, None]);
    }

    #[test]
    fn given_routes_that_cannot_be_read_then_only_the_last_try_reports_them() {
        // GIVEN waiting targets of both kinds and a server whose routes
        // cannot be read, WHEN a try that is not the last runs, THEN nothing
        // is reported and both targets wait; WHEN the last try runs, THEN
        // each kind is reported as AudioDevice and both targets are dropped.
        let server = Fake::with(vec![], vec![]);
        server.unreadable.set(true);
        let mut devices = Devices {
            pending: [Some("mic-b".to_owned()), Some("spk-b".to_owned())],
            ..Devices::default()
        };
        assert_eq!(
            run_pending(&server, &mut devices, (false, 0), deadlines()),
            PendingRun {
                waiting: true,
                moves_failed: Vec::new(),
            }
        );
        assert_eq!(
            run_pending(&server, &mut devices, (true, 0), deadlines()),
            PendingRun {
                waiting: false,
                moves_failed: vec![
                    ("microphone", FailureCode::AudioDevice),
                    ("speaker", FailureCode::AudioDevice),
                ],
            }
        );
        assert_eq!(devices.pending, [None, None]);
    }

    #[test]
    fn given_a_route_split_by_an_earlier_try_then_the_last_try_never_reports_kept() {
        // GIVEN two playback streams on one speaker, a server that refuses
        // the second move and the move back of the first, WHEN an early try
        // moves the first stream and cannot undo it, THEN the kind is marked
        // lost; WHEN the last try reads the split routes and fails again with
        // nothing to undo, THEN the answer is AudioDevice, not DeviceKept.
        let mut server = Fake::with(vec![], vec![at(2, "spk-a"), at(3, "spk-a")]);
        server.refuse = vec![(3, "spk-b"), (2, "spk-a")];
        let mut devices = Devices::default();
        devices.pending[1] = Some("spk-b".to_owned());
        run_pending(&server, &mut devices, (false, 0), deadlines());
        assert_eq!(server.route(Kind::Playout, 2).as_deref(), Some("spk-b"));
        assert!(devices.lost[1]);
        let last = run_pending(&server, &mut devices, (true, 0), deadlines());
        assert_eq!(
            last,
            PendingRun {
                waiting: false,
                moves_failed: vec![("speaker", FailureCode::AudioDevice)],
            }
        );
        assert_eq!(devices.lost, [false, false]);
    }

    #[test]
    fn given_two_kinds_when_one_fails_then_both_forward_moves_run_before_any_rollback() {
        // GIVEN a microphone whose second stream is refused (after the first
        // moved) and a speaker that moves, WHEN a try runs, THEN the speaker
        // move is sent before the microphone rollback, the speaker is done,
        // and only the microphone is reported.
        let mut server = Fake::with(vec![at(1, "mic-a"), at(4, "mic-a")], vec![at(2, "spk-a")]);
        server.refuse = vec![(4, "mic-b")];
        let mut devices = Devices {
            pending: [Some("mic-b".to_owned()), Some("spk-b".to_owned())],
            ..Devices::default()
        };
        let run = run_pending(&server, &mut devices, (false, 0), deadlines());
        assert_eq!(
            *server.sent.borrow(),
            vec![
                (Kind::Recording, 1, "mic-b".to_owned()),
                (Kind::Recording, 4, "mic-b".to_owned()),
                (Kind::Playout, 2, "spk-b".to_owned()),
                (Kind::Recording, 1, "mic-a".to_owned()),
            ]
        );
        assert_eq!(
            run,
            PendingRun {
                waiting: true,
                moves_failed: vec![("microphone", FailureCode::DeviceKept)],
            }
        );
        assert_eq!(devices.pending[1], None);
        assert_eq!(devices.pending[0].as_deref(), Some("mic-b"));
    }

    #[test]
    fn given_a_try_of_an_older_change_then_it_leaves_the_newer_target_alone() {
        // GIVEN a speaker target of a newer change (generation 1) and a
        // server whose routes cannot be read, WHEN a last try that was
        // scheduled for the older change (generation 0) runs late, THEN it
        // reads nothing, reports nothing and the newer target still waits;
        // WHEN a last try of the newer change runs, THEN it reports the
        // unreadable routes and drops the target.
        let server = Fake::with(vec![], vec![]);
        server.unreadable.set(true);
        let mut devices = Devices {
            pending: [None, Some("spk-c".to_owned())],
            generation: 1,
            ..Devices::default()
        };
        assert_eq!(
            run_pending(&server, &mut devices, (true, 0), deadlines()),
            PendingRun {
                waiting: true,
                moves_failed: Vec::new(),
            }
        );
        assert_eq!(server.reads.get(), 0);
        assert_eq!(devices.pending[1].as_deref(), Some("spk-c"));
        assert_eq!(
            run_pending(&server, &mut devices, (true, 1), deadlines()),
            PendingRun {
                waiting: false,
                moves_failed: vec![("speaker", FailureCode::AudioDevice)],
            }
        );
    }

    #[test]
    fn given_one_kind_moved_by_a_try_when_the_other_fails_last_then_it_is_audio_device() {
        // GIVEN waiting targets of both kinds and a speaker move that the
        // server refuses, WHEN the last try moves the microphone and the
        // speaker stays, THEN the speaker is AudioDevice: one route of the
        // change moved, so the call does not run on the devices it had.
        // GIVEN a microphone stream that opened on its target by itself
        // THEN no route moved, and the speaker is DeviceKept.
        for (mic, code) in [
            ("mic-a", FailureCode::AudioDevice),
            ("mic-b", FailureCode::DeviceKept),
        ] {
            let mut server = Fake::with(vec![at(1, mic)], vec![at(2, "spk-a")]);
            server.refuse = vec![(2, "spk-b")];
            let mut devices = Devices {
                pending: [Some("mic-b".to_owned()), Some("spk-b".to_owned())],
                ..Devices::default()
            };
            let run = run_pending(&server, &mut devices, (true, 0), deadlines());
            assert_eq!(
                run,
                PendingRun {
                    waiting: false,
                    moves_failed: vec![("speaker", code)],
                },
                "{mic}"
            );
            assert_eq!(server.route(Kind::Recording, 1).as_deref(), Some("mic-b"));
        }
    }

    #[test]
    fn given_an_accepted_change_then_a_new_generation_starts_with_no_moved_kind() {
        // GIVEN a kind that an earlier try moved, WHEN a change is accepted,
        // THEN the generation counts it and no kind is marked as moved;
        // WHEN a change is refused, THEN the generation stays.
        let server = Fake::with(vec![], vec![]);
        let mut devices = Devices {
            applied: [true, true],
            ..Devices::default()
        };
        let accepted = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-b"), Some("pulse:spk-b")),
            deadlines(),
        );
        assert_eq!(accepted, Ok(()));
        assert_eq!(devices.generation, 1);
        assert_eq!(devices.applied, [false, false]);
        let refused = run_change(
            &server,
            DEFAULTS,
            &mut devices,
            (Some("pulse:mic-gone"), None),
            deadlines(),
        );
        assert_eq!(refused, Err(FailureCode::DeviceNotFound));
        assert_eq!(devices.generation, 1);
    }

    #[test]
    fn given_a_start_default_that_is_gone_when_changed_then_nothing_moves() {
        // GIVEN a call-start speaker that the server no longer has and no
        // playback stream, WHEN a new microphone and "System default" for
        // the speaker are chosen, THEN the change is Unsupported and no
        // stream moves. GIVEN a start microphone that is a monitor the
        // server still has, WHEN "System default" is chosen, THEN the
        // stream moves there.
        let server = Fake::with(vec![at(1, "mic-a")], vec![]);
        let mut devices = Devices::default();
        let gone = run_change(
            &server,
            (Some("mic-a"), Some("spk-gone")),
            &mut devices,
            (Some("pulse:mic-b"), None),
            deadlines(),
        );
        assert_eq!(gone, Err(FailureCode::Unsupported));
        assert!(server.sent.borrow().is_empty());
        assert_eq!(devices.pending, [None, None]);
        let monitor = run_change(
            &server,
            (Some("spk-a.monitor"), Some("spk-a")),
            &mut devices,
            (None, None),
            deadlines(),
        );
        assert_eq!(monitor, Ok(()));
        assert_eq!(
            server.route(Kind::Recording, 1).as_deref(),
            Some("spk-a.monitor")
        );
    }

    #[test]
    fn given_a_stream_on_a_monitor_when_routed_then_the_monitor_name_is_known() {
        // GIVEN a monitor source that the lists keep apart, and a recording
        // stream on it, WHEN the routes are read, THEN the stream shows the
        // monitor's name, while the choices still leave the monitor out.
        let lists = Lists {
            sources: vec![("mic-a".to_owned(), String::new())],
            source_indexes: vec![1],
            monitors: vec![("spk-a.monitor".to_owned(), 9)],
            ..Lists::default()
        };
        let own = std::process::id();
        let routed = route_streams(
            vec![(5, Some(own), Some(9))],
            own,
            &lists.recording_devices(),
        );
        assert_eq!(routed, vec![at(5, "spk-a.monitor")]);
        assert_eq!(lists.sources.len(), 1);
    }
}
