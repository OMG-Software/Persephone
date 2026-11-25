//! Other capture methods for the screen probe (ADR-0053 amendment): DXGI
//! Desktop Duplication and GDI `BitBlt`. Each one captures the first monitor
//! once and checks the pixels, because a capture that returns only black is a
//! silent failure. The numbers are the exit codes of the child process.

use std::cell::Cell;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DESKTOPHORZRES, DESKTOPVERTRES, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDeviceCaps,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::core::Interface;

/// Which method a child run measures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    /// Windows.Graphics.Capture (`screen/wgc.rs`).
    Wgc,
    /// `IDXGIOutput1::DuplicateOutput`, `AcquireNextFrame`, a CPU copy.
    Dxgi,
    /// `GetDC(NULL)` and `BitBlt` into a DIB section.
    Gdi,
}

impl Method {
    pub const ALL: [Method; 3] = [Method::Wgc, Method::Dxgi, Method::Gdi];

    pub fn number(self) -> u32 {
        match self {
            Method::Wgc => 0,
            Method::Dxgi => 1,
            Method::Gdi => 2,
        }
    }

    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|method| method.number() == number)
    }

    pub fn name(self) -> &'static str {
        match self {
            Method::Wgc => "WGC",
            Method::Dxgi => "DXGI Desktop Duplication",
            Method::Gdi => "GDI BitBlt",
        }
    }
}

pub const DEVICE: u32 = 15;
pub const NO_OUTPUT: u32 = 16;
pub const DUPLICATE: u32 = 17;
pub const ACQUIRE: u32 = 18;
pub const COPY: u32 = 19;
pub const GDI_DC: u32 = 20;
pub const GDI_BITMAP: u32 = 21;
pub const GDI_BLT: u32 = 22;
pub const BLACK: u32 = 23;
pub const UNIFORM: u32 = 24;
/// The last stage of any method.
pub const LAST_STAGE: u32 = UNIFORM;

/// The name of a stage of these methods; `None` for the others.
pub fn describe(stage: u32) -> Option<&'static str> {
    Some(match stage {
        DEVICE => "Direct3D 11 device or DXGI factory failed",
        NO_OUTPUT => "no DXGI output listed",
        DUPLICATE => "DuplicateOutput failed",
        ACQUIRE => "AcquireNextFrame failed",
        COPY => "frame copy or map failed",
        GDI_DC => "GetDC(NULL) or memory DC failed",
        GDI_BITMAP => "CreateDIBSection failed",
        GDI_BLT => "BitBlt failed",
        BLACK => "the call succeeded but every pixel is black",
        UNIFORM => "the call succeeded but every pixel is the same colour",
        _ => return None,
    })
}

thread_local! {
    static LAST_ERROR: Cell<i32> = const { Cell::new(0) };
    static MEAN: Cell<u32> = const { Cell::new(0) };
}

/// The mean colour (`0xRRGGBB`) of the last checked frame on this thread.
pub fn last_mean() -> u32 {
    MEAN.with(Cell::get)
}

/// The `HRESULT` of the last failure on this thread (0: none).
pub fn last_error() -> i32 {
    LAST_ERROR.with(Cell::get)
}

fn note(stage: u32, error: windows::core::Error) -> u32 {
    LAST_ERROR.with(|cell| cell.set(error.code().0));
    stage
}

/// What a BGRA frame holds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pixels {
    /// Pixels with a non-zero blue, green or red channel.
    pub colored: u64,
    /// Pixels that differ from the first pixel in a colour channel.
    pub different: u64,
    /// Pixels counted.
    pub total: u64,
    /// The sums of the blue, green and red channels.
    pub sums: [u64; 3],
}

impl Pixels {
    /// The mean colour as `0xRRGGBB` (0 for no pixels).
    pub fn mean(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        let channel = |index: usize| (self.sums[index] / self.total) as u32;
        (channel(2) << 16) | (channel(1) << 8) | channel(0)
    }
}

/// Counts pixels of a BGRA frame; the alpha channel is ignored. A row that
/// the buffer does not fully hold ends the count.
pub fn inspect(bytes: &[u8], width: usize, height: usize, stride: usize) -> Pixels {
    let mut pixels = Pixels::default();
    if width == 0 || stride < width.saturating_mul(4) {
        return pixels;
    }
    let first = bytes.get(..3).map(|first| [first[0], first[1], first[2]]);
    for row in 0..height {
        let Some(line) = row
            .checked_mul(stride)
            .and_then(|start| bytes.get(start..start.checked_add(width * 4)?))
        else {
            break;
        };
        for pixel in line.as_chunks::<4>().0 {
            pixels.total += 1;
            for (sum, &value) in pixels.sums.iter_mut().zip(pixel) {
                *sum += u64::from(value);
            }
            if pixel[..3] != [0, 0, 0] {
                pixels.colored += 1;
            }
            if Some([pixel[0], pixel[1], pixel[2]]) != first {
                pixels.different += 1;
            }
        }
    }
    pixels
}

/// The exit stage for a frame: 0 when it has varied colour. The mean colour
/// stays for `last_mean`.
fn verdict(pixels: Pixels) -> u32 {
    MEAN.with(|cell| cell.set(pixels.mean()));
    if pixels.colored == 0 {
        BLACK
    } else if pixels.different == 0 {
        UNIFORM
    } else {
        0
    }
}

/// The capture attempt of `method`: 0 for a real picture, else a stage.
/// WGC has its own code in `wgc.rs`; it is not run here.
pub fn run(method: Method) -> u32 {
    match method {
        Method::Dxgi => dxgi(),
        Method::Gdi => gdi(),
        Method::Wgc => 0,
    }
}

fn dxgi() -> u32 {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(factory) => factory,
        Err(error) => return note(DEVICE, error),
    };
    let mut first_error: Option<u32> = None;
    let mut seen_output = false;
    let mut adapter_index = 0;
    // SAFETY: plain enumeration calls on live COM objects.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        adapter_index += 1;
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            seen_output = true;
            let Ok(output) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            let Ok(adapter) = adapter.cast::<IDXGIAdapter>() else {
                continue;
            };
            let (mut device, mut context) = (None::<ID3D11Device>, None::<ID3D11DeviceContext>);
            // SAFETY: the out pointers are valid; the adapter is explicit, so
            // the driver type is UNKNOWN.
            if let Err(error) = unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            } {
                first_error.get_or_insert_with(|| note(DEVICE, error));
                continue;
            }
            let (Some(device), Some(context)) = (device, context) else {
                continue;
            };
            // SAFETY: `device` is a live device of the output's adapter.
            let duplication = match unsafe { output.DuplicateOutput(&device) } {
                Ok(duplication) => duplication,
                Err(error) => {
                    first_error.get_or_insert_with(|| note(DUPLICATE, error));
                    continue;
                }
            };
            return duplicate_frame(&device, &context, &duplication);
        }
    }
    match first_error {
        Some(stage) => stage,
        None if seen_output => DEVICE,
        None => NO_OUTPUT,
    }
}

fn duplicate_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    duplication: &IDXGIOutputDuplication,
) -> u32 {
    let until = Instant::now() + Duration::from_secs(5);
    let mut last = ACQUIRE;
    while Instant::now() < until {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: valid out values; the frame is released below.
        if let Err(error) = unsafe { duplication.AcquireNextFrame(500, &mut info, &mut resource) } {
            last = note(ACQUIRE, error);
            continue;
        }
        let stage = match resource.and_then(|resource| resource.cast::<ID3D11Texture2D>().ok()) {
            Some(texture) => copy_and_check(device, context, &texture),
            None => COPY,
        };
        // SAFETY: balances the successful acquire.
        let _ = unsafe { duplication.ReleaseFrame() };
        // A frame with no picture update is the cursor only: ask again.
        if stage == 0 || info.LastPresentTime != 0 {
            return stage;
        }
    }
    last
}

fn copy_and_check(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> u32 {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `desc` is a valid out value.
    unsafe { texture.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    // SAFETY: valid description and out value.
    if let Err(error) = unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) } {
        return note(COPY, error);
    }
    let Some(staging) = staging else {
        return COPY;
    };
    // SAFETY: both textures belong to this device and match in size.
    unsafe { context.CopyResource(&staging, texture) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: a CPU-readable staging texture; the `Unmap` below balances it.
    if let Err(error) = unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) } {
        return note(COPY, error);
    }
    let stride = mapped.RowPitch as usize;
    let stage = if mapped.pData.is_null() {
        COPY
    } else {
        // SAFETY: the mapping holds `RowPitch * Height` readable bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                stride.saturating_mul(desc.Height as usize),
            )
        };
        verdict(inspect(
            bytes,
            desc.Width as usize,
            desc.Height as usize,
            stride,
        ))
    };
    // SAFETY: balances the successful `Map`.
    unsafe { context.Unmap(&staging, 0) };
    stage
}

fn gdi() -> u32 {
    // SAFETY: GDI calls on handles made and released in this function.
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return GDI_DC;
        }
        let release = |screen| {
            ReleaseDC(Some(HWND::default()), screen);
        };
        let width = GetDeviceCaps(Some(screen), DESKTOPHORZRES).max(0);
        let height = GetDeviceCaps(Some(screen), DESKTOPVERTRES).max(0);
        if !(2..=16_384).contains(&width) || !(2..=16_384).contains(&height) {
            release(screen);
            return GDI_DC;
        }
        let memory = CreateCompatibleDC(Some(screen));
        if memory.is_invalid() {
            release(screen);
            return GDI_DC;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) if !bits.is_null() => bitmap,
            Ok(bitmap) => {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(memory);
                release(screen);
                return GDI_BITMAP;
            }
            Err(error) => {
                let _ = DeleteDC(memory);
                release(screen);
                return note(GDI_BITMAP, error);
            }
        };
        let old = SelectObject(memory, bitmap.into());
        let blt = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            0,
            0,
            SRCCOPY | CAPTUREBLT,
        );
        let stage = match blt {
            Ok(()) => {
                let (w, h) = (width as usize, height as usize);
                // SAFETY: the DIB section holds `w * h * 4` bytes.
                let bytes = std::slice::from_raw_parts(bits.cast::<u8>(), w * h * 4);
                verdict(inspect(bytes, w, h, w * 4))
            }
            Err(error) => note(GDI_BLT, error),
        };
        SelectObject(memory, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        release(screen);
        stage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pixels: &[[u8; 4]]) -> Vec<u8> {
        pixels.iter().flatten().copied().collect()
    }

    /// GIVEN a frame of black pixels WHEN inspected THEN the verdict is
    /// `BLACK`, even though the capture call succeeded.
    #[test]
    fn given_black_frame_then_verdict_is_black() {
        let bytes = frame(&[[0, 0, 0, 255]; 4]);
        assert_eq!(verdict(inspect(&bytes, 2, 2, 8)), BLACK);
    }

    /// GIVEN one colour everywhere WHEN inspected THEN `UNIFORM`.
    #[test]
    fn given_one_colour_then_verdict_is_uniform() {
        let bytes = frame(&[[9, 8, 7, 255]; 4]);
        assert_eq!(verdict(inspect(&bytes, 2, 2, 8)), UNIFORM);
    }

    /// GIVEN two colours WHEN inspected THEN it counts as a picture, and the
    /// alpha channel does not matter.
    #[test]
    fn given_varied_pixels_then_verdict_is_a_picture() {
        let bytes = frame(&[[1, 2, 3, 0], [4, 5, 6, 255], [1, 2, 3, 9], [1, 2, 3, 9]]);
        assert_eq!(verdict(inspect(&bytes, 2, 2, 8)), 0);
    }

    /// GIVEN a buffer shorter than the frame or a stride below the row size
    /// WHEN inspected THEN no byte outside the buffer is read.
    #[test]
    fn given_short_buffer_or_small_stride_then_inspect_stays_inside() {
        let bytes = frame(&[[1, 1, 1, 0]; 3]);
        assert_eq!(inspect(&bytes, 2, 2, 8).colored, 2);
        assert_eq!(inspect(&bytes, 2, 2, 8).mean(), 0x01_01_01);
        assert_eq!(inspect(&bytes, 2, 2, 4), Pixels::default());
        assert_eq!(inspect(&[], 2, 2, 8), Pixels::default());
    }

    /// GIVEN every stage of these methods WHEN described THEN each has a
    /// distinct name.
    #[test]
    fn given_stages_then_each_has_a_name() {
        let mut names = std::collections::HashSet::new();
        for stage in DEVICE..=LAST_STAGE {
            assert!(names.insert(describe(stage).expect("a name")), "{stage}");
        }
        assert!(describe(0).is_none());
    }
}
