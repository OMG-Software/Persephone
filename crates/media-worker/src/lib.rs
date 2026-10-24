//! The media worker's containment step (ADR-0003, ADR-0010, ADR-0011), as a
//! library so that the containment probe tests apply exactly the function the
//! worker binary runs before it reads input.

mod containment;

pub use containment::{
    CHECK_FAILED, MISSING_APPCONTAINER, MISSING_DYNAMIC_CODE_BLOCK, MISSING_JOB,
    MISSING_NO_NEW_PRIVS, MISSING_SANDBOX, MISSING_SECCOMP, MISSING_WIN32K_LOCKDOWN,
    UNSUPPORTED_PLATFORM, contain,
};
