// PERSEPHONE PATCH (ADR-0062): the microphone gain and a report of the audio
// processing module. Not part of upstream libwebrtc.

use webrtc_sys::peer_connection_factory::ffi as sys_pcf;

/// The highest microphone gain, in percent (ADR-0062).
pub const MAX_CAPTURE_GAIN_PERCENT: u16 = 400;

/// Sets the microphone gain, in percent, for the audio processing module of
/// every peer connection factory, now and later: a linear gain on the
/// captured sound before any processing. 100 is no change. A gain over
/// [`MAX_CAPTURE_GAIN_PERCENT`] changes nothing and returns `false`.
pub fn set_capture_gain_percent(percent: u16) -> bool {
    sys_pcf::set_capture_gain_percent(percent)
}

/// One processing component as WebRTC reports it: what was asked and what
/// runs. `None` is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    pub requested: Option<bool>,
    pub active: Option<bool>,
}

/// What the audio processing module of the newest peer connection factory
/// does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureProcessing {
    /// The microphone gain in percent that the module applies. 100 when the
    /// adjustment is off.
    pub gain_percent: u32,
    pub echo_cancellation: Component,
    pub noise_suppression: Component,
    pub auto_gain_control: Component,
    /// The audio device module: playout prepared, playout running and capture
    /// running.
    pub playout_initialized: bool,
    pub playing: bool,
    pub recording: bool,
}

fn tri(value: i8) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// The state of the newest live factory, or `None` when there is none.
pub fn capture_processing() -> Option<CaptureProcessing> {
    let state = sys_pcf::capture_processing_state();
    state.found.then(|| CaptureProcessing {
        gain_percent: state.gain_percent,
        echo_cancellation: Component {
            requested: tri(state.echo_requested),
            active: tri(state.echo_active),
        },
        noise_suppression: Component {
            requested: tri(state.noise_requested),
            active: tri(state.noise_active),
        },
        auto_gain_control: Component {
            requested: tri(state.agc_requested),
            active: tri(state.agc_active),
        },
        playout_initialized: state.playout_initialized,
        playing: state.playing,
        recording: state.recording,
    })
}
