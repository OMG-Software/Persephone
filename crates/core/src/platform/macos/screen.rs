//! The macOS screen share source (ADR-0064): the Screen Recording permission
//! and the choice of one display. The core, not the call worker, asks the
//! user; the worker captures the named display with ScreenCaptureKit.
//!
//! The permission calls are CoreGraphics functions declared here, with no
//! binding crate. `CGPreflightScreenCaptureAccess` only reads the state.
//! `CGRequestScreenCaptureAccess` shows the system's own prompt and returns
//! `false` at once on the first ask: the user then allows Persephone in
//! System Settings, Privacy & Security, Screen Recording, and starts the
//! share again (macOS may need Persephone restarted before the permission
//! counts). The share is then `Refused`.
//!
//! This module is not compiled on the development machine; CI builds it, and
//! a Mac must run it. No default-run test calls it: a request in a test would
//! show a system prompt. The flow itself is `calls::screen::choose_display`,
//! which has its own tests.

use super::credentials::NativePrompt;
use crate::calls::screen::{
    Display, DisplayShare, MAX_DISPLAYS, ScreenGrant, ScreenRefusal, ScreenSource, choose_display,
    points,
};
use crate::credentials::CredentialSource;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// CoreGraphics is a system framework; the core declares these few functions
// instead of adding a binding crate (as for the idle time, ADR-0025).
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// `bool CGPreflightScreenCaptureAccess(void)` (macOS 10.15 and later).
    fn CGPreflightScreenCaptureAccess() -> bool;
    /// `bool CGRequestScreenCaptureAccess(void)`.
    fn CGRequestScreenCaptureAccess() -> bool;
    /// `CGError CGGetActiveDisplayList(uint32_t, CGDirectDisplayID *,
    /// uint32_t *)`.
    fn CGGetActiveDisplayList(max: u32, active: *mut u32, count: *mut u32) -> i32;
    /// `CGDirectDisplayID CGMainDisplayID(void)`.
    fn CGMainDisplayID() -> u32;
    /// `CGRect CGDisplayBounds(CGDirectDisplayID)`: the display's frame in
    /// points.
    fn CGDisplayBounds(display: u32) -> CGRect;
}

/// The macOS screen source: the permission, then one question for each
/// display.
pub struct MacScreen;

struct System;

impl DisplayShare for System {
    fn allowed(&self) -> bool {
        // SAFETY: A function with no arguments that only reads the state.
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    fn request(&self) -> bool {
        // SAFETY: A function with no arguments. It may show the system's
        // prompt and returns at once.
        unsafe { CGRequestScreenCaptureAccess() }
    }

    fn displays(&self) -> (Vec<Display>, u32) {
        let mut ids = [0_u32; MAX_DISPLAYS];
        let mut count = 0_u32;
        // SAFETY: `ids` has room for `MAX_DISPLAYS` entries, which is the
        // limit passed; `count` is a live `u32`. The call writes at most that
        // many.
        let status =
            unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
        // SAFETY: A function with no arguments.
        let main = unsafe { CGMainDisplayID() };
        if status != 0 {
            return (Vec::new(), main);
        }
        let listed = (count as usize).min(MAX_DISPLAYS);
        let displays = ids[..listed]
            .iter()
            .map(|&id| {
                // SAFETY: A display ID that the call above listed; the
                // function reads it and returns a struct by value.
                let bounds = unsafe { CGDisplayBounds(id) };
                // A display of no usable size gets 0, which no target takes.
                Display {
                    id,
                    width: points(bounds.size.width).unwrap_or(0),
                    height: points(bounds.size.height).unwrap_or(0),
                }
            })
            .collect();
        (displays, main)
    }

    /// The core's own Yes or No alert, "No" by default. No main loop, or an
    /// aborted alert, is `None`.
    fn ask(&self, text: &str) -> Option<bool> {
        NativePrompt.confirm_open_link("Persephone", text).ok()
    }
}

impl ScreenSource for MacScreen {
    fn available(&self) -> bool {
        true
    }

    fn choose(&self, cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
        choose_display(&System, cancel)
    }
}
