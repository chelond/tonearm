//! Listening history and the "continue where you left off" state, kept in the data directory
//! (`~/.local/share/tonearm`, `%LOCALAPPDATA%\tonearm\data`). Nothing leaves the computer.
//!
//! Two files: `history.json` holds the latest plays (capped, rewritten when a track ends) for
//! recents and the weekly stats; `plays/<year>.jsonl` keeps every play for good, one JSON line
//! each, appended and never rewritten. The yearly files feed the year summary and the smart
//! playlists, and are plain enough to export (artist, title, album, start time, length).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tonearm_core::fsutil;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_uri: Option<String>,
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

/// Appends one JSON line. If an earlier write was cut off mid-line (crash, full disk), the
/// unfinished line is closed first, so it costs only itself and not the play written now.
fn append_line(file: &std::path::Path, e: &Entry) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut line = serde_json::to_vec(e).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(file)?;
    if f.metadata()?.len() > 0 {
        let mut last = [0u8; 1];
        f.seek(SeekFrom::End(-1))?;
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.insert(0, b'\n');
        }
    }
    f.write_all(&line)
}

/// Writes atomically (unique temp file + rename) so a crash never leaves half a file.
/// Synchronous: only for the final save at exit (after [`fsutil::flush`]) and for tests.
fn write_json<T: Serialize>(name: &str, value: &T) {
    let Some(p) = path(name) else {
        return;
    };
    let Ok(data) = serde_json::to_vec(value) else {
        return;
    };
    if let Err(e) = fsutil::write_atomic(&p, &data, fsutil::Access::Private) {
        tracing::warn!(error = %e, file = name, "cannot save");
    }
}

/// Queues the write on the shared writer thread (keeps order, newest snapshot wins).
fn write_json_later<T: Serialize + Send + 'static>(name: &str, value: T) {
    let Some(p) = path(name) else {
        return;
    };
    fsutil::write_later(p, fsutil::Access::Private, move || {
        serde_json::to_vec(&value).ok()
    });
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    let bytes = std::fs::read(path(name)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

const HISTORY_FILE: &str = "history.json";
const RESUME_FILE: &str = "resume.json";
const PLAYS_DIR: &str = "plays";

/// Local calendar year of a unix time.
#[must_use]
pub fn year_of(at: u64) -> i32 {
    use chrono::Datelike;
    i64::try_from(at)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map_or(1970, |t| t.with_timezone(&chrono::Local).year())
}

/// The permanent play log: one `<year>.jsonl` file per year.
pub struct PlayLog {
    dir: PathBuf,
}

impl PlayLog {
    #[must_use]
    pub fn open() -> Option<Self> {
        path(PLAYS_DIR).map(|dir| Self { dir })
    }

    #[cfg(test)]
    #[must_use]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn file(&self, year: i32) -> PathBuf {
        self.dir.join(format!("{year}.jsonl"))
    }

    /// Adds one play at the end of its year's file.
    pub fn append(&self, e: &Entry) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        append_line(&self.file(year_of(e.at)), e)
    }

    /// First start with the log: copy the history kept so far into it, so the year summary
    /// starts from everything already known. Written into a side directory and renamed into
    /// place at the end, so a crash halfway leaves no half-seeded log (it is redone next time).
    pub fn seed(&self, entries: &[Entry]) {
        if self.dir.exists() {
            return;
        }
        let tmp = self.dir.with_extension("seeding");
        let _ = std::fs::remove_dir_all(&tmp);
        let seeded = std::fs::create_dir_all(&tmp).and_then(|()| {
            for e in entries {
                append_line(&tmp.join(format!("{}.jsonl", year_of(e.at))), e)?;
            }
            std::fs::rename(&tmp, &self.dir)
        });
        if let Err(err) = seeded {
            tracing::warn!(error = %err, "cannot write the play log");
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    /// Plays that happened in `year` in local time. A play near New Year can sit in the
    /// neighbouring file (the file is picked by the time zone at the moment of the play), so
    /// those are read too.
    #[must_use]
    pub fn plays_in(&self, year: i32) -> Vec<Entry> {
        let mut out: Vec<Entry> = [year - 1, year, year + 1]
            .into_iter()
            .flat_map(|y| self.read_year(y))
            .filter(|e| year_of(e.at) == year)
            .collect();
        out.sort_by_key(|e| e.at);
        out
    }

    /// Years with plays, newest first.
    #[must_use]
    pub fn years(&self) -> Vec<i32> {
        let mut years: Vec<i32> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|d| {
                let name = d.file_name().into_string().ok()?;
                name.strip_suffix(".jsonl")?.parse().ok()
            })
            .collect();
        years.sort_unstable_by(|a, b| b.cmp(a));
        years
    }

    /// Plays of one year in the order they happened. A damaged line is skipped, not fatal.
    #[must_use]
    pub fn read_year(&self, year: i32) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(self.file(year)) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Every play ever logged, oldest first.
    #[must_use]
    pub fn read_all(&self) -> Vec<Entry> {
        let mut years = self.years();
        years.reverse();
        years.into_iter().flat_map(|y| self.read_year(y)).collect()
    }
}

#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Store {
    #[must_use]
    pub fn load() -> Self {
        let store = Self {
            entries: read_json(HISTORY_FILE).unwrap_or_default(),
            resume: read_json(RESUME_FILE),
        };
        if let Some(log) = PlayLog::open() {
            log.seed(&store.entries);
        }
        store
    }

    /// The history (written when a track ends) and the resume point. Synchronous: use at exit,
    /// after [`fsutil::flush`], so no queued older snapshot lands after it.
    pub fn save(&self) {
        write_json(HISTORY_FILE, &self.entries);
        if let Some(r) = &self.resume {
            write_json(RESUME_FILE, r);
        }
    }

    /// Like [`Store::save`], queued on the writer thread (serialised there, not on the caller).
    pub fn save_later(self) {
        write_json_later(HISTORY_FILE, self.entries);
        if let Some(r) = self.resume {
            write_json_later(RESUME_FILE, r);
        }
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

/// Only the small resume file (written every few seconds while playing), queued.
pub fn save_resume(resume: Option<Resume>) {
    if let Some(r) = resume {
        write_json_later(RESUME_FILE, r);
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
    fn play_log_appends_by_year_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let log = PlayLog::at(dir.path().join("plays"));
        // 2025-06-01 and 2026-06-01 (midday UTC: the same year in any time zone).
        let (a, b) = (1_748_779_200, 1_780_315_200);
        log.seed(&[entry("t1", "A", a, "")]);
        // Seeding happens once only.
        log.seed(&[entry("x", "A", a, "")]);
        log.append(&entry("t2", "B", b, "")).unwrap();
        log.append(&entry("t3", "B", b + 10, "")).unwrap();
        assert_eq!(log.years(), [2026, 2025]);
        let y: Vec<String> = log.read_year(2026).into_iter().map(|e| e.uri).collect();
        assert_eq!(y, ["t2", "t3"]);
        let all: Vec<String> = log.read_all().into_iter().map(|e| e.uri).collect();
        assert_eq!(all, ["t1", "t2", "t3"]);
        // A broken line does not lose the rest.
        let f = dir.path().join("plays/2026.jsonl");
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str("{broken\n");
        std::fs::write(&f, text).unwrap();
        log.append(&entry("t4", "B", b + 20, "")).unwrap();
        assert_eq!(log.read_year(2026).len(), 3);
    }

    #[test]
    fn a_cut_off_line_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let log = PlayLog::at(dir.path().join("plays"));
        let b = 1_780_315_200;
        log.append(&entry("t1", "A", b, "")).unwrap();
        let f = dir.path().join("plays/2026.jsonl");
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str("{\"uri\":\"half"); // crash in the middle of a write
        std::fs::write(&f, text).unwrap();
        log.append(&entry("t2", "A", b + 1, "")).unwrap();
        let uris: Vec<String> = log.read_year(2026).into_iter().map(|e| e.uri).collect();
        assert_eq!(uris, ["t1", "t2"]);
    }

    #[test]
    fn plays_near_new_year_count_for_their_local_year() {
        let dir = tempfile::tempdir().unwrap();
        let log = PlayLog::at(dir.path().join("plays"));
        let mid_2026 = 1_780_315_200;
        // A 2026 play that ended up in 2025.jsonl (written under another time zone).
        std::fs::create_dir_all(dir.path().join("plays")).unwrap();
        append_line(
            &dir.path().join("plays/2025.jsonl"),
            &entry("moved", "A", mid_2026, ""),
        )
        .unwrap();
        log.append(&entry("t1", "A", mid_2026 + 5, "")).unwrap();
        let uris: Vec<String> = log.plays_in(2026).into_iter().map(|e| e.uri).collect();
        assert_eq!(uris, ["moved", "t1"]);
        assert!(log.plays_in(2025).is_empty());
    }

    #[test]
    fn seeding_is_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log = PlayLog::at(dir.path().join("plays"));
        // Left over by a crash during seeding.
        std::fs::create_dir_all(dir.path().join("plays.seeding")).unwrap();
        std::fs::write(dir.path().join("plays.seeding/2026.jsonl"), "{half").unwrap();
        log.seed(&[entry("t1", "A", 1_780_315_200, "")]);
        assert!(!dir.path().join("plays.seeding").exists());
        assert_eq!(log.read_year(2026).len(), 1);
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
