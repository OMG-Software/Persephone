//! The contained media worker on Windows. The glue is shared with Linux and
//! macOS ([`crate::media::contained`]); the Windows containment itself (the
//! AppContainer profile, Job and mitigations) is in the media broker.

pub use crate::media::contained::ContainedWorker;
