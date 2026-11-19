//! Listening history and the "continue where you left off" state, kept in the data directory
//! (`~/.local/share/tonearm`, `%LOCALAPPDATA%\tonearm\data`). Nothing leaves the computer.
//!
//! The history is capped and written as one small JSON file; stats are computed from it.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// History entries kept (a few months of normal listening, well under 1 MB).
const MAX_ENTRIES: usize = 3000;
/// A track counts as listened after this much playback.
pub const COUNT_AFTER_MS: u32 = 30_000;
const WEEK_SECS: u64 = 7 * 24 * 3600;

/// What was playing from: a page the user can open again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Context {
    /// `spotify:playlist:…`, `spotify:album:…`, `liked`, or empty (search, radio, link).
    pub uri: String,
    /// 0 liked, 1 playlist, 2 album, 3 artist
    pub kind: i32,
    pub title: String,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Entry {
    pub uri: String,
    pub title: String,
    pub artists: String,
    #[serde(default)]
    pub artist_uri: Option<String>,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub image_url: Option<String>,
    pub duration_ms: u32,
    pub listened_ms: u32,
    /// Unix seconds when the track started.
    pub at: u64,
    #[serde(default)]
    pub context: Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QueuedTrack {
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub duration_ms: u32,
}

/// The session to continue at the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Resume {
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub cover_url: Option<String>,
    pub duration_ms: u32,
    pub position_ms: u32,
    /// The queue context and where the track is in it.
    pub items: Vec<QueuedTrack>,
    pub start: usize,
    pub context: Context,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub resume: Option<Resume>,
}

pub struct Stats {
    pub minutes: u64,
    pub plays: usize,
    pub artists: usize,
    /// (entry of the track, plays, minutes), most played first.
    pub top_tracks: Vec<(Entry, usize, u64)>,
    /// (artist name, first artist uri, plays, minutes).
    pub top_artists: Vec<(String, Option<String>, usize, u64)>,
}

fn path(name: &str) -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "tonearm").map(|d| d.data_dir().join(name))
}

/// Writes atomically (temp file + rename) so a crash never leaves half a file.
fn write_json<T: Serialize>(name: &str, value: &T) {
    let Some(p) = path(name) else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(data) = serde_json::to_vec(value) else {
        return;
    };
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    let bytes = std::fs::read(path(name)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

const HISTORY_FILE: &str = "history.json";
const RESUME_FILE: &str = "resume.json";

#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Store {
    #[must_use]
    pub fn load() -> Self {
        Self {
            entries: read_json(HISTORY_FILE).unwrap_or_default(),
            resume: read_json(RESUME_FILE),
        }
    }

    /// The history (written when a track ends) and the resume point.
    pub fn save(&self) {
        write_json(HISTORY_FILE, &self.entries);
        save_resume(self.resume.as_ref());
    }
    pub fn add(&mut self, e: Entry) {
        self.entries.push(e);
        if self.entries.len() > MAX_ENTRIES {
            let extra = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..extra);
        }
    }

    /// Newest first, one entry per track.
    #[must_use]
    pub fn recent_tracks(&self, n: usize) -> Vec<Entry> {
        let mut seen = std::collections::HashSet::new();
        self.entries
            .iter()
            .rev()
            .filter(|e| seen.insert(e.uri.clone()))
            .take(n)
            .cloned()
            .collect()
    }

    /// Pages played from recently (playlists, albums, Liked), newest first.
    #[must_use]
    pub fn recent_contexts(&self, n: usize) -> Vec<Context> {
        let mut seen = std::collections::HashSet::new();
        self.entries
            .iter()
            .rev()
            .map(|e| &e.context)
            .filter(|c| !c.uri.is_empty() && seen.insert(c.uri.clone()))
            .take(n)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn stats(&self, now: u64) -> Stats {
        let week: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.at + WEEK_SECS >= now)
            .collect();
        let minutes = week.iter().map(|e| u64::from(e.listened_ms)).sum::<u64>() / 60_000;
        let mut tracks: HashMap<&str, (Entry, usize, u64)> = HashMap::new();
        let mut artists: HashMap<String, (Option<String>, usize, u64)> = HashMap::new();
        for e in &week {
            let t = tracks
                .entry(e.uri.as_str())
                .or_insert_with(|| ((*e).clone(), 0, 0));
            t.1 += 1;
            t.2 += u64::from(e.listened_ms);
            let name = e.artists.split(", ").next().unwrap_or("").to_owned();
            if !name.is_empty() {
                let a = artists
                    .entry(name)
                    .or_insert_with(|| (e.artist_uri.clone(), 0, 0));
                a.1 += 1;
                a.2 += u64::from(e.listened_ms);
                if a.0.is_none() {
                    a.0.clone_from(&e.artist_uri);
                }
            }
        }
        let artist_count = artists.len();
        let mut top_tracks: Vec<(Entry, usize, u64)> = tracks
            .into_values()
            .map(|(e, n, ms)| (e, n, ms / 60_000))
            .collect();
        top_tracks.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(b.2.cmp(&a.2))
                .then(a.0.title.cmp(&b.0.title))
        });
        top_tracks.truncate(10);
        let mut top_artists: Vec<(String, Option<String>, usize, u64)> = artists
            .into_iter()
            .map(|(name, (uri, n, ms))| (name, uri, n, ms / 60_000))
            .collect();
        top_artists.sort_by(|a, b| b.2.cmp(&a.2).then(b.3.cmp(&a.3)).then(a.0.cmp(&b.0)));
        top_artists.truncate(10);
        Stats {
            minutes,
            plays: week.len(),
            artists: artist_count,
            top_tracks,
            top_artists,
        }
    }
}

/// Only the small resume file (written every few seconds while playing).
pub fn save_resume(resume: Option<&Resume>) {
    if let Some(r) = resume {
        write_json(RESUME_FILE, r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uri: &str, artist: &str, at: u64, ctx: &str) -> Entry {
        Entry {
            uri: uri.into(),
            title: uri.into(),
            artists: artist.into(),
            duration_ms: 180_000,
            listened_ms: 120_000,
            at,
            context: Context {
                uri: ctx.into(),
                ..Context::default()
            },
            ..Entry::default()
        }
    }

    #[test]
    fn stats_count_the_last_week_only() {
        let now = 1_000_000_000;
        let mut s = Store::default();
        s.add(entry("old", "A", now - WEEK_SECS - 10, ""));
        s.add(entry("t1", "A, B", now - 100, "p1"));
        s.add(entry("t1", "A", now - 50, "p1"));
        s.add(entry("t2", "C", now - 10, "p2"));
        let st = s.stats(now);
        assert_eq!(st.plays, 3);
        assert_eq!(st.minutes, 6);
        assert_eq!(st.artists, 2);
        assert_eq!(st.top_tracks[0].0.uri, "t1");
        assert_eq!(st.top_tracks[0].1, 2);
        assert_eq!(st.top_artists[0].0, "A");
    }

    #[test]
    fn recent_lists_are_newest_first_and_unique() {
        let mut s = Store::default();
        s.add(entry("t1", "A", 1, "p1"));
        s.add(entry("t2", "A", 2, "p2"));
        s.add(entry("t1", "A", 3, "p1"));
        s.add(entry("t3", "A", 4, ""));
        let tracks: Vec<String> = s.recent_tracks(10).into_iter().map(|e| e.uri).collect();
        assert_eq!(tracks, ["t3", "t1", "t2"]);
        let ctx: Vec<String> = s.recent_contexts(10).into_iter().map(|c| c.uri).collect();
        assert_eq!(ctx, ["p1", "p2"]);
    }

    #[test]
    fn history_is_capped() {
        let mut s = Store::default();
        for i in 0..(MAX_ENTRIES + 5) {
            s.add(entry(&i.to_string(), "A", i as u64, ""));
        }
        assert_eq!(s.entries.len(), MAX_ENTRIES);
        assert_eq!(s.entries[0].uri, "5");
    }

    #[test]
    fn store_round_trips_json() {
        let mut s = Store::default();
        s.add(entry("t1", "A", 1, "p1"));
        s.resume = Some(Resume {
            uri: "t1".into(),
            position_ms: 5000,
            ..Resume::default()
        });
        let json = serde_json::to_vec(&s).unwrap();
        let back: Store = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.entries, s.entries);
        assert_eq!(back.resume, s.resume);
    }
}
