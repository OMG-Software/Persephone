//! Persephone core (ADR-0004).
//!
//! The core is the application's entry process. It will own sessions, the
//! Matrix SDK, stores, keys and the media broker. The UI process holds none of
//! these; it sends one bounded request frame per call, and the core answers
//! with one bounded reply frame.
//!
//! The UI is treated as untrusted. Every request is read with the budgeted
//! frame reader, so an oversized or malformed header is refused before its body
//! is allocated. Anything a correct UI never sends (a bad frame, an unknown
//! command or field) ends the session instead of being answered: the core
//! stops serving and the UI is shut down.

use persephone_contracts::AppStatus;
use persephone_core_protocol::{Call, Outcome, ReceiveError, Reply, Request, receive, send};
use persephone_media_protocol::ResourceBudgets;
use std::io::{Read, Write};

pub const MILESTONE: &str = "P1";

/// Why the core stopped serving a UI connection.
#[derive(Debug, PartialEq, Eq)]
pub enum ServeEnd {
    /// The UI closed its end, normally because it exited.
    UiClosed,
    /// The UI sent something a correct UI never sends.
    ProtocolViolation,
    /// A reply could not be written.
    ReplyFailed,
}

/// Answers one request.
pub fn handle(request: &Request) -> Reply {
    let outcome = match request.call {
        Call::AppStatus {} => serde_json::to_value(AppStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            milestone: MILESTONE.into(),
        })
        .map_or(
            Outcome::Error(persephone_core_protocol::ErrorCode::Internal),
            Outcome::Ok,
        ),
    };
    Reply {
        id: request.id,
        outcome,
    }
}

/// Serves requests from `input` until the UI closes the stream or violates the
/// protocol. Requests are handled one at a time, in order.
pub fn serve<R: Read, W: Write>(mut input: R, mut output: W) -> ServeEnd {
    let budgets = ResourceBudgets::default();
    loop {
        match receive::<_, Request>(&mut input, &budgets) {
            Ok(request) => {
                if send(&mut output, &handle(&request)).is_err() {
                    return ServeEnd::ReplyFailed;
                }
            }
            Err(ReceiveError::Closed) => return ServeEnd::UiClosed,
            Err(
                ReceiveError::Frame(_) | ReceiveError::UnexpectedKind | ReceiveError::Malformed,
            ) => {
                return ServeEnd::ProtocolViolation;
            }
        }
    }
}
