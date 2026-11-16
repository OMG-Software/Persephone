//! Persephone call worker (ADR-0043).
//!
//! One process runs one call. It checks its own containment, reports the
//! result in `Ready`, and then reads core messages from stdin and writes
//! worker messages to stdout, both as media-protocol Control frames. The
//! first message must be `Join`. Stdout is the frame channel: nothing else
//! may write to it, and the worker writes nothing to stderr.
//!
//! Three bounded queues connect the parts: the stdin reader thread, the
//! call task, and the stdout writer thread.

mod containment;
mod mapping;
mod session;

use std::io;
use std::process::ExitCode;

use persephone_call_protocol::{
    FailureCode, FromWorker, ReadError, ToWorker, read_message, write_message,
};
use persephone_media_protocol::ResourceBudgets;
use tokio::sync::mpsc;

pub const EXIT_DONE: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_UNREPORTABLE: u8 = 2;
pub const EXIT_CONTAINMENT: u8 = 3;

/// Core messages waiting for the call task.
const INPUT_CAPACITY: usize = 16;
/// Worker messages waiting for the stdout writer.
const OUTPUT_CAPACITY: usize = 64;

/// What the stdin reader hands to the call task.
pub enum Input {
    Message(ToWorker),
    /// The core closed stdin.
    Eof,
    /// A frame or message failed validation.
    Bad,
}

fn main() -> ExitCode {
    let bits = containment::missing_controls();
    let (output, writer_queue) = mpsc::channel::<FromWorker>(OUTPUT_CAPACITY);
    let writer = std::thread::spawn(move || write_loop(writer_queue));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return ExitCode::from(EXIT_UNREPORTABLE),
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

async fn run(bits: u32, output: mpsc::Sender<FromWorker>) -> u8 {
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
    match input.recv().await {
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

/// Writes worker messages until every sender is gone. Returns false when a
/// write failed.
fn write_loop(mut queue: mpsc::Receiver<FromWorker>) -> bool {
    let mut stdout = io::stdout().lock();
    while let Some(message) = queue.blocking_recv() {
        if write_message(&mut stdout, &message).is_err() {
            return false;
        }
    }
    true
}
