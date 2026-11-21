//! Smart playlists: lists built from rules over Liked Songs and the local play log, e.g.
//! "liked tracks not played for a month". They live only on this computer (`smart.json` in the
//! config dir) and are rebuilt every time they are opened.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tonearm_core::library::TrackItem;

use crate::history::Entry;

/// Page uri of a smart playlist: `tonearm:smart:<id>`. Never sent to Spotify.
pub const PREFIX: &str = "tonearm:smart:";
const FILE: &str = "smart.json";
const DAY: u64 = 24 * 3600;

#[must_use]
pub fn uri(id: &str) -> String {
    format!("{PREFIX}{id}")
}

#[must_use]
pub fn id_of(uri: &str) -> Option<&str> {
    uri.strip_prefix(PREFIX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Liked Songs (loaded from Spotify).
    #[default]
    Liked,
    /// Everything played in Tonearm.
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Played {
    #[default]
    Any,
    /// Never played in Tonearm.
    Never,
    /// Played before, but not in the last `days`.
    NotFor,
    /// Played in the last `days` (plays are counted in that window).
    Within,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    MostPlayed,
    /// Longest since the last play first.
    Forgotten,
    RecentlyPlayed,
    /// Shuffled, the same order all day.
    Random,
    /// As the source lists them (Liked: newest first).
    Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub source: Source,
    pub played: Played,
    pub days: u32,
    /// At least this many plays (in the window for `Within`, ever otherwise).
    pub min_plays: u32,
    pub order: Order,
    /// 0 = all.
    pub limit: u32,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "Новый умный плейлист".into(),
            source: Source::Liked,
            played: Played::NotFor,
            days: 30,
            min_plays: 0,
            order: Order::Forgotten,
            limit: 100,
        }
    }
}

impl Rule {
    /// One line under the title: what the rule picks.
    #[must_use]
    pub fn describe(&self) -> String {
        let src = match self.source {
            Source::Liked => "Любимые",
            Source::History => "Всё прослушанное",
        };
        let mut parts = vec![src.to_owned()];
        match self.played {
            Played::Any => {}
            Played::Never => parts.push("ни разу не слушал".into()),
            Played::NotFor => parts.push(format!("не слушал {}", days_label(self.days))),
            Played::Within => parts.push(format!("слушал за {}", days_label(self.days))),
        }
        if self.min_plays > 1 && self.played != Played::Never {
            parts.push(format!("от {} раз", self.min_plays));
        }
        parts.push(
            match self.order {
                Order::MostPlayed => "сначала частые",
                Order::Forgotten => "сначала давние",
                Order::RecentlyPlayed => "сначала недавние",
                Order::Random => "вперемешку",
                Order::Source => "по порядку",
            }
            .into(),
        );
        if self.limit > 0 {
            parts.push(format!("до {}", self.limit));
        }
        parts.join(" · ")
    }

    #[must_use]
    pub fn needs_liked(&self) -> bool {
        self.source == Source::Liked
    }
}

#[must_use]
pub fn days_label(days: u32) -> String {
    match days {
        7 => "неделю".into(),
        30 => "месяц".into(),
        90 => "3 месяца".into(),
        180 => "полгода".into(),
        365 => "год".into(),
        d => format!("{d} дн."),
    }
}

/// The ones a fresh install starts with.
#[must_use]
pub fn presets() -> Vec<Rule> {
    vec![
        Rule {
            id: "forgotten".into(),
            name: "Забытые любимые".into(),
            source: Source::Liked,
            played: Played::NotFor,
            days: 30,
            min_plays: 0,
            order: Order::Forgotten,
            limit: 100,
        },
        Rule {
            id: "month-top".into(),
            name: "Чаще всего за месяц".into(),
            source: Source::History,
            played: Played::Within,
            days: 30,
            min_plays: 2,
            order: Order::MostPlayed,
            limit: 50,
        },
        Rule {
            id: "never".into(),
            name: "Любимые, но не слушал".into(),
            source: Source::Liked,
            played: Played::Never,
            days: 30,
            min_plays: 0,
            order: Order::Source,
            limit: 0,
        },
        Rule {
            id: "on-repeat".into(),
            name: "На повторе".into(),
            source: Source::History,
            played: Played::Within,
            days: 7,
            min_plays: 3,
            order: Order::MostPlayed,
            limit: 30,
        },
    ]
}

fn file() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "tonearm").map(|d| d.config_dir().join(FILE))
}

/// The saved rules; the presets on first start (or if the file is unreadable).
#[must_use]
pub fn load() -> Vec<Rule> {
    file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Rule>>(&b).ok())
        .unwrap_or_else(presets)
}

pub fn save(rules: &[Rule]) {
    let Some(p) = file() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(data) = serde_json::to_vec_pretty(rules) else {
        return;
    };
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// A short id that is not taken yet.
#[must_use]
pub fn new_id(rules: &[Rule], now: u64) -> String {
    let mut n = now;
    loop {
        let id = format!("r{n:x}");
        if rules.iter().all(|r| r.id != id) {
            return id;
        }
        n += 1;
    }
}

#[derive(Default)]
struct Seen {
    plays: u32,
    in_window: u32,
    last: u64,
    entry: Option<Entry>,
}

fn entry_track(e: &Entry) -> TrackItem {
    TrackItem {
        uri: e.uri.clone(),
        title: e.title.clone(),
        artists: e.artists.clone(),
        artist_uri: e.artist_uri.clone(),
        album: e.album.clone(),
        album_uri: e.album_uri.clone(),
        duration_ms: e.duration_ms,
        playable: true,
        image_url: e.image_url.clone(),
        ..TrackItem::default()
    }
}

/// A cheap stable shuffle key: changes once a day.
fn mix(uri: &str, seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for b in uri.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Applies `rule` to Liked Songs (`liked`, newest first) or the play log (`plays`, any order).
#[must_use]
pub fn evaluate(rule: &Rule, liked: &[TrackItem], plays: &[Entry], now: u64) -> Vec<TrackItem> {
    let since = now.saturating_sub(u64::from(rule.days) * DAY);
    let mut seen: HashMap<&str, Seen> = HashMap::new();
    for e in plays {
        let s = seen.entry(e.uri.as_str()).or_default();
        s.plays += 1;
        if e.at >= since {
            s.in_window += 1;
        }
        if e.at >= s.last {
            s.last = e.at;
            s.entry = Some(e.clone());
        }
    }
    let candidates: Vec<TrackItem> = match rule.source {
        Source::Liked => liked.to_vec(),
        Source::History => {
            let mut list: Vec<(&u64, TrackItem)> = seen
                .values()
                .filter_map(|s| s.entry.as_ref().map(|e| (&s.last, entry_track(e))))
                .collect();
            list.sort_by(|a, b| b.0.cmp(a.0));
            list.into_iter().map(|x| x.1).collect()
        }
    };
    let empty = Seen::default();
    let stat = |t: &TrackItem| seen.get(t.uri.as_str()).unwrap_or(&empty);
    let mut picked: Vec<(usize, TrackItem)> = candidates
        .into_iter()
        .enumerate()
        .filter(|(_, t)| {
            let s = stat(t);
            let counted = if rule.played == Played::Within {
                s.in_window
            } else {
                s.plays
            };
            let when = match rule.played {
                Played::Any => true,
                Played::Never => s.plays == 0,
                Played::NotFor => s.plays > 0 && s.last < since,
                Played::Within => s.in_window > 0,
            };
            when && (rule.played == Played::Never || counted >= rule.min_plays)
        })
        .collect();
    let count = |t: &TrackItem| {
        let s = stat(t);
        if rule.played == Played::Within {
            s.in_window
        } else {
            s.plays
        }
    };
    match rule.order {
        Order::MostPlayed => {
            picked.sort_by(|a, b| count(&b.1).cmp(&count(&a.1)).then(a.0.cmp(&b.0)));
        }
        Order::Forgotten => picked.sort_by(|a, b| {
            let (sa, sb) = (stat(&a.1), stat(&b.1));
            sa.last.cmp(&sb.last).then(a.0.cmp(&b.0))
        }),
        Order::RecentlyPlayed => picked.sort_by(|a, b| {
            let (sa, sb) = (stat(&a.1), stat(&b.1));
            sb.last.cmp(&sa.last).then(a.0.cmp(&b.0))
        }),
        Order::Random => {
            let seed = now / DAY;
            picked.sort_by_key(|(_, t)| mix(&t.uri, seed));
        }
        Order::Source => {}
    }
    if rule.limit > 0 {
        picked.truncate(rule.limit as usize);
    }
    picked
        .into_iter()
        .map(|(_, mut t)| {
            // Liked rows from Spotify are complete; fill gaps of history rows only.
            if let Some(e) = stat(&t).entry.as_ref() {
                if t.image_url.is_none() {
                    t.image_url.clone_from(&e.image_url);
                }
                if t.album_uri.is_none() {
                    t.album_uri.clone_from(&e.album_uri);
                }
            }
            t
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000 * DAY;

    fn liked(uri: &str) -> TrackItem {
        TrackItem {
            uri: uri.into(),
            title: uri.into(),
            playable: true,
            ..TrackItem::default()
        }
    }

    fn play(uri: &str, days_ago: u64) -> Entry {
        Entry {
            uri: uri.into(),
            title: uri.into(),
            listened_ms: 60_000,
            at: NOW - days_ago * DAY,
            ..Entry::default()
        }
    }

    fn uris(list: &[TrackItem]) -> Vec<&str> {
        list.iter().map(|t| t.uri.as_str()).collect()
    }

    fn rule(source: Source, played: Played, days: u32, min: u32, order: Order) -> Rule {
        Rule {
            source,
            played,
            days,
            min_plays: min,
            order,
            limit: 0,
            ..Rule::default()
        }
    }

    #[test]
    fn forgotten_liked_tracks() {
        let lib = [liked("a"), liked("b"), liked("c"), liked("d")];
        let plays = [play("a", 100), play("b", 40), play("b", 5), play("c", 60)];
        let r = rule(Source::Liked, Played::NotFor, 30, 0, Order::Forgotten);
        // b was played 5 days ago; d never: neither is "forgotten".
        assert_eq!(uris(&evaluate(&r, &lib, &plays, NOW)), ["a", "c"]);
    }

    #[test]
    fn never_played_keeps_the_liked_order() {
        let lib = [liked("a"), liked("b"), liked("c")];
        let plays = [play("b", 1)];
        let r = rule(Source::Liked, Played::Never, 30, 5, Order::Source);
        assert_eq!(uris(&evaluate(&r, &lib, &plays, NOW)), ["a", "c"]);
    }

    #[test]
    fn most_played_counts_only_the_window() {
        let plays = [
            play("x", 50),
            play("x", 50),
            play("x", 50),
            play("x", 2),
            play("y", 1),
            play("y", 3),
            play("z", 4),
        ];
        let r = rule(Source::History, Played::Within, 30, 2, Order::MostPlayed);
        assert_eq!(uris(&evaluate(&r, &[], &plays, NOW)), ["y"]);
        let r = rule(Source::History, Played::Within, 30, 1, Order::MostPlayed);
        let got = evaluate(&r, &[], &plays, NOW);
        assert_eq!(got[0].uri, "y");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn limit_and_random_are_stable() {
        let lib: Vec<TrackItem> = (0..20).map(|i| liked(&format!("t{i}"))).collect();
        let mut r = rule(Source::Liked, Played::Any, 30, 0, Order::Random);
        r.limit = 5;
        let a = evaluate(&r, &lib, &[], NOW);
        let b = evaluate(&r, &lib, &[], NOW + 60);
        assert_eq!(a.len(), 5);
        assert_eq!(uris(&a), uris(&b));
    }

    #[test]
    fn history_source_lists_recent_first_with_details() {
        let mut p = play("old", 10);
        p.image_url = Some("img".into());
        let plays = [p, play("new", 1)];
        let r = rule(Source::History, Played::Any, 30, 0, Order::Source);
        let got = evaluate(&r, &[], &plays, NOW);
        assert_eq!(uris(&got), ["new", "old"]);
        assert_eq!(got[1].image_url.as_deref(), Some("img"));
    }

    #[test]
    fn rules_round_trip_and_describe() {
        let rules = presets();
        let json = serde_json::to_string(&rules).unwrap();
        let back: Vec<Rule> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rules);
        assert!(rules[0].describe().contains("не слушал месяц"));
        assert_eq!(id_of(&uri("abc")), Some("abc"));
        assert!(new_id(&rules, 5).starts_with('r'));
    }
}
