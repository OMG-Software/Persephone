//! Windows.Graphics.Capture of one monitor (ADR-0053, ADR-0067).
//!
//! This file uses only `std` and the `windows` crate, so the screen helper
//! (`persephone-screen-helper`) and the native containment gate
//! (`tests/platform/windows`) compile the same source. The helper runs it
//! with the user's token; the gate runs it inside the call worker's
//! AppContainer to measure that capture is denied there.
//!
//! The monitor list is read in display order (`EnumDisplayMonitors`). Each
//! size is the monitor's current mode in pixels (`EnumDisplaySettingsW`), not
//! the DPI-scaled rectangle, so the core and the worker, which differ in DPI
//! awareness, read the same size. The yellow capture border stays on: the
//! code never turns it off.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, IInspectable, Interface, PCWSTR, factory};

/// The most monitors the list keeps.
pub const MAX_MONITORS: usize = 16;
/// The length of a monitor's GDI device name buffer (`CCHDEVICENAME`).
pub const DEVICE_NAME_UNITS: usize = 32;
/// The largest side of a monitor the list keeps, in pixels.
pub const MAX_MONITOR_SIDE: u32 = 16_384;
/// Frame buffers in the capture pool.
const POOL_BUFFERS: i32 = 2;

/// Where a capture stopped. The numbers are the exit codes of the
/// containment gate's probe; never change a released value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Stage {
    /// COM could not start on this thread.
    Com = 1,
    /// No monitor is listed.
    NoMonitor = 2,
    /// The monitor at the index has another size than the caller expects.
    SizeChanged = 3,
    /// The capture item factory is not available.
    Interop = 4,
    /// `CreateForMonitor` failed.
    Item = 5,
    /// The Direct3D 11 device could not be made.
    Device = 6,
    /// The WinRT device could not be made from it.
    WinrtDevice = 7,
    /// The frame pool could not be made.
    Pool = 8,
    /// The capture session could not be made.
    Session = 9,
    /// `StartCapture` failed.
    Start = 10,
    /// No frame came in time (the gate's probe only).
    #[allow(dead_code)]
    NoFrame = 11,
    /// The frame's texture could not be read.
    Texture = 12,
    /// The frame could not be mapped for reading.
    Map = 13,
    /// The capture is not usable on this size.
    BadSize = 14,
}

/// One monitor, as listed.
#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    pub handle: HMONITOR,
    /// The GDI device name (`MONITORINFOEXW::szDevice`, for example
    /// `\\.\DISPLAY1`), with zeros after the name. The share names its
    /// monitor by it (`monitor.rs`).
    #[allow(dead_code)]
    pub device: [u16; DEVICE_NAME_UNITS],
    /// The rectangle on the virtual screen (left, top, right, bottom), in
    /// the pixels of the calling thread's DPI awareness (physical pixels for
    /// a per-monitor aware thread). The share names its monitor by it too
    /// (`monitor.rs`).
    #[allow(dead_code)]
    pub rect: (i32, i32, i32, i32),
    /// The size the user is shown: the current display mode, else the
    /// rectangle's size.
    pub width: u32,
    pub height: u32,
}

struct Collector {
    found: Vec<Monitor>,
}

unsafe extern "system" fn collect(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Collector` that `monitors` passed and keeps
    // alive for the whole enumeration.
    let collector = unsafe { &mut *(data.0 as *mut Collector) };
    if let Some(listed) = monitor_size(monitor) {
        // A list over the limit stops growing; later monitors are not shown.
        if collector.found.len() < MAX_MONITORS
            && (1..=MAX_MONITOR_SIDE).contains(&listed.width)
            && (1..=MAX_MONITOR_SIDE).contains(&listed.height)
        {
            collector.found.push(listed);
        }
    }
    BOOL(1)
}

/// Where a monitor is: its device name (zeros after the name) and its
/// rectangle on the virtual screen (left, top, right, bottom), in the pixels
/// of the calling thread's DPI awareness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Place {
    pub device: [u16; DEVICE_NAME_UNITS],
    pub rect: (i32, i32, i32, i32),
}

/// A monitor's place from one `GetMonitorInfoW` call, or `None` when Windows
/// no longer knows the handle. The list and the helper's place checks
/// both read a monitor through this, so their readings compare equal while
/// it does not change.
pub fn monitor_place(monitor: HMONITOR) -> Option<Place> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a valid MONITORINFOEXW with its size set; the
    // pointer cast is the documented way to pass the extended structure.
    // A handle that is no longer valid makes the call fail, nothing else.
    let known = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
    if !known.as_bool() {
        return None;
    }
    // Keep the name up to its NUL only: Windows does not promise what the
    // rest of the buffer holds.
    let mut device = [0; DEVICE_NAME_UNITS];
    let end = info
        .szDevice
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(info.szDevice.len());
    device[..end].copy_from_slice(&info.szDevice[..end]);
    let rect = info.monitorInfo.rcMonitor;
    Some(Place {
        device,
        rect: (rect.left, rect.top, rect.right, rect.bottom),
    })
}

/// A monitor as listed: its place, and its size in pixels (its current
/// display mode, else its rectangle's size).
fn monitor_size(monitor: HMONITOR) -> Option<Monitor> {
    let Place { device, rect } = monitor_place(monitor)?;
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // A name that fills the buffer has no NUL, and the mode read needs one.
    let read = device[DEVICE_NAME_UNITS - 1] == 0
        // SAFETY: `device` is NUL-terminated (checked above) and `mode` is
        // a valid DEVMODEW with its size set.
        && unsafe {
            EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
        }
        .as_bool();
    let (width, height) = if read && mode.dmPelsWidth > 0 && mode.dmPelsHeight > 0 {
        (mode.dmPelsWidth, mode.dmPelsHeight)
    } else {
        let (left, top, right, bottom) = rect;
        (
            u32::try_from(right.checked_sub(left)?).ok()?,
            u32::try_from(bottom.checked_sub(top)?).ok()?,
        )
    };
    Some(Monitor {
        handle: monitor,
        device,
        rect,
        width,
        height,
    })
}

/// The monitors in display order, at most [`MAX_MONITORS`].
pub fn monitors() -> Vec<Monitor> {
    let mut collector = Collector { found: Vec::new() };
    // SAFETY: the callback only uses the collector for the duration of the
    // call.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM((&raw mut collector) as isize),
        )
    };
    collector.found
}

/// Balances `CoInitializeEx` on the thread that made it.
pub struct ComGuard;

impl ComGuard {
    pub fn start() -> Result<Self, Stage> {
        // SAFETY: COM is initialised on this thread only, and `Drop` ends
        // it on the same thread.
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return Err(Stage::Com);
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` of `start`.
        unsafe { CoUninitialize() };
    }
}

thread_local! {
    /// The `HRESULT` of the last failure on this thread, for the gate's
    /// probe. The helper never reads it, and it is never sent anywhere.
    static LAST_ERROR: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Records the error of a failed call and names where it failed.
fn note(stage: Stage, error: windows::core::Error) -> Stage {
    LAST_ERROR.with(|last| last.set(error.code().0));
    stage
}

/// The `HRESULT` of the last failure on this thread (0: none).
#[allow(dead_code)] // read by the containment gate's probe only
pub fn last_error() -> i32 {
    LAST_ERROR.with(std::cell::Cell::get)
}

/// Receives one frame: `(width, height, stride, bytes)`.
pub type Deliver<'a> = dyn FnMut(u32, u32, u32, &[u8]) + 'a;

/// One running capture of a monitor. Use it on the thread that made it.
pub struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    size: SizeInt32,
    closed: Arc<AtomicBool>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

/// Starts capturing the monitor at `index` of [`monitors`]. When `expected`
/// is set, a monitor of another size is [`Stage::SizeChanged`]: the share
/// asked for one screen, and it never takes another.
pub fn open(index: usize, expected: Option<(u32, u32)>) -> Result<Capture, Stage> {
    let list = monitors();
    let monitor = *list.get(index).ok_or(Stage::NoMonitor)?;
    if expected.is_some_and(|size| size != (monitor.width, monitor.height)) {
        return Err(Stage::SizeChanged);
    }
    open_monitor(&monitor)
}

/// Starts capturing `monitor`, which the caller took from [`monitors`] a
/// moment ago (the helper chooses it by name, size and rectangle).
pub fn open_monitor(monitor: &Monitor) -> Result<Capture, Stage> {
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|error| note(Stage::Interop, error))?;
    // SAFETY: `monitor.handle` came from `EnumDisplayMonitors` just now.
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor.handle) }
        .map_err(|error| note(Stage::Item, error))?;
    let size = item.Size().map_err(|error| note(Stage::Item, error))?;
    if size.Width < 2 || size.Height < 2 {
        return Err(Stage::BadSize);
    }

    let mut device = None;
    let mut context = None;
    // SAFETY: the out pointers are valid for the call; no adapter or
    // software module is passed with the hardware driver type.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| note(Stage::Device, error))?;
    let (device, context) = device.zip(context).ok_or(Stage::Device)?;
    let dxgi: IDXGIDevice = device
        .cast()
        .map_err(|error| note(Stage::WinrtDevice, error))?;
    // SAFETY: `dxgi` is a live DXGI device.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .map_err(|error| note(Stage::WinrtDevice, error))?;
    let winrt_device: IDirect3DDevice = inspectable
        .cast()
        .map_err(|error| note(Stage::WinrtDevice, error))?;

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        POOL_BUFFERS,
        size,
    )
    .map_err(|error| note(Stage::Pool, error))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|error| note(Stage::Session, error))?;
    // The cursor is part of the picture. This is the default; it is set so
    // the rule is in the code. The yellow border is never touched
    // (`IsBorderRequired` stays at its default, on): the user always sees
    // that the screen is shared. A system that lacks the property keeps its
    // default, which is also on.
    let _ = session.SetIsCursorCaptureEnabled(true);
    let closed = Arc::new(AtomicBool::new(false));
    let flag = closed.clone();
    item.Closed(
        &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            flag.store(true, Ordering::Release);
            Ok(())
        }),
    )
    .map_err(|error| note(Stage::Session, error))?;
    session
        .StartCapture()
        .map_err(|error| note(Stage::Start, error))?;
    Ok(Capture {
        device,
        context,
        winrt_device,
        pool,
        session,
        item,
        size,
        closed,
        staging: None,
    })
}

impl Capture {
    /// The monitor or its capture item went away.
    pub fn source_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Takes the next frame, if one waits. `Ok(false)`: no frame waits.
    /// A frame that `admit` refuses is dropped before any copy. Otherwise
    /// `deliver` gets the BGRA bytes as `(width, height, stride, bytes)`;
    /// they are valid only during the call.
    pub fn poll(
        &mut self,
        admit: &mut dyn FnMut() -> bool,
        deliver: &mut Deliver<'_>,
    ) -> Result<bool, Stage> {
        let Ok(frame) = self.pool.TryGetNextFrame() else {
            return Ok(false);
        };
        let content = frame
            .ContentSize()
            .map_err(|error| note(Stage::Texture, error))?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            // The monitor changed size: the next frames use the new size.
            if content.Width < 2 || content.Height < 2 {
                return Err(Stage::BadSize);
            }
            self.pool
                .Recreate(
                    &self.winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    POOL_BUFFERS,
                    content,
                )
                .map_err(|error| note(Stage::Pool, error))?;
            self.size = content;
            return Ok(true);
        }
        if !admit() {
            return Ok(true);
        }
        let surface = frame
            .Surface()
            .map_err(|error| note(Stage::Texture, error))?;
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|error| note(Stage::Texture, error))?;
        // SAFETY: the surface of a capture frame is a Direct3D 11 texture.
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(|error| note(Stage::Texture, error))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out value.
        unsafe { texture.GetDesc(&mut desc) };
        let (width, height) = (
            desc.Width.min(content.Width as u32),
            desc.Height.min(content.Height as u32),
        );
        let staging = self.staging_for(&desc)?;
        // SAFETY: both textures belong to this device and have the same
        // description apart from usage and access.
        unsafe { self.context.CopyResource(&staging, &texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` is a CPU-readable staging texture; `mapped` is a
        // valid out value; the mapping ends with the `Unmap` below.
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|error| note(Stage::Map, error))?;
        let stride = mapped.RowPitch;
        let length = (stride as usize).saturating_mul(height as usize);
        if !mapped.pData.is_null() && stride >= width.saturating_mul(4) {
            // SAFETY: the mapped texture holds `RowPitch * Height` readable
            // bytes until `Unmap`; the slice does not outlive this block.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), length) };
            deliver(width, height, stride, bytes);
        }
        // SAFETY: balances the successful `Map` above.
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(true)
    }

    /// A staging texture of the frame's description, kept for the next frame.
    fn staging_for(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D, Stage> {
        if let Some((texture, width, height)) = &self.staging
            && (*width, *height) == (desc.Width, desc.Height)
        {
            return Ok(texture.clone());
        }
        let mut staging_desc = *desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        staging_desc.MiscFlags = 0;
        let mut texture = None;
        // SAFETY: `staging_desc` is a valid description; `texture` is a
        // valid out value.
        unsafe {
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut texture))
        }
        .map_err(|error| note(Stage::Texture, error))?;
        let texture = texture.ok_or(Stage::Texture)?;
        self.staging = Some((texture.clone(), desc.Width, desc.Height));
        Ok(texture)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Closing ends the capture and releases the screen. A failure here
        // leaves nothing more to do.
        let _ = self.session.Close();
        let _ = self.pool.Close();
        let _ = &self.item;
    }
}
