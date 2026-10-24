//! Persephone media worker (ADR-0003, ADR-0009).
//!
//! One process runs one operation on one image. Before reading any input the
//! worker applies its own part of its containment and checks the part its
//! broker is required to apply ([`contain`]); if any check fails it reports
//! which, reads nothing, and exits. On Linux and macOS it then writes one
//! [`WorkerReady`] frame, so the broker can check the containment from
//! outside before it sends anything. It then reads one request from stdin: a
//! control frame that names the operation ([`WorkerOperation`]), then one
//! encoded-image frame. It runs the operation in the WebAssembly sandbox and
//! writes its reply to stdout:
//!
//! * decode: one RGBA pixel frame followed by one [`PixelInfo`] control frame
//!   with the EXIF orientation;
//! * strip: one encoded-image frame with the stripped file;
//! * on failure, instead: one failure control frame.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use persephone_media_protocol::WorkerReady;
use persephone_media_protocol::{
    FrameHeader, FrameKind, PixelInfo, ResourceBudgets, WorkerFailure, WorkerOperation, read_frame,
    write_frame,
};
use persephone_media_sandbox::{Limits, Sandbox, SandboxError};
use persephone_media_worker::contain;
use std::{io, process::ExitCode};

/// The decoder guest, fixed at build time by build.rs.
static GUEST: &[u8] = include_bytes!(env!("PERSEPHONE_MEDIA_GUEST_PATH"));

const EXIT_DONE: u8 = 0;
const EXIT_FAILURE_REPORTED: u8 = 1;
const EXIT_UNREPORTABLE: u8 = 2;
const EXIT_CONTAINMENT: u8 = 3;

fn main() -> ExitCode {
    let missing = contain();
    if missing != 0 {
        return report(WorkerFailure::ContainmentCheck(missing), EXIT_CONTAINMENT);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if write_frame(
        &mut io::stdout().lock(),
        WorkerReady::header(),
        &WorkerReady.encode(),
    )
    .is_err()
    {
        return ExitCode::from(EXIT_UNREPORTABLE);
    }
    match handle_request() {
        Ok(()) => ExitCode::from(EXIT_DONE),
        Err(failure) => report(failure, EXIT_FAILURE_REPORTED),
    }
}

fn handle_request() -> Result<(), WorkerFailure> {
    // Budgets bound each request body before it is allocated.
    let budgets = ResourceBudgets::default();
    let mut stdin = io::stdin().lock();
    let operation = read_frame(&mut stdin, &budgets).map_err(|_| WorkerFailure::BadRequest)?;
    if operation.header().kind != FrameKind::Control {
        return Err(WorkerFailure::BadRequest);
    }
    let operation = WorkerOperation::decode(operation.body()).ok_or(WorkerFailure::BadRequest)?;
    let request = read_frame(&mut stdin, &budgets).map_err(|_| WorkerFailure::BadRequest)?;
    if request.header().kind != FrameKind::EncodedImage {
        return Err(WorkerFailure::BadRequest);
    }
    drop(stdin);
    let sandbox = Sandbox::new(GUEST).map_err(|_| WorkerFailure::ModuleRejected)?;
    match operation {
        WorkerOperation::Decode => decode(&sandbox, request.body()),
        WorkerOperation::Strip => strip(&sandbox, request.body()),
    }
}

fn decode(sandbox: &Sandbox, encoded: &[u8]) -> Result<(), WorkerFailure> {
    // Pixels go to stdout straight from guest memory; the worker never holds a
    // second copy of the output.
    let (written, _) = sandbox
        .decode_with(
            encoded,
            &Limits::default(),
            |width, height, orientation, rgba| {
                let header = FrameHeader {
                    kind: FrameKind::RgbaPixels,
                    length: rgba.len() as u64,
                    width,
                    height,
                };
                let info = PixelInfo::new(orientation).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "orientation out of range")
                })?;
                let mut stdout = io::stdout().lock();
                // write_frame applies the receiver's validation before sending.
                write_frame(&mut stdout, header, rgba)?;
                write_frame(&mut stdout, PixelInfo::header(), &info.encode())
            },
        )
        .map_err(failure_for)?;
    written.map_err(|_| WorkerFailure::InvalidOutput)
}

fn strip(sandbox: &Sandbox, encoded: &[u8]) -> Result<(), WorkerFailure> {
    // The sandbox has checked that the stripped file is not longer than the
    // input, so it fits one encoded-image frame.
    let (written, _) = sandbox
        .strip_with(encoded, &Limits::for_strip(), |stripped| {
            let header = FrameHeader {
                kind: FrameKind::EncodedImage,
                length: stripped.len() as u64,
                width: 0,
                height: 0,
            };
            write_frame(&mut io::stdout().lock(), header, stripped)
        })
        .map_err(failure_for)?;
    written.map_err(|_| WorkerFailure::InvalidOutput)
}

fn failure_for(error: SandboxError) -> WorkerFailure {
    match error {
        SandboxError::InvalidModule(_)
        | SandboxError::ForbiddenImport { .. }
        | SandboxError::InvalidExports(_) => WorkerFailure::ModuleRejected,
        SandboxError::InputTooLarge => WorkerFailure::InputTooLarge,
        SandboxError::Instantiation(_) => WorkerFailure::Instantiation,
        SandboxError::FuelExhausted => WorkerFailure::FuelExhausted,
        SandboxError::Trap(_) => WorkerFailure::Trap,
        SandboxError::GuestRejected(status) => WorkerFailure::GuestRejected(status),
        SandboxError::NoOutput => WorkerFailure::NoOutput,
        SandboxError::InvalidOutput(_) => WorkerFailure::InvalidOutput,
    }
}

fn report(failure: WorkerFailure, code: u8) -> ExitCode {
    match write_frame(
        &mut io::stdout().lock(),
        WorkerFailure::header(),
        &failure.encode(),
    ) {
        Ok(()) => ExitCode::from(code),
        Err(_) => ExitCode::from(EXIT_UNREPORTABLE),
    }
}
