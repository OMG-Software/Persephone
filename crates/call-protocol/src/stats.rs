//! The call statistics message (ADR-0066): closed numeric values only.
//!
//! Every field is an integer with a unit and an upper bound that the reader
//! checks. There is no text except the participants' identities, which the
//! worker has already validated for other messages. There is no address,
//! port, candidate type, SSRC or codec name.
//!
//! The page's contract repeats the `MAX_STATS_*` bounds as `CALL_STATS_*`
//! in `persephone-contracts`. The core checks at compile time that they
//! match. Change them together.

use super::{DecodeError, MAX_OTHER_CONNECTIONS, MAX_PARTICIPANTS, Reader, put_str};

/// The highest bitrate or bandwidth a statistic holds, in kbit/s (1 Gbit/s).
pub const MAX_STATS_KBPS: u32 = 1_000_000;
/// The longest time a statistic holds, in milliseconds (one minute).
pub const MAX_STATS_MS: u16 = 60_000;
/// The highest frame rate a statistic holds, in frames per second.
pub const MAX_STATS_FPS: u8 = 120;
/// The longest picture side a statistic holds, in pixels.
pub const MAX_STATS_SIDE: u16 = 16_384;
/// The most connections one message describes: the own SFU and the other
/// SFUs of ADR-0054.
pub const MAX_STATS_CONNECTIONS: usize = 1 + MAX_OTHER_CONNECTIONS as usize;

/// Why a sent video track runs below what was asked, as WebRTC reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum QualityLimit {
    None = 0,
    Cpu = 1,
    Bandwidth = 2,
    Other = 3,
}

impl TryFrom<u8> for QualityLimit {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        Ok(match value {
            0 => Self::None,
            1 => Self::Cpu,
            2 => Self::Bandwidth,
            3 => Self::Other,
            _ => return Err(DecodeError::InvalidValue),
        })
    }
}

/// The own microphone track. `None` fields are not known yet: a rate needs
/// two samples, and the far end reports loss, jitter and round-trip time
/// only after its first report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioSendStats {
    /// 0 to `MAX_SOUND_LEVEL` (100), as the meter shows it.
    pub level: u8,
    /// Kbit/s, at most [`MAX_STATS_KBPS`].
    pub bitrate_kbps: Option<u32>,
    /// Packets sent since the track began (saturates at `u32::MAX`).
    pub packets_sent: u32,
    /// Packets the far end reports lost.
    pub packets_lost: Option<u32>,
    /// Milliseconds, at most [`MAX_STATS_MS`].
    pub jitter_ms: Option<u16>,
    /// Milliseconds, at most [`MAX_STATS_MS`].
    pub rtt_ms: Option<u16>,
}

/// The own camera or screen share track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSendStats {
    /// At most [`MAX_STATS_FPS`].
    pub fps: u8,
    /// At most [`MAX_STATS_SIDE`] each.
    pub width: u16,
    pub height: u16,
    /// Kbit/s, at most [`MAX_STATS_KBPS`].
    pub bitrate_kbps: Option<u32>,
    pub limit: QualityLimit,
    /// Packets the far end reports lost.
    pub packets_lost: Option<u32>,
}

/// The audio of one remote participant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioReceiveStats {
    /// 0 to `MAX_SOUND_LEVEL` (100).
    pub level: u8,
    pub bitrate_kbps: Option<u32>,
    pub packets_received: u32,
    pub packets_lost: u32,
    /// Milliseconds, at most [`MAX_STATS_MS`].
    pub jitter_ms: u16,
    /// Share of the samples of the last second that WebRTC made up, 0 to
    /// 100.
    pub concealed_percent: Option<u8>,
}

/// The video of one remote participant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoReceiveStats {
    pub fps: u8,
    pub width: u16,
    pub height: u16,
    pub bitrate_kbps: Option<u32>,
    /// Frames dropped each second, over the last interval, at most
    /// [`MAX_STATS_FPS`]. Not known until the second sample.
    pub dropped_fps: Option<u8>,
    pub packets_lost: u32,
}

/// One remote participant, named by the identity and connection that the
/// worker already validated for its other messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteStats {
    pub connection: u8,
    pub identity: String,
    pub audio: Option<AudioReceiveStats>,
    pub video: Option<VideoReceiveStats>,
}

/// One connection to an SFU: 0 is the own one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionStats {
    pub connection: u8,
    /// The bandwidth that WebRTC estimates for sending, kbit/s.
    pub outgoing_kbps: Option<u32>,
    /// Everything sent and everything received on the connection, kbit/s.
    pub send_kbps: Option<u32>,
    pub receive_kbps: Option<u32>,
    pub rtt_ms: Option<u16>,
}

/// One reading of the call's statistics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CallStats {
    pub microphone: Option<AudioSendStats>,
    pub camera: Option<VideoSendStats>,
    pub screen: Option<VideoSendStats>,
    /// At most [`MAX_PARTICIPANTS`], at most one for each participant.
    pub remotes: Vec<RemoteStats>,
    /// At most [`MAX_STATS_CONNECTIONS`], at most one for each connection.
    pub connections: Vec<ConnectionStats>,
}

// Writing. The caller keeps every value inside its bound; the reader refuses
// anything else.

fn put_flag(out: &mut Vec<u8>, present: bool) {
    out.push(u8::from(present));
}

fn put_u32(out: &mut Vec<u8>, value: Option<u32>) {
    put_flag(out, value.is_some());
    if let Some(value) = value {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn put_u16(out: &mut Vec<u8>, value: Option<u16>) {
    put_flag(out, value.is_some());
    if let Some(value) = value {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn put_u8(out: &mut Vec<u8>, value: Option<u8>) {
    put_flag(out, value.is_some());
    if let Some(value) = value {
        out.push(value);
    }
}

fn put_audio_send(out: &mut Vec<u8>, audio: &AudioSendStats) {
    out.push(audio.level);
    put_u32(out, audio.bitrate_kbps);
    out.extend_from_slice(&audio.packets_sent.to_be_bytes());
    put_u32(out, audio.packets_lost);
    put_u16(out, audio.jitter_ms);
    put_u16(out, audio.rtt_ms);
}

fn put_video_send(out: &mut Vec<u8>, video: &VideoSendStats) {
    out.push(video.fps);
    out.extend_from_slice(&video.width.to_be_bytes());
    out.extend_from_slice(&video.height.to_be_bytes());
    put_u32(out, video.bitrate_kbps);
    out.push(video.limit as u8);
    put_u32(out, video.packets_lost);
}

fn put_audio_receive(out: &mut Vec<u8>, audio: &AudioReceiveStats) {
    out.push(audio.level);
    put_u32(out, audio.bitrate_kbps);
    out.extend_from_slice(&audio.packets_received.to_be_bytes());
    out.extend_from_slice(&audio.packets_lost.to_be_bytes());
    out.extend_from_slice(&audio.jitter_ms.to_be_bytes());
    put_u8(out, audio.concealed_percent);
}

fn put_video_receive(out: &mut Vec<u8>, video: &VideoReceiveStats) {
    out.push(video.fps);
    out.extend_from_slice(&video.width.to_be_bytes());
    out.extend_from_slice(&video.height.to_be_bytes());
    put_u32(out, video.bitrate_kbps);
    put_u8(out, video.dropped_fps);
    out.extend_from_slice(&video.packets_lost.to_be_bytes());
}

/// Writes `stats` after the message tag. The lists are cut to their limits,
/// so each count fits its byte. Like the other writers of this protocol, it
/// writes every entry as given: the reader refuses the whole message for an
/// entry out of bounds (an identity that is too long or has a control
/// character, a connection past `MAX_OTHER_CONNECTIONS`, a repeated entry, a
/// number past its maximum). The caller (`mapping::stats` in the worker)
/// leaves such entries out and clamps the numbers first.
pub(super) fn put_stats(out: &mut Vec<u8>, stats: &CallStats) {
    let present = u8::from(stats.microphone.is_some())
        | u8::from(stats.camera.is_some()) << 1
        | u8::from(stats.screen.is_some()) << 2;
    out.push(present);
    if let Some(audio) = &stats.microphone {
        put_audio_send(out, audio);
    }
    for video in [&stats.camera, &stats.screen].into_iter().flatten() {
        put_video_send(out, video);
    }
    let remotes = &stats.remotes[..stats.remotes.len().min(MAX_PARTICIPANTS)];
    out.push(remotes.len() as u8);
    for remote in remotes {
        out.push(remote.connection);
        put_str(out, &remote.identity);
        out.push(u8::from(remote.audio.is_some()) | u8::from(remote.video.is_some()) << 1);
        if let Some(audio) = &remote.audio {
            put_audio_receive(out, audio);
        }
        if let Some(video) = &remote.video {
            put_video_receive(out, video);
        }
    }
    let connections = &stats.connections[..stats.connections.len().min(MAX_STATS_CONNECTIONS)];
    out.push(connections.len() as u8);
    for connection in connections {
        out.push(connection.connection);
        put_u32(out, connection.outgoing_kbps);
        put_u32(out, connection.send_kbps);
        put_u32(out, connection.receive_kbps);
        put_u16(out, connection.rtt_ms);
    }
}

// Reading.

impl Reader<'_> {
    fn flag(&mut self) -> Result<bool, DecodeError> {
        self.bool()
    }

    fn kbps(&mut self) -> Result<Option<u32>, DecodeError> {
        if !self.flag()? {
            return Ok(None);
        }
        let value = self.u32()?;
        if value > MAX_STATS_KBPS {
            return Err(DecodeError::InvalidValue);
        }
        Ok(Some(value))
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn millis(&mut self) -> Result<u16, DecodeError> {
        let value = self.u16()?;
        if value > MAX_STATS_MS {
            return Err(DecodeError::InvalidValue);
        }
        Ok(value)
    }

    fn optional_millis(&mut self) -> Result<Option<u16>, DecodeError> {
        if self.flag()? {
            Ok(Some(self.millis()?))
        } else {
            Ok(None)
        }
    }

    fn optional_count(&mut self) -> Result<Option<u32>, DecodeError> {
        if self.flag()? {
            Ok(Some(self.u32()?))
        } else {
            Ok(None)
        }
    }

    fn fps(&mut self) -> Result<u8, DecodeError> {
        let value = self.u8()?;
        if value > MAX_STATS_FPS {
            return Err(DecodeError::InvalidValue);
        }
        Ok(value)
    }

    fn side(&mut self) -> Result<u16, DecodeError> {
        let value = self.u16()?;
        if value > MAX_STATS_SIDE {
            return Err(DecodeError::InvalidValue);
        }
        Ok(value)
    }

    fn percent(&mut self) -> Result<u8, DecodeError> {
        let value = self.u8()?;
        if value > 100 {
            return Err(DecodeError::InvalidValue);
        }
        Ok(value)
    }

    fn audio_send(&mut self) -> Result<AudioSendStats, DecodeError> {
        Ok(AudioSendStats {
            level: self.sound_level()?,
            bitrate_kbps: self.kbps()?,
            packets_sent: self.u32()?,
            packets_lost: self.optional_count()?,
            jitter_ms: self.optional_millis()?,
            rtt_ms: self.optional_millis()?,
        })
    }

    fn video_send(&mut self) -> Result<VideoSendStats, DecodeError> {
        Ok(VideoSendStats {
            fps: self.fps()?,
            width: self.side()?,
            height: self.side()?,
            bitrate_kbps: self.kbps()?,
            limit: QualityLimit::try_from(self.u8()?)?,
            packets_lost: self.optional_count()?,
        })
    }

    fn audio_receive(&mut self) -> Result<AudioReceiveStats, DecodeError> {
        Ok(AudioReceiveStats {
            level: self.sound_level()?,
            bitrate_kbps: self.kbps()?,
            packets_received: self.u32()?,
            packets_lost: self.u32()?,
            jitter_ms: self.millis()?,
            concealed_percent: if self.flag()? {
                Some(self.percent()?)
            } else {
                None
            },
        })
    }

    fn video_receive(&mut self) -> Result<VideoReceiveStats, DecodeError> {
        Ok(VideoReceiveStats {
            fps: self.fps()?,
            width: self.side()?,
            height: self.side()?,
            bitrate_kbps: self.kbps()?,
            dropped_fps: if self.flag()? {
                Some(self.fps()?)
            } else {
                None
            },
            packets_lost: self.u32()?,
        })
    }
}

/// Reads the statistics after the message tag. The counts are checked
/// before anything is allocated, and a participant or a connection that
/// appears twice is refused.
pub(super) fn read_stats(reader: &mut Reader<'_>) -> Result<CallStats, DecodeError> {
    let present = reader.u8()?;
    if present > 0b111 {
        return Err(DecodeError::InvalidValue);
    }
    let microphone = if present & 1 != 0 {
        Some(reader.audio_send()?)
    } else {
        None
    };
    let camera = if present & 2 != 0 {
        Some(reader.video_send()?)
    } else {
        None
    };
    let screen = if present & 4 != 0 {
        Some(reader.video_send()?)
    } else {
        None
    };
    let count = usize::from(reader.u8()?);
    if count > MAX_PARTICIPANTS {
        return Err(DecodeError::TooLarge);
    }
    let mut remotes: Vec<RemoteStats> = Vec::with_capacity(count);
    for _ in 0..count {
        let connection = reader.connection()?;
        let identity = reader.identity()?;
        if remotes
            .iter()
            .any(|seen| seen.connection == connection && seen.identity == identity)
        {
            return Err(DecodeError::InvalidValue);
        }
        let kinds = reader.u8()?;
        if kinds > 0b11 {
            return Err(DecodeError::InvalidValue);
        }
        let audio = if kinds & 1 != 0 {
            Some(reader.audio_receive()?)
        } else {
            None
        };
        let video = if kinds & 2 != 0 {
            Some(reader.video_receive()?)
        } else {
            None
        };
        remotes.push(RemoteStats {
            connection,
            identity,
            audio,
            video,
        });
    }
    let count = usize::from(reader.u8()?);
    if count > MAX_STATS_CONNECTIONS {
        return Err(DecodeError::TooLarge);
    }
    let mut connections: Vec<ConnectionStats> = Vec::with_capacity(count);
    for _ in 0..count {
        let connection = reader.connection()?;
        if connections.iter().any(|seen| seen.connection == connection) {
            return Err(DecodeError::InvalidValue);
        }
        connections.push(ConnectionStats {
            connection,
            outgoing_kbps: reader.kbps()?,
            send_kbps: reader.kbps()?,
            receive_kbps: reader.kbps()?,
            rtt_ms: reader.optional_millis()?,
        });
    }
    Ok(CallStats {
        microphone,
        camera,
        screen,
        remotes,
        connections,
    })
}

#[cfg(test)]
pub(crate) mod tests;
