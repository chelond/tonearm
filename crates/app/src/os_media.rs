//! OS media integration: MPRIS on Linux (media keys, GNOME/KDE player widgets, `playerctl`),
//! System Media Transport Controls on Windows, and a desktop notification on track change.
//!
//! Lives on the UI thread: SMTC needs the window handle, and updates come from player events
//! that are already dispatched there. Controls from the OS go straight to the player channel.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tonearm_player::{PlaybackState, PlayerCommand, TrackInfo};

use crate::AppWindow;

/// Seek step for "seek forward/backward" without an amount (media keys, `playerctl position 5+`).
const SEEK_STEP: Duration = Duration::from_secs(5);

struct OsMedia {
    controls: MediaControls,
    /// Last known position, shared with the media event thread for relative seeks.
    position: Arc<AtomicU32>,
    state: PlaybackState,
    position_ms: u32,
    duration_ms: u32,
    track_uri: String,
}

thread_local! {
    static MEDIA: RefCell<Option<OsMedia>> = const { RefCell::new(None) };
}

/// What the OS can ask the UI to do (raise the window, quit).
pub enum UiRequest {
    Raise,
    Quit,
}

/// Registers the media controls. Call after the window is shown (SMTC needs its handle).
///
/// Returns `false` when the native window does not exist yet (Windows): call again later.
/// souvlaki panics without an HWND on Windows, so it is never called without one.
pub fn init(
    window: &AppWindow,
    player: impl Fn(PlayerCommand) + Send + 'static,
    ui: impl Fn(UiRequest) + Send + 'static,
) -> bool {
    let hwnd = window_handle(window);
    if cfg!(windows) && hwnd.is_none() {
        return false;
    }
    let config = PlatformConfig {
        display_name: "Tonearm",
        dbus_name: "tonearm",
        hwnd,
    };
    let mut controls = match MediaControls::new(config) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = ?e, "media controls unavailable");
            return true;
        }
    };
    let position = Arc::new(AtomicU32::new(0));
    let pos = Arc::clone(&position);
    let handler = move |event: MediaControlEvent| {
        let cmd = match event {
            MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
                // The player only knows "toggle"; Play while playing is rare and harmless to skip.
                Some(PlayerCommand::TogglePlay)
            }
            MediaControlEvent::Next => Some(PlayerCommand::Next),
            MediaControlEvent::Previous => Some(PlayerCommand::Prev),
            MediaControlEvent::Stop => Some(PlayerCommand::TogglePlay),
            MediaControlEvent::Seek(dir) => Some(seek_by(&pos, dir, SEEK_STEP)),
            MediaControlEvent::SeekBy(dir, by) => Some(seek_by(&pos, dir, by)),
            MediaControlEvent::SetPosition(MediaPosition(p)) => Some(PlayerCommand::Seek {
                position_ms: u32::try_from(p.as_millis()).unwrap_or(u32::MAX),
            }),
            MediaControlEvent::SetVolume(v) => Some(PlayerCommand::SetVolume(
                // Clamped to 0..=100 before the cast.
                (v.clamp(0.0, 1.0) * 100.0).round() as u8,
            )),
            MediaControlEvent::OpenUri(uri) => Some(PlayerCommand::Load(uri)),
            MediaControlEvent::Raise => {
                ui(UiRequest::Raise);
                None
            }
            MediaControlEvent::Quit => {
                ui(UiRequest::Quit);
                None
            }
            _ => None,
        };
        if let Some(cmd) = cmd {
            player(cmd);
        }
    };
    if let Err(e) = controls.attach(handler) {
        tracing::warn!(error = ?e, "media controls: attach failed");
        return true;
    }
    MEDIA.with(|m| {
        *m.borrow_mut() = Some(OsMedia {
            controls,
            position,
            state: PlaybackState::Stopped,
            position_ms: 0,
            duration_ms: 0,
            track_uri: String::new(),
        });
    });
    true
}

fn seek_by(position: &AtomicU32, dir: SeekDirection, by: Duration) -> PlayerCommand {
    let now = position.load(Ordering::Relaxed);
    let by = u32::try_from(by.as_millis()).unwrap_or(u32::MAX);
    PlayerCommand::Seek {
        position_ms: match dir {
            SeekDirection::Forward => now.saturating_add(by),
            SeekDirection::Backward => now.saturating_sub(by),
        },
    }
}

#[cfg(windows)]
fn window_handle(window: &AppWindow) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::ComponentHandle;
    let handle = window.window().window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

#[cfg(not(windows))]
fn window_handle(_window: &AppWindow) -> Option<*mut std::ffi::c_void> {
    None
}

fn with(f: impl FnOnce(&mut OsMedia)) {
    MEDIA.with(|m| {
        if let Some(media) = m.borrow_mut().as_mut() {
            f(media);
        }
    });
}

pub fn on_state(state: PlaybackState, position_ms: u32) {
    with(|m| {
        m.position.store(position_ms, Ordering::Relaxed);
        // Position ticks every 500 ms; MPRIS clients interpolate, so only push real changes
        // and jumps (seeks), not every tick.
        let expected = m.position_ms.saturating_add(500);
        let jumped = position_ms.abs_diff(expected) > 1500;
        m.position_ms = position_ms;
        if state == m.state && !jumped {
            return;
        }
        m.state = state;
        let progress = Some(MediaPosition(Duration::from_millis(u64::from(position_ms))));
        let playback = match state {
            PlaybackState::Playing => MediaPlayback::Playing { progress },
            PlaybackState::Paused | PlaybackState::Loading => MediaPlayback::Paused { progress },
            PlaybackState::Stopped => MediaPlayback::Stopped,
        };
        if let Err(e) = m.controls.set_playback(playback) {
            tracing::debug!(error = ?e, "media controls: set_playback");
        }
    });
}

/// New track: update the OS widgets; notify when the window is not in front.
pub fn on_track(track: &TrackInfo, window_focused: bool, notify: bool) {
    let mut changed = false;
    with(|m| {
        changed = m.track_uri != track.uri;
        m.track_uri.clone_from(&track.uri);
        m.duration_ms = track.duration_ms;
        let metadata = MediaMetadata {
            title: Some(&track.title),
            album: Some(track.album.as_str()).filter(|a| !a.is_empty()),
            artist: Some(&track.artists),
            cover_url: track.cover_url.as_deref(),
            duration: Some(Duration::from_millis(u64::from(track.duration_ms))),
        };
        if let Err(e) = m.controls.set_metadata(metadata) {
            tracing::debug!(error = ?e, "media controls: set_metadata");
        }
    });
    if changed && notify && !window_focused {
        notification::track_changed(&track.title, &track.artists);
    }
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn on_volume(percent: u8) {
    #[cfg(target_os = "linux")]
    with(|m| {
        if let Err(e) = m.controls.set_volume(f64::from(percent) / 100.0) {
            tracing::debug!(error = ?e, "media controls: set_volume");
        }
    });
}

#[cfg(target_os = "linux")]
mod notification {
    //! `org.freedesktop.Notifications` over zbus (already in the tree via keyring and souvlaki),
    //! so no notification daemon library is needed. Each notification replaces the previous one.

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    static LAST_ID: AtomicU32 = AtomicU32::new(0);

    pub fn track_changed(title: &str, artists: &str) {
        let (title, artists) = (title.to_owned(), artists.to_owned());
        std::thread::spawn(move || {
            if let Err(e) = send(&title, &artists) {
                tracing::debug!(error = %e, "notification not shown");
            }
        });
    }

    fn send(title: &str, artists: &str) -> zbus::Result<()> {
        let conn = zbus::blocking::Connection::session()?;
        let mut hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        hints.insert("category", "x-gnome.music".into());
        hints.insert("transient", true.into());
        let reply = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Tonearm",
                LAST_ID.load(Ordering::Relaxed),
                "audio-x-generic",
                title,
                artists,
                Vec::<&str>::new(),
                hints,
                4000i32,
            ),
        )?;
        let id: u32 = reply.body().deserialize()?;
        LAST_ID.store(id, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod notification {
    // Windows shows its own media overlay through SMTC; no separate toast.
    pub fn track_changed(_title: &str, _artists: &str) {}
}
