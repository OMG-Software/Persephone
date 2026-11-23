//! Sound server device names on Linux (ADR-0049).
//!
//! libwebrtc's PulseAudio module gives no stable device IDs, only positions
//! (`index:N`), which are never listed or selected (ADR-0043). This module
//! asks the sound server for its sink and source names through the
//! PulseAudio client library that libwebrtc already loads. It uses one
//! short-lived connection, before containment stage 2, and only reads.
//!
//! The library is opened with `dlopen` (as libwebrtc does), so the worker
//! has no build or link dependency on it. A missing library, a failed
//! connection or a slow server gives `None`; the caller then keeps the
//! system default only.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use persephone_call_protocol::MAX_PULSE_NAME_BYTES;

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
unsafe fn symbol<F: Copy>(handle: *mut c_void, name: &CStr) -> Option<F> {
    // SAFETY: the handle is open and the name is a C string.
    let pointer = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if pointer.is_null() {
        return None;
    }
    // SAFETY: F is a function pointer of the same size, as the caller states.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&pointer) })
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: a fixed library name; the handle is never closed.
        let handle =
            unsafe { libc::dlopen(c"libpulse.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return None;
        }
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
            PA_CONTEXT_READY => break,
            PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => return false,
            _ => {}
        }
        if Instant::now() >= deadline || !step(api, mainloop) {
            return false;
        }
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
