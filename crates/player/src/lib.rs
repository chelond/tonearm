//! Player layer: playback on top of librespot, play queue with shuffle/repeat, volume, seek, events.

pub mod queue;
mod service;
pub mod soloist;
pub mod uri;

pub use queue::{QueueItem, QueueSnapshot};
pub use service::run;
pub use soloist::{SoloistConfig, SoloistStatus};

/// Commands from the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerCommand {
    /// Play a pasted URI or `open.spotify.com` link from the start (a one-track context).
    Load(String),
    /// Play `items` (a playlist, album, search results…) starting at `start`.
    /// `shuffle: Some(true)` is the "Перемешать" button.
    PlayContext {
        items: Vec<QueueItem>,
        start: usize,
        name: String,
        shuffle: Option<bool>,
    },
    Next,
    /// Restarts the track if it has played for more than 3 s, otherwise goes back.
    Prev,
    ToggleShuffle,
    /// Off → All → One → Off.
    CycleRepeat,
    /// "Добавить в очередь": plays after the current track, before the context continues.
    Enqueue(QueueItem),
    /// Play the n-th entry of the upcoming list.
    JumpTo(usize),
    /// Drop the hand-queued tracks.
    ClearQueue,
    /// Drag and drop in the queue panel: entry `from` of the upcoming list goes to `to`.
    MoveInQueue {
        from: usize,
        to: usize,
    },
    /// New audio settings; a new output/bitrate applies from the next track.
    Configure(AudioOptions),
    /// Pause if playing (playback moved to another device).
    Pause,
    /// Continue `uri` from `position_ms` (playback came back from another device): inside the
    /// current queue if it is there, otherwise as a one-track queue.
    PlayAt {
        uri: String,
        position_ms: u32,
    },
    TogglePlay,
    /// Seek to a position in the current track.
    Seek {
        position_ms: u32,
    },
    /// Volume in percent, 0..=100.
    SetVolume(u8),
    /// Play through Spotify Soloist (`Some`) or the built-in librespot player (`None`).
    Soloist(Option<SoloistConfig>),
    /// Try Soloist again after a failure, or sign it in again.
    SoloistRetry,
}

/// Audio settings the player needs (mirrors the `[audio]` section of config.toml).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOptions {
    /// 96 | 160 | 320
    pub bitrate: u16,
    pub normalisation: bool,
    pub gapless: bool,
    /// Output device name; empty = system default.
    pub device: String,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            bitrate: 320,
            normalisation: false,
            gapless: true,
            device: String::new(),
        }
    }
}

/// Names of the audio outputs (for the settings page). Blocking: call off the UI thread.
#[must_use]
pub fn output_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    let mut names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
    names.dedup();
    names
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

/// What the player bar shows about the current track.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackInfo {
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
}

/// Events to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    State {
        state: PlaybackState,
        position_ms: u32,
    },
    Track(TrackInfo),
    /// Volume in percent, 0..=100.
    Volume(u8),
    Queue(QueueSnapshot),
    Error(String),
    /// Spotify refused the built-in player's audio keys several times in a row (librespot
    /// issue #1649, mostly newer accounts): offer Soloist.
    AudioKeyRefused,
    Soloist(SoloistStatus),
}

/// Repeat mode of the play queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    /// Cycles Off -> All -> One -> Off, like the repeat button in the player bar.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

/// Converts a 0..=100 percentage to librespot's 0..=u16::MAX volume.
#[must_use]
pub fn percent_to_volume(percent: u8) -> u16 {
    let p = u32::from(percent.min(100));
    u16::try_from(p * u32::from(u16::MAX) / 100).unwrap_or(u16::MAX)
}

/// Converts librespot's 0..=u16::MAX volume to a rounded percentage.
#[must_use]
pub fn volume_to_percent(volume: u16) -> u8 {
    let v = u32::from(volume);
    u8::try_from((v * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_cycles() {
        assert_eq!(RepeatMode::Off.next().next().next(), RepeatMode::Off);
    }

    #[test]
    fn volume_roundtrip() {
        for p in 0..=100u8 {
            assert_eq!(volume_to_percent(percent_to_volume(p)), p);
        }
        assert_eq!(percent_to_volume(200), u16::MAX);
    }
}
