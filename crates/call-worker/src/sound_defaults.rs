//! The system's default microphone and speaker for media.
//!
//! On Windows, libwebrtc opens the default *communications* endpoint when no
//! device is chosen (`adm_helpers::Init` selects
//! `kDefaultCommunicationDevice`). That can be another device than the one
//! Windows calls "Default Device", for example a virtual mixer, so the user
//! hears nothing. The worker reads the "console" default here and selects it
//! by its endpoint ID, which is the ID that libwebrtc lists for the device.
//! Other systems need no help: their default stream already is the system
//! default, so this reads nothing there.

/// The default endpoints: `(speaker, microphone)`. `None` when there is no
/// default or it cannot be read; libwebrtc's own choice then stays.
#[cfg(windows)]
pub fn media_defaults() -> (Option<String>, Option<String>) {
    // COM is set up on a thread of its own, so the runtime threads keep
    // their COM state, and the thread ends it again before it ends.
    std::thread::Builder::new()
        .name("sound-defaults".into())
        .spawn(windows_defaults::read)
        .ok()
        .and_then(|thread| thread.join().ok())
        .unwrap_or((None, None))
}

#[cfg(not(windows))]
pub fn media_defaults() -> (Option<String>, Option<String>) {
    (None, None)
}

#[cfg(windows)]
mod windows_defaults {
    use windows::Win32::Media::Audio::{
        EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, CoUninitialize,
    };

    /// Ends the COM use of this thread when dropped.
    struct Com;

    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: balances the successful `CoInitializeEx` in `read`, on
            // the same thread, after every COM object of it is dropped.
            unsafe { CoUninitialize() };
        }
    }

    pub fn read() -> (Option<String>, Option<String>) {
        // SAFETY: a new thread with no COM state; `Com` ends it on drop.
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return (None, None);
        }
        let _com = Com;
        // SAFETY: COM is initialised on this thread.
        let Ok(enumerator) = (unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(
                &MMDeviceEnumerator,
                None,
                CLSCTX_INPROC_SERVER,
            )
        }) else {
            return (None, None);
        };
        (
            endpoint_id(&enumerator, eRender),
            endpoint_id(&enumerator, eCapture),
        )
    }

    /// The ID of the console default endpoint of `flow`.
    fn endpoint_id(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Option<String> {
        // SAFETY: a valid enumerator on its COM thread; no default is an error.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }.ok()?;
        // SAFETY: `GetId` returns a string that the caller frees with
        // `CoTaskMemFree`, once, after it is copied.
        unsafe {
            let id = device.GetId().ok()?;
            let text = id.to_string().ok();
            CoTaskMemFree(Some(id.as_ptr().cast()));
            text
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::media_defaults;

    /// GIVEN this Windows machine WHEN the media defaults are read THEN each
    /// one that exists is an endpoint ID of its own direction, the form that
    /// libwebrtc lists (render `{0.0.0.…}`, capture `{0.0.1.…}`).
    #[test]
    fn given_windows_defaults_then_they_are_endpoint_ids_of_their_direction() {
        let (speaker, microphone) = media_defaults();
        if let Some(speaker) = speaker {
            assert!(speaker.starts_with("{0.0.0."), "{speaker}");
        }
        if let Some(microphone) = microphone {
            assert!(microphone.starts_with("{0.0.1."), "{microphone}");
        }
    }
}
