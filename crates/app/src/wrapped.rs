//! The year summary ("Итоги года"): top tracks, artists and albums, hours listened, the month
//! and the time of day with the most music, the longest streak of days. Computed from the
//! permanent play log on this computer.

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Local, NaiveDate, Timelike};

use crate::history::Entry;

pub struct Summary {
    pub year: i32,
    pub minutes: u64,
    pub plays: usize,
    pub tracks: usize,
    pub artists: usize,
    /// Days with at least one play.
    pub days: usize,
    /// Most days in a row with music.
    pub streak: usize,
    /// Minutes per month, January first.
    pub months: [u64; 12],
    /// Hour of the day (0..24) with the most minutes.
    pub top_hour: Option<u32>,
    /// The first play of the year.
    pub first: Option<Entry>,
    /// Up to 50 (the page shows 10, "play" takes all).
    pub top_tracks: Vec<Ranked>,
    pub top_artists: Vec<Ranked>,
    pub top_albums: Vec<Ranked>,
}

/// One place in a top list. `entry` is the most played track of the group (its cover, links).
pub struct Ranked {
    pub title: String,
    pub subtitle: String,
    pub uri: Option<String>,
    pub entry: Entry,
    pub plays: usize,
    pub minutes: u64,
}

fn local(at: u64) -> Option<chrono::DateTime<Local>> {
    let secs = i64::try_from(at).ok()?;
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.with_timezone(&Local))
}

pub fn first_artist(artists: &str) -> &str {
    artists.split(", ").next().unwrap_or("").trim()
}

#[derive(Default)]
struct Group {
    plays: usize,
    ms: u64,
    uri: Option<String>,
    /// Plays per track uri inside the group, to pick its face.
    per_track: HashMap<String, (usize, Entry)>,
}

impl Group {
    fn add(&mut self, e: &Entry, uri: Option<&String>) {
        self.plays += 1;
        self.ms += u64::from(e.listened_ms);
        if self.uri.is_none() {
            self.uri = uri.cloned();
        }
        let t = self
            .per_track
            .entry(e.uri.clone())
            .or_insert_with(|| (0, e.clone()));
        t.0 += 1;
        // The latest copy has the most complete details.
        if e.image_url.is_some() || t.1.image_url.is_none() {
            t.1 = e.clone();
        }
    }

    fn face(&self) -> Entry {
        self.per_track
            .values()
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.title.cmp(&a.1.title)))
            .map(|t| t.1.clone())
            .unwrap_or_default()
    }
}

fn ranked(
    groups: HashMap<String, Group>,
    n: usize,
    label: impl Fn(&str, &Entry) -> (String, String),
) -> Vec<Ranked> {
    let mut list: Vec<Ranked> = groups
        .into_iter()
        .map(|(key, g)| {
            let entry = g.face();
            let (title, subtitle) = label(&key, &entry);
            Ranked {
                title,
                subtitle,
                uri: g.uri,
                entry,
                plays: g.plays,
                minutes: g.ms / 60_000,
            }
        })
        .collect();
    list.sort_by(|a, b| {
        b.plays
            .cmp(&a.plays)
            .then(b.minutes.cmp(&a.minutes))
            .then(a.title.cmp(&b.title))
    });
    list.truncate(n);
    list
}

/// Longest run of consecutive days in a sorted, deduplicated list.
fn longest_streak(days: &[NaiveDate]) -> usize {
    let mut best = 0;
    let mut run = 0;
    let mut prev: Option<NaiveDate> = None;
    for &d in days {
        run = match prev {
            Some(p) if p.succ_opt() == Some(d) => run + 1,
            _ => 1,
        };
        best = best.max(run);
        prev = Some(d);
    }
    best
}

#[must_use]
pub fn summary(entries: &[Entry], year: i32) -> Summary {
    let mut minutes_ms = 0u64;
    let mut plays = 0;
    let mut months = [0u64; 12];
    let mut hours = [0u64; 24];
    let mut days: HashSet<NaiveDate> = HashSet::new();
    let mut tracks: HashMap<String, Group> = HashMap::new();
    let mut artists: HashMap<String, Group> = HashMap::new();
    let mut albums: HashMap<String, Group> = HashMap::new();
    let mut first: Option<&Entry> = None;
    for e in entries {
        let Some(t) = local(e.at) else { continue };
        if t.year() != year {
            continue;
        }
        plays += 1;
        let ms = u64::from(e.listened_ms);
        minutes_ms += ms;
        months[t.month0() as usize] += ms;
        hours[t.hour() as usize] += ms;
        days.insert(t.date_naive());
        if first.is_none_or(|f| e.at < f.at) {
            first = Some(e);
        }
        tracks
            .entry(e.uri.clone())
            .or_default()
            .add(e, Some(&e.uri));
        let artist = first_artist(&e.artists);
        if !artist.is_empty() {
            artists
                .entry(artist.to_owned())
                .or_default()
                .add(e, e.artist_uri.as_ref());
        }
        if !e.album.is_empty() {
            let key = format!("{}\u{1f}{artist}", e.album);
            albums.entry(key).or_default().add(e, e.album_uri.as_ref());
        }
    }
    let track_count = tracks.len();
    let artist_count = artists.len();
    let mut day_list: Vec<NaiveDate> = days.into_iter().collect();
    day_list.sort_unstable();
    let top_hour = (0..24u32)
        .filter(|&h| hours[h as usize] > 0)
        .max_by_key(|&h| hours[h as usize]);
    Summary {
        year,
        minutes: minutes_ms / 60_000,
        plays,
        tracks: track_count,
        artists: artist_count,
        days: day_list.len(),
        streak: longest_streak(&day_list),
        months: months.map(|ms| ms / 60_000),
        top_hour,
        first: first.cloned(),
        top_tracks: ranked(tracks, 50, |_, e| (e.title.clone(), e.artists.clone())),
        top_artists: ranked(artists, 5, |name, _| (name.to_owned(), String::new())),
        top_albums: ranked(albums, 5, |key, e| {
            let album = key.split('\u{1f}').next().unwrap_or(&e.album).to_owned();
            (album, first_artist(&e.artists).to_owned())
        }),
    }
}

/// "утром", "днём"… for the hour with the most music.
#[must_use]
pub fn part_of_day(hour: u32) -> &'static str {
    match hour {
        5..=11 => "утром",
        12..=17 => "днём",
        18..=23 => "вечером",
        _ => "ночью",
    }
}

pub const MONTHS: [&str; 12] = [
    "январь",
    "февраль",
    "март",
    "апрель",
    "май",
    "июнь",
    "июль",
    "август",
    "сентябрь",
    "октябрь",
    "ноябрь",
    "декабрь",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32) -> u64 {
        use chrono::TimeZone;
        u64::try_from(
            Local
                .with_ymd_and_hms(y, m, d, h, 0, 0)
                .single()
                .unwrap()
                .timestamp(),
        )
        .unwrap()
    }

    fn play(uri: &str, artist: &str, album: &str, at: u64) -> Entry {
        Entry {
            uri: uri.into(),
            title: uri.to_uppercase(),
            artists: artist.into(),
            album: album.into(),
            duration_ms: 200_000,
            listened_ms: 180_000,
            at,
            ..Entry::default()
        }
    }

    #[test]
    fn summary_counts_only_its_year() {
        let entries = vec![
            play("old", "Z", "Zz", at(2025, 12, 31, 12)),
            play("t1", "A, B", "Alb", at(2026, 3, 1, 21)),
            play("t1", "A", "Alb", at(2026, 3, 2, 21)),
            play("t2", "A", "Alb", at(2026, 3, 3, 9)),
            play("t3", "C", "Other", at(2026, 7, 10, 22)),
        ];
        let s = summary(&entries, 2026);
        assert_eq!(s.plays, 4);
        assert_eq!(s.minutes, 12);
        assert_eq!(s.tracks, 3);
        assert_eq!(s.artists, 2);
        assert_eq!(s.days, 4);
        assert_eq!(s.streak, 3);
        assert_eq!(s.months[2], 9);
        assert_eq!(s.months[6], 3);
        assert_eq!(s.top_hour, Some(21));
        assert_eq!(s.first.as_ref().unwrap().uri, "t1");
        assert_eq!(s.top_tracks[0].entry.uri, "t1");
        assert_eq!(s.top_tracks[0].plays, 2);
        assert_eq!(s.top_artists[0].title, "A");
        assert_eq!(s.top_artists[0].plays, 3);
        assert_eq!(s.top_albums[0].title, "Alb");
        assert_eq!(s.top_albums[0].subtitle, "A");
        assert_eq!(s.top_albums[0].entry.uri, "t1");
    }

    #[test]
    fn empty_year_is_empty() {
        let s = summary(&[], 2026);
        assert_eq!(s.plays, 0);
        assert_eq!(s.streak, 0);
        assert!(s.top_hour.is_none());
        assert!(s.top_tracks.is_empty());
    }

    #[test]
    fn streak_breaks_on_a_gap() {
        let d = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        assert_eq!(
            longest_streak(&[d(1, 1), d(1, 2), d(1, 4), d(1, 5), d(1, 6)]),
            3
        );
        assert_eq!(longest_streak(&[d(2, 28), d(3, 1)]), 2);
    }
}
