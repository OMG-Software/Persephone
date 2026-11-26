//! Persephone call worker (ADR-0043).
//!
//! One process runs one call. It checks its own containment, reports the
//! result in `Ready`, and then reads core messages from stdin and writes
//! worker messages to stdout, both as media-protocol Control frames. The
//! first message must be `Join`, `ListDevices` or `TestDevice`, after any
//! number of `SetDiagnostics`. Stdout is the frame channel: nothing else may write to
//! it, and the worker writes nothing to stderr. Diagnostic records go to the
//! core over stdout only while the core has turned them on (ADR-0058).
//!
//! Three bounded queues connect the parts: the stdin reader thread, the
//! call task, and the stdout writer thread. Video pictures do not use the
//! output queue: see `output.rs`.

mod args;
mod camera;
mod camera_channel;
mod containment;
mod diagnostics;
mod effects;
mod mapping;
#[cfg(test)]
mod native_volume_tests;
mod output;
#[cfg(target_os = "linux")]
mod pulse;
mod screen;
mod segmenter;
mod session;
mod sound_defaults;
mod sound_test;
mod video;

use std::io;
use std::process::ExitCode;

use persephone_call_protocol::{FailureCode, FromWorker, ReadError, ToWorker, read_message};
use persephone_media_protocol::ResourceBudgets;
use tokio::sync::mpsc;

use crate::output::Output;

pub const EXIT_DONE: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_UNREPORTABLE: u8 = 2;
pub const EXIT_CONTAINMENT: u8 = 3;

/// Core messages waiting for the call task.
const INPUT_CAPACITY: usize = 16;

/// What the stdin reader hands to the call task.
#[allow(clippy::large_enum_variant)] // one value at a time in a queue of 16
pub enum Input {
    Message(ToWorker),
    /// The core closed stdin.
    Eof,
    /// A frame or message failed validation.
    Bad,
}

fn main() -> ExitCode {
    // The arguments come first: a refused argument is a missing control, so
    // the worker reports it as one and ends (ADR-0049).
    let arguments_ok = args::init();
    // Descriptor 3, the camera channel, is adopted before anything else can
    // open a descriptor.
    #[cfg(target_os = "linux")]
    camera_channel::init();
    let mut bits = containment::missing_controls();
    if !arguments_ok {
        bits |= containment::CHECK_FAILED;
    }
    // Records only after the core turns them on.
    diagnostics::install();
    let (output, writer_queue) = Output::new();
    let writer = std::thread::spawn(move || output::write_stdout(writer_queue));
    let Ok(runtime) = build_runtime() else {
        return ExitCode::from(EXIT_UNREPORTABLE);
    };
    let code = runtime.block_on(run(bits, output));
    // Dropping the runtime stops its tasks; the writer ends when the last
    // sender is gone, after it has written every queued message.
    drop(runtime);
    match writer.join() {
        Ok(true) => ExitCode::from(code),
        _ => ExitCode::from(EXIT_UNREPORTABLE),
    }
}

/// The call task's runtime. LiveKit opens its signalling socket with tokio's
/// networking, so the IO driver must be on: without it the first connect
/// panics and the worker exits with no `Failed` message.
fn build_runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
}

async fn run(bits: u32, output: Output) -> u8 {
    if output
        .send(FromWorker::Ready {
            containment_bits: bits,
        })
        .await
        .is_err()
    {
        return EXIT_UNREPORTABLE;
    }
    if bits != 0 && !cfg!(feature = "dev-uncontained") {
        let _ = output
            .send(FromWorker::Failed {
                code: FailureCode::Containment,
            })
            .await;
        return EXIT_CONTAINMENT;
    }
    // Input is read only after the containment check passed.
    let (input_tx, mut input) = mpsc::channel::<Input>(INPUT_CAPACITY);
    std::thread::spawn(move || read_loop(input_tx));
    match first_message(&mut input, |on| diagnostics::global().set_on(on)).await {
        Some(Input::Message(ToWorker::ListDevices)) => session::list_devices(&output).await,
        Some(Input::Message(first @ ToWorker::TestDevice { .. })) => {
            session::test_device(first, &mut input, &output).await
        }
        Some(Input::Message(first)) if mapping::is_first_message_allowed(&first) => {
            session::run(first, &mut input, &output).await
        }
        Some(Input::Eof) | None => EXIT_DONE,
        Some(Input::Message(_)) | Some(Input::Bad) => {
            let _ = output
                .send(FromWorker::Failed {
                    code: FailureCode::Protocol,
                })
                .await;
            EXIT_FAILED
        }
    }
}

/// The first message that is not the diagnostic log switch. The switch may
/// come first, any number of times, so the join's own records are kept
/// (ADR-0058); each one goes to `switch`.
async fn first_message(
    input: &mut mpsc::Receiver<Input>,
    mut switch: impl FnMut(bool),
) -> Option<Input> {
    loop {
        match input.recv().await {
            Some(Input::Message(ToWorker::SetDiagnostics { on })) => switch(on),
            other => return other,
        }
    }
}

/// Reads core messages until stdin closes or one is invalid.
fn read_loop(queue: mpsc::Sender<Input>) {
    let budgets = ResourceBudgets::default();
    let mut stdin = io::stdin().lock();
    loop {
        let item = match read_message::<_, ToWorker>(&mut stdin, &budgets) {
            Ok(message) => Input::Message(message),
            Err(ReadError::Frame(persephone_media_protocol::FrameError::Io(_))) => Input::Eof,
            Err(_) => Input::Bad,
        };
        let last = !matches!(item, Input::Message(_));
        if queue.blocking_send(item).is_err() || last {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Input, build_runtime, first_message};
    use persephone_call_protocol::ToWorker;
    use tokio::sync::mpsc;

    /// GIVEN switches before the first message WHEN the worker waits for it
    /// THEN each switch is applied in order and the next message is returned;
    /// a closed input after a switch is no message.
    #[test]
    fn given_switches_before_the_first_message_then_they_apply_and_it_follows() {
        let runtime = build_runtime().expect("runtime builds");
        runtime.block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            for item in [
                Input::Message(ToWorker::SetDiagnostics { on: true }),
                Input::Message(ToWorker::SetDiagnostics { on: false }),
                Input::Message(ToWorker::SetDiagnostics { on: true }),
                Input::Message(ToWorker::ListDevices),
            ] {
                tx.send(item).await.expect("queued");
            }
            let mut seen = Vec::new();
            let first = first_message(&mut rx, |on| seen.push(on)).await;
            assert!(matches!(first, Some(Input::Message(ToWorker::ListDevices))));
            assert_eq!(seen, [true, false, true]);

            tx.send(Input::Message(ToWorker::SetDiagnostics { on: true }))
                .await
                .expect("queued");
            drop(tx);
            let mut seen = Vec::new();
            assert!(first_message(&mut rx, |on| seen.push(on)).await.is_none());
            assert_eq!(seen, [true]);
        });
    }

    /// GIVEN the worker's runtime WHEN a loopback TCP connection is made on it
    /// THEN it succeeds (a runtime without the IO driver panics here, as the
    /// first LiveKit signalling connect did in the live call test).
    #[test]
    fn given_worker_runtime_when_tcp_connects_then_it_does_not_panic() {
        let runtime = build_runtime().expect("runtime builds");
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            let address = listener.local_addr().expect("local address");
            let (client, server) =
                tokio::join!(tokio::net::TcpStream::connect(address), listener.accept());
            client.expect("connect");
            server.expect("accept");
        });
    }
}
