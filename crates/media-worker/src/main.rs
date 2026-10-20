//! Persephone media worker (ADR-0003).
//!
//! One process decodes one image. Before reading any input the worker checks
//! that it is running with the containment its broker is required to apply;
//! if any check fails it reports which, reads nothing, and exits. It then reads
//! one encoded-image frame from stdin, decodes it in the WebAssembly sandbox,
//! and writes exactly one frame to stdout: RGBA pixels, or a failure message.

use persephone_media_protocol::{
    FrameHeader, FrameKind, ResourceBudgets, WorkerFailure, read_frame, write_frame,
};
use persephone_media_sandbox::{Limits, Sandbox, SandboxError};
use std::{io, process::ExitCode};

mod containment;

/// The decoder guest, fixed at build time by build.rs.
static GUEST: &[u8] = include_bytes!(env!("PERSEPHONE_MEDIA_GUEST_PATH"));

const EXIT_DECODED: u8 = 0;
const EXIT_FAILURE_REPORTED: u8 = 1;
const EXIT_UNREPORTABLE: u8 = 2;
const EXIT_CONTAINMENT: u8 = 3;

fn main() -> ExitCode {
    let missing = containment::missing_controls();
    if missing != 0 {
        return report(WorkerFailure::ContainmentCheck(missing), EXIT_CONTAINMENT);
    }
    match decode_request() {
        Ok(()) => ExitCode::from(EXIT_DECODED),
        Err(failure) => report(failure, EXIT_FAILURE_REPORTED),
    }
}

fn decode_request() -> Result<(), WorkerFailure> {
    // Budgets bound the request body before it is allocated.
    let budgets = ResourceBudgets::default();
    let request =
        read_frame(&mut io::stdin().lock(), &budgets).map_err(|_| WorkerFailure::BadRequest)?;
    if request.header().kind != FrameKind::EncodedImage {
        return Err(WorkerFailure::BadRequest);
    }
    let sandbox = Sandbox::new(GUEST).map_err(|_| WorkerFailure::ModuleRejected)?;
    // Pixels go to stdout straight from guest memory; the worker never holds a
    // second copy of the output.
    let (written, _) = sandbox
        .decode_with(request.body(), &Limits::default(), |width, height, rgba| {
            let header = FrameHeader {
                kind: FrameKind::RgbaPixels,
                length: rgba.len() as u64,
                width,
                height,
            };
            // write_frame applies the receiver's validation before sending.
            write_frame(&mut io::stdout().lock(), header, rgba)
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
