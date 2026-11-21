//! User settings in `config.toml` (platform config dir). Never holds secrets: tokens and the
//! login live in the keyring / encrypted store. Unknown or broken values fall back to defaults,
//! so a hand-edited file can never stop the app from starting.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub spotify: SpotifySettings,
    pub audio: AudioSettings,
    pub effects: EffectsSettings,
    pub cache: CacheSettings,
    pub ui: UiSettings,
    pub player: PlayerSettings,
    pub update: UpdateSettings,
    pub lastfm: LastFmSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastFmSettings {
    /// Send plays to Last.fm (once signed in; the session key is in the keyring).
    pub scrobble: bool,
    /// Own API account from last.fm/api, instead of the one built into release builds.
    /// Empty = built-in.
    pub api_key: String,
    pub api_secret: String,
}

impl Default for LastFmSettings {
    fn default() -> Self {
        Self {
            scrobble: true,
            api_key: String::new(),
            api_secret: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerSettings {
    /// Play through Spotify Soloist instead of the built-in librespot player. The Soloist key
    /// itself lives in the keyring (`soloist-api-key`), never here.
    pub soloist: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Look for a new version at startup and once a day.
    pub check: bool,
    /// Release feed in GitHub's `releases/latest` JSON shape; empty = GitHub Releases of the
    /// project. Files are verified against the built-in signing key wherever they come from.
    pub feed_url: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check: true,
            feed_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotifySettings {
    /// Own app from developer.spotify.com for Web API tokens (avoids the shared client id's
    /// rate limit). Empty = login5 token of the session. Not a secret (PKCE).
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// 96 | 160 | 320 kbit/s.
    pub bitrate: u16,
    pub normalisation: bool,
    /// Preload the next track so there is no gap.
    pub gapless: bool,
    /// Output device name; empty = system default.
    pub device: String,
}

/// Equalizer, fades and visualizer. They work with Soloist on Linux with PipeWire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectsSettings {
    pub eq: bool,
    /// A preset id (`flat`, `bass`…) or `custom` for `eq_gains`.
    pub eq_preset: String,
    /// Ten band gains in dB, −12..=12 (31 Hz … 16 kHz). Kept for `custom`.
    pub eq_gains: Vec<i8>,
    /// Fade out the end of a track and fade in the next one, seconds; 0 = off.
    pub fade_secs: u8,
    /// Bars under the cover in the player bar.
    pub visualizer: bool,
}

impl Default for EffectsSettings {
    fn default() -> Self {
        Self {
            eq: false,
            eq_preset: "flat".into(),
            eq_gains: vec![0; EQ_BANDS],
            fade_secs: 0,
            visualizer: false,
        }
    }
}

pub const EQ_BANDS: usize = 10;
pub const FADE_SECS: [u8; 5] = [0, 2, 4, 6, 8];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheSettings {
    pub max_size_mb: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// Desktop notification on track change (only while the window is in the background).
    pub notifications: bool,
    /// Colour theme: a built-in id (`graphite`, `light`…) or `file:<name>.toml` from the
    /// themes folder. Unknown ids fall back to the default.
    pub theme: String,
    /// Lyrics lit word by word (adds NetEase, an unofficial source) instead of line by line.
    pub lyrics_words: bool,
    /// Album art behind the lyrics: 0 off, 1 subtle, 2 vivid.
    pub lyrics_backdrop: u8,
    /// The sidebar's smart playlists are folded.
    pub smart_collapsed: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            bitrate: 320,
            normalisation: false,
            gapless: true,
            device: String::new(),
        }
    }
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self { max_size_mb: 512 }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            notifications: true,
            theme: "graphite".into(),
            lyrics_words: false,
            lyrics_backdrop: 1,
            smart_collapsed: false,
        }
    }
}

pub const BITRATES: [u16; 3] = [96, 160, 320];
pub const CACHE_SIZES_MB: [u32; 4] = [128, 256, 512, 1024];

impl Settings {
    /// Clamps values a hand-edited file may have broken.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if !BITRATES.contains(&self.audio.bitrate) {
            self.audio.bitrate = 320;
        }
        self.cache.max_size_mb = self.cache.max_size_mb.clamp(32, 16_384);
        let fx = &mut self.effects;
        fx.eq_gains.resize(EQ_BANDS, 0);
        for g in &mut fx.eq_gains {
            *g = (*g).clamp(-12, 12);
        }
        fx.fade_secs = fx.fade_secs.min(12);
        if fx.eq_preset.is_empty() {
            fx.eq_preset = "flat".into();
        }
        self
    }

    #[must_use]
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "tonearm")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Loads the settings; a missing or unreadable file gives defaults (and a log line).
    #[must_use]
    pub fn load() -> Self {
        Self::path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Self>(&text)
                .map(Self::sanitized)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "config.toml is invalid, using defaults");
                    Self::default()
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read config.toml, using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        match Self::path() {
            Some(p) => self.save_to(&p),
            None => Ok(()),
        }
    }

    /// Atomic write: temp file + rename, so a crash never leaves half a config.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(
            &tmp,
            format!("# Tonearm settings. Tokens are never stored here.\n\n{text}"),
        )?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        assert_eq!(Settings::load_from(&path), Settings::default());
        let mut s = Settings::default();
        s.audio.bitrate = 160;
        s.audio.device = "USB DAC".into();
        s.ui.notifications = false;
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
    }

    #[test]
    fn broken_values_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[audio]\nbitrate = 999\n[cache]\nmax_size_mb = 1\n[extra]\nx = 1\n",
        )
        .unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.audio.bitrate, 320);
        assert_eq!(s.cache.max_size_mb, 32);
        assert!(s.audio.gapless, "missing keys default");
        std::fs::write(&path, "[effects]\neq_gains = [40, -3]\nfade_secs = 99\n").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.effects.eq_gains, [12, -3, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(s.effects.fade_secs, 12);
        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
    }
}
