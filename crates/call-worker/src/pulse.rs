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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use persephone_call_protocol::{FailureCode, MAX_PULSE_NAME_BYTES};

use crate::mapping::{SoundKnown, SoundNow, own_streams, plan_sound_move};

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
/// (name, description) pairs. Monitor sources are left out.
#[derive(Debug, Default)]
pub struct Lists {
    pub sinks: Vec<(String, String)>,
    pub sources: Vec<(String, String)>,
}

/// One list as the callbacks fill it. The library calls back through a
/// pointer while `run` reads the state, so every access goes through a
/// shared reference and cells; no `&mut` is held across a library call.
#[derive(Default)]
struct Collector {
    entries: RefCell<Vec<(String, String)>>,
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
    unsafe fn add(&self, name: *const c_char, description: *const c_char) {
        if name.is_null() || self.entries.borrow().len() >= MAX_ENTRIES {
            return;
        }
        // SAFETY: a C string from the library, as the caller states.
        let name = unsafe { CStr::from_ptr(name) }.to_bytes();
        if name.len() > MAX_PULSE_NAME_BYTES {
            return;
        }
        let Ok(name) = std::str::from_utf8(name) else {
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
        unsafe { collector.add((*info).name, (*info).description) };
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
            unsafe { collector.add(info.name, info.description) };
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

/// A stream listing as `(index, owner process ID)` pairs.
#[derive(Default)]
struct StreamList {
    entries: RefCell<Vec<(u32, Option<u32>)>>,
    done: Cell<bool>,
    failed: Cell<bool>,
}

impl StreamList {
    /// Copies one stream.
    ///
    /// # Safety
    ///
    /// `proplist` is null or a property list that lives for the call.
    unsafe fn add(&self, index: u32, proplist: *const c_void) {
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
        self.entries.borrow_mut().push((index, owner));
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
        unsafe { list.add(info.index, info.proplist) };
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
        unsafe { list.add(info.index, info.proplist) };
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
#[derive(Clone, Copy)]
enum Kind {
    Recording,
    Playout,
}

impl Inner {
    /// Starts one operation with `start` and runs the main loop until `done`
    /// or the deadline, then cancels it, so its callback never runs after
    /// the caller's state is gone. `true` when `done` held in time.
    fn drive(
        &self,
        deadline: Instant,
        start: impl FnOnce(&Self) -> *mut Operation,
        done: impl Fn() -> bool,
    ) -> bool {
        // SAFETY: the context is live.
        if unsafe { (self.api.context_get_state)(self.context) } != PA_CONTEXT_READY {
            return false;
        }
        let operation = start(self);
        if operation.is_null() {
            return false;
        }
        while !done() && Instant::now() < deadline && step(self.api, self.mainloop) {}
        // SAFETY: the operation is owned here.
        unsafe {
            (self.api.operation_cancel)(operation);
            (self.api.operation_unref)(operation);
        }
        done()
    }

    /// The server's default microphone and speaker names now.
    fn defaults(&self, deadline: Instant) -> (Option<String>, Option<String>) {
        let state = ServerDefaults::default();
        let ok = self.drive(
            deadline,
            // SAFETY: the context is live; the state outlives the operation.
            |me| unsafe { (me.ext.get_server_info)(me.context, on_server_info, state_ptr(&state)) },
            || state.done.get(),
        );
        if ok {
            (state.source.into_inner(), state.sink.into_inner())
        } else {
            (None, None)
        }
    }

    /// The sound server's streams of one kind as `(index, process ID)`.
    fn streams(&self, deadline: Instant, kind: Kind) -> Option<Vec<(u32, Option<u32>)>> {
        let list = StreamList::default();
        let ok = self.drive(
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
        (ok && !list.failed.get()).then(|| list.entries.into_inner())
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
        );
        let sources_ok = sinks_ok
            && self.drive(
                deadline,
                // SAFETY: as above.
                |me| unsafe {
                    (me.api.get_source_info_list)(me.context, on_source, sources.userdata())
                },
                || sources.done.get(),
            );
        (sources_ok && !sinks.failed.get() && !sources.failed.get()).then(|| Lists {
            sinks: sinks.entries.into_inner(),
            sources: sources.entries.into_inner(),
        })
    }

    /// Moves one stream to the device with this name. `true` when the server
    /// accepted the move.
    fn move_one(&self, deadline: Instant, kind: Kind, index: u32, name: &CStr) -> bool {
        let state = Success::default();
        let userdata = std::ptr::from_ref(&state).cast_mut().cast();
        self.drive(
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
        ) && state.ok.get()
    }

    /// Moves each stream, in order, and stops at the first failure. Gives the
    /// indexes that moved and whether all did.
    fn move_all(
        &self,
        deadline: Instant,
        kind: Kind,
        indexes: &[u32],
        name: &str,
    ) -> (Vec<u32>, bool) {
        let mut moved = Vec::new();
        let Ok(name) = std::ffi::CString::new(name) else {
            return (moved, false);
        };
        for &index in indexes {
            if !self.move_one(deadline, kind, index, &name) {
                return (moved, false);
            }
            moved.push(index);
        }
        (moved, true)
    }
}

fn state_ptr(state: &ServerDefaults) -> *mut c_void {
    std::ptr::from_ref(state).cast_mut().cast()
}

/// The call's sound server connection and what it knows of the devices.
pub struct Session {
    inner: Mutex<Inner>,
    /// The server's default microphone and speaker when the call started.
    default_source: Option<String>,
    default_sink: Option<String>,
    /// The microphone and speaker the call's streams are on; `None` when
    /// that is not known (a failed move that could not be undone).
    current: Mutex<(Option<String>, Option<String>)>,
}

impl Session {
    /// Opens the connection and reads the defaults. `None` when the library
    /// or a function is missing, the server does not answer, or the time runs
    /// out. It blocks for at most about [`OPERATION_TIMEOUT`]. Call it before
    /// containment stage 2: it opens a new sound server socket. `recording`
    /// and `playout` are the join's choices.
    pub fn open(recording: Option<&str>, playout: Option<&str>) -> Option<Self> {
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
        let (default_source, default_sink) = inner.defaults(deadline);
        let chosen = |choice: Option<&str>, default: &Option<String>| {
            choice
                .and_then(persephone_call_protocol::pulse_name)
                .map(str::to_owned)
                .or_else(|| default.clone())
        };
        let current = (
            chosen(recording, &default_source),
            chosen(playout, &default_sink),
        );
        Some(Self {
            inner: Mutex::new(inner),
            default_source,
            default_sink,
            current: Mutex::new(current),
        })
    }

    /// Moves the call's own streams to the chosen microphone and speaker
    /// (ADR-0059). It blocks for at most a few operation timeouts: call it
    /// on a blocking thread. Both choices are checked before either stream
    /// moves. A stream that cannot move stays where it was. `DeviceKept`
    /// when every move is undone, `AudioDevice` when one is not.
    pub fn switch(
        &self,
        recording: Option<&str>,
        playout: Option<&str>,
    ) -> Result<(), FailureCode> {
        let inner = self.inner.lock().map_err(|_| FailureCode::AudioDevice)?;
        let mut current = self.current.lock().map_err(|_| FailureCode::AudioDevice)?;
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let lists = inner.lists(deadline).ok_or(FailureCode::AudioDevice)?;
        let sources = inner
            .streams(deadline, Kind::Recording)
            .ok_or(FailureCode::AudioDevice)?;
        let sinks = inner
            .streams(deadline, Kind::Playout)
            .ok_or(FailureCode::AudioDevice)?;
        let own = std::process::id();
        let own_recording = own_streams(sources, own);
        let own_playout = own_streams(sinks, own);
        let source_names: Vec<&str> = lists.sources.iter().map(|(n, _)| n.as_str()).collect();
        let sink_names: Vec<&str> = lists.sinks.iter().map(|(n, _)| n.as_str()).collect();
        let plan = plan_sound_move(
            recording,
            playout,
            &SoundKnown {
                default_source: self.default_source.as_deref(),
                default_sink: self.default_sink.as_deref(),
                source: current.0.as_deref(),
                sink: current.1.as_deref(),
            },
            &SoundNow {
                sources: &source_names,
                sinks: &sink_names,
                own_recording: own_recording.len(),
                own_playout: own_playout.len(),
            },
        )?;
        // A fresh deadline for the moves: the checks are done.
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let old = current.clone();
        let mut source_moved = Vec::new();
        if let Some(name) = &plan.source {
            let (moved, all) = inner.move_all(deadline, Kind::Recording, &own_recording, name);
            if all {
                current.0 = Some(name.clone());
            } else {
                let undone = undo(&inner, deadline, Kind::Recording, &moved, old.0.as_deref());
                if !undone {
                    current.0 = None;
                }
                return Err(failure(undone));
            }
            source_moved = own_recording;
        }
        if let Some(name) = &plan.sink {
            let (moved, all) = inner.move_all(deadline, Kind::Playout, &own_playout, name);
            if all {
                current.1 = Some(name.clone());
            } else {
                let undone = undo(&inner, deadline, Kind::Playout, &moved, old.1.as_deref());
                if !undone {
                    current.1 = None;
                }
                // The microphone goes back too, so the call keeps its devices.
                let back = undo(
                    &inner,
                    deadline,
                    Kind::Recording,
                    &source_moved,
                    old.0.as_deref(),
                );
                current.0 = if source_moved.is_empty() || back {
                    old.0
                } else {
                    plan.source
                };
                return Err(failure(undone && back));
            }
        }
        Ok(())
    }
}

/// `DeviceKept` when the streams are back on their old device, else
/// `AudioDevice`.
fn failure(undone: bool) -> FailureCode {
    if undone {
        FailureCode::DeviceKept
    } else {
        FailureCode::AudioDevice
    }
}

/// Moves streams that moved back to `old`. `true` when there is nothing to
/// undo or all are back.
fn undo(inner: &Inner, deadline: Instant, kind: Kind, moved: &[u32], old: Option<&str>) -> bool {
    moved.is_empty() || old.is_some_and(|old| inner.move_all(deadline, kind, moved, old).1)
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
        unsafe { list.add(5, std::ptr::null()) };
        assert_eq!(list.entries.into_inner(), vec![(5, None)]);
    }

    #[test]
    fn given_library_entries_when_copied_then_long_or_bad_names_are_left_out() {
        // GIVEN names that are fine, too long, not UTF-8 and null, WHEN
        // copied, THEN only the good one stays, with its description.
        let collector = Collector::default();
        let long = std::ffi::CString::new("a".repeat(MAX_PULSE_NAME_BYTES + 1)).unwrap();
        // SAFETY: each pointer is null or a live C string.
        unsafe {
            collector.add(c"vmic".as_ptr(), c"Virtual mic".as_ptr());
            collector.add(long.as_ptr(), c"Long".as_ptr());
            collector.add(c"\xff\xfe".as_ptr(), c"Bad".as_ptr());
            collector.add(std::ptr::null(), c"Null".as_ptr());
            collector.add(c"nodesc".as_ptr(), std::ptr::null());
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
            unsafe { collector.add(c"s".as_ptr(), c"d".as_ptr()) };
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
        // them, THEN only the real one is kept.
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
    }
}
