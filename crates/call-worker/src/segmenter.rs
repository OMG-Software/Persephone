//! The person mask for the background effects (ADR-0068): blur and pictures
//! need to know which pixels show the person.
//!
//! This build has no segmenter. [`available`] is `false` and [`open`] gives
//! nothing, so the worker refuses blur and pictures and keeps the camera off
//! for them. The later segmenter (tract with an embedded model, ADR-0068)
//! goes behind [`Segmenter`] and needs no change to the session. It must
//! read the picture, but [`Planes`] has no read access yet: that step adds
//! read-only accessors for its size and planes to `effects.rs`, and the
//! write access stays private there. No LiveKit and no OS types.

use crate::effects::{Mask, Planes};

/// Makes one person mask for one camera picture.
pub trait Segmenter: Send {
    /// Writes the mask of `picture` into `out`: 0 is background, 255 is the
    /// person, at any size that [`Mask::resize`] allows. `false` when it
    /// cannot: the caller then drops the picture, so no picture goes out
    /// without its effect.
    fn mask(&mut self, picture: &Planes<'_>, out: &mut Mask) -> bool;
}

/// Whether this build can make person masks. It decides what the worker
/// reports in `Devices` and which effects it accepts.
pub const fn available() -> bool {
    false
}

/// A segmenter for one camera feed, or `None` when this build has none.
pub fn open() -> Option<Box<dyn Segmenter>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN this build
    /// WHEN the segmenter is asked for
    /// THEN none is available, and the report agrees with what `open` gives.
    #[test]
    fn given_this_build_when_asked_then_no_segmenter_is_available() {
        assert!(!available());
        assert_eq!(open().is_some(), available());
    }
}
