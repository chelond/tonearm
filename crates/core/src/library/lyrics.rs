//! Song lyrics: Spotify's own (through the librespot session, no Client ID) and LRCLIB, a free
//! public lyrics database, asked at the same time, plus NetEase when the user wants words lit one
//! by one. The best wins: timed by word, then timed by line, then plain text.
//!
//! Two modes, picked in the lyrics overlay. Lines: official sources, lit line by line. Words:
//! word timings from the source (LRCLIB's enhanced LRC, NetEase YRC), and for lines without them
//! times spread at the song's own pace.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use librespot_core::{Session, SpotifyId, SpotifyUri};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderValue};

/// One word (with its trailing space) and when it is sung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricWord {
    pub start_ms: u32,
    pub end_ms: u32,
    pub text: String,
}

/// One lyrics line; `start_ms` is `None` for unsynced lyrics, `words` is empty for them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricLine {
    pub start_ms: Option<u32>,
    pub text: String,
    pub words: Vec<LyricWord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub synced: bool,
    /// The source timed every word (else word timings are estimated from the line).
    pub word_synced: bool,
    /// LRCLIB knows the track has no vocals; `lines` is empty.
    pub instrumental: bool,
    /// "Musixmatch", "LRCLIB"…
    pub provider: String,
}

impl Lyrics {
    fn rank(&self) -> u8 {
        match (self.instrumental, self.word_synced, self.synced) {
            (true, ..) => 0,
            (_, true, _) => 3,
            (_, _, true) => 2,
            _ => 1,
        }
    }
}

/// Recently fetched lyrics, so reopening the overlay or replaying a track costs nothing.
static CACHE: Mutex<VecDeque<(String, bool, Lyrics)>> = Mutex::new(VecDeque::new());
const CACHE_LEN: usize = 32;

/// Lyrics of a track; `None` when nobody has them.
///
/// `words = false`: official sources only (Spotify, LRCLIB), lit line by line.
/// `words = true`: NetEase too (unofficial API, word timings for many tracks), lit word by word;
/// lines nobody timed by word get times from the song's own pace.
pub async fn lyrics(
    session: &Session,
    uri: &str,
    title: &str,
    artist: &str,
    duration_ms: u32,
    words: bool,
) -> Option<Lyrics> {
    if let Some(hit) = CACHE.lock().ok().and_then(|c| {
        c.iter()
            .find(|(u, w, _)| u == uri && *w == words)
            .map(|(_, _, l)| l.clone())
    }) {
        return Some(hit);
    }
    let (spotify, lrclib, netease) = tokio::join!(
        spotify_lyrics(session, uri),
        lrclib(title, artist, duration_ms),
        async {
            if words {
                netease(title, artist, duration_ms).await
            } else {
                None
            }
        }
    );
    let name = |l: &Option<Lyrics>| l.as_ref().map_or("none", sync_name);
    tracing::info!(
        spotify = name(&spotify),
        lrclib = name(&lrclib),
        netease = name(&netease),
        words,
        "lyrics sources"
    );
    let mut best = pick(pick(spotify, lrclib), netease)?;
    if !words {
        for l in &mut best.lines {
            l.words.clear();
        }
        best.word_synced = false;
    }
    fill_timings(&mut best.lines, duration_ms, words);
    if let Ok(mut c) = CACHE.lock() {
        if c.len() >= CACHE_LEN {
            c.pop_front();
        }
        c.push_back((uri.to_owned(), words, best.clone()));
    }
    Some(best)
}

fn sync_name(l: &Lyrics) -> &'static str {
    match l.rank() {
        0 => "instrumental",
        1 => "plain",
        2 => "line",
        _ => "word",
    }
}

/// The better of the two; the first on a tie.
fn pick(first: Option<Lyrics>, second: Option<Lyrics>) -> Option<Lyrics> {
    match (first, second) {
        (Some(a), Some(b)) => Some(if b.rank() > a.rank() { b } else { a }),
        (a, b) => a.or(b),
    }
}

async fn spotify_lyrics(session: &Session, uri: &str) -> Option<Lyrics> {
    let SpotifyUri::Track { id } = SpotifyUri::from_uri(uri).ok()? else {
        return None;
    };
    let id: SpotifyId = id;
    let bytes = match session.spclient().get_lyrics(&id).await {
        Ok(b) => b,
        Err(e) => {
            // The web player asks with these; some accounts only get lyrics this way.
            tracing::debug!(error = %e, "spotify lyrics, plain request");
            let endpoint = format!(
                "/color-lyrics/v2/track/{}?format=json&vocalRemoval=false&market=from_token",
                id.to_base62().ok()?
            );
            let mut headers = HeaderMap::new();
            headers.insert("app-platform", HeaderValue::from_static("WebPlayer"));
            session
                .spclient()
                .request_as_json(&Method::GET, &endpoint, Some(headers), None)
                .await
                .map_err(|e| tracing::debug!(error = %e, "spotify lyrics, web request"))
                .ok()?
        }
    };
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    parse_spotify(&json)
}

fn parse_spotify(json: &serde_json::Value) -> Option<Lyrics> {
    let inner = &json["lyrics"];
    let sync = inner["syncType"].as_str().unwrap_or("");
    let synced = sync.ends_with("_SYNCED");
    if sync != "LINE_SYNCED" && synced {
        // Not seen yet; logged so its shape can be supported once it shows up.
        tracing::info!(
            sync,
            "spotify lyrics: unknown sync type, reading lines only"
        );
    }
    let lines: Vec<LyricLine> = inner["lines"]
        .as_array()?
        .iter()
        .map(|l| LyricLine {
            start_ms: synced
                .then(|| l["startTimeMs"].as_str()?.parse().ok())
                .flatten(),
            text: l["words"]
                .as_str()
                .unwrap_or("")
                .replace('♪', "")
                .trim()
                .to_owned(),
            words: Vec::new(),
        })
        .collect();
    (!lines.is_empty()).then(|| Lyrics {
        lines,
        synced,
        provider: inner["providerDisplayName"]
            .as_str()
            .unwrap_or("Spotify")
            .to_owned(),
        ..Lyrics::default()
    })
}

/// What one LRCLIB request gave.
enum Fetch {
    Hit(Lyrics),
    Miss,
    /// The server can't be reached: no point asking again for this track.
    Down,
}

async fn lrclib(title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    // One pool for the whole run: no new TLS handshake with LRCLIB for every track.
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .connect_timeout(Duration::from_secs(4))
                .user_agent(concat!(
                    "Tonearm/",
                    env!("CARGO_PKG_VERSION"),
                    " (https://github.com/chelond/tonearm)"
                ))
                .build()
                .ok()
        })
        .as_ref()?;
    let artist = artist.split(", ").next().unwrap_or(artist).trim();
    let mut titles = vec![title.trim().to_owned()];
    let clean = clean_title(title);
    if !clean.is_empty() && clean != titles[0] {
        titles.push(clean);
    }
    let duration = (duration_ms / 1000).to_string();
    let mut best: Option<Lyrics> = None;

    // 1. Exact match: needs the artist and the length (LRCLIB allows ±2 s).
    if duration_ms > 0 && !artist.is_empty() {
        for t in &titles {
            let q = [
                ("track_name", t.as_str()),
                ("artist_name", artist),
                ("duration", duration.as_str()),
            ];
            match lrclib_get(client, "get", &q, duration_ms).await {
                Fetch::Hit(l) => {
                    if keep(l, &mut best) {
                        return best;
                    }
                }
                Fetch::Miss => {}
                Fetch::Down => return best,
            }
        }
    }
    // 2. Search by title and artist, then 3. by free text: other spellings, a featured artist
    // listed first, a version with a slightly different length.
    let mut searches: Vec<Vec<(&str, String)>> = Vec::new();
    for t in &titles {
        let mut q = vec![("track_name", t.clone())];
        if !artist.is_empty() {
            q.push(("artist_name", artist.to_owned()));
        }
        searches.push(q);
    }
    let last = titles.last().map_or("", String::as_str);
    searches.push(vec![("q", format!("{last} {artist}").trim().to_owned())]);
    for q in searches {
        let q: Vec<(&str, &str)> = q.iter().map(|(k, v)| (*k, v.as_str())).collect();
        match lrclib_get(client, "search", &q, duration_ms).await {
            Fetch::Hit(l) => {
                if keep(l, &mut best) {
                    return best;
                }
            }
            Fetch::Miss => {}
            Fetch::Down => return best,
        }
    }
    best
}

/// NetEase Cloud Music: no key, word timings (YRC) for many tracks. An unofficial API: any
/// failure just means no lyrics from it.
async fn netease(title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::REFERER,
                HeaderValue::from_static("https://music.163.com/"),
            );
            reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .connect_timeout(Duration::from_secs(4))
                .default_headers(headers)
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) Tonearm")
                .build()
                .ok()
        })
        .as_ref()?;
    let artist = artist.split(", ").next().unwrap_or(artist).trim();
    let clean = clean_title(title);
    let query = format!("{clean} {artist}");
    let found = client
        .get("https://music.163.com/api/search/get")
        .query(&[("s", query.trim()), ("type", "1"), ("limit", "10")])
        .send()
        .await
        .map_err(|e| tracing::info!(error = %e, "NetEase unreachable"))
        .ok()?;
    let found = crate::net::json_limited(found, crate::net::MAX_JSON).await?;
    let id = netease_match(&found, &clean, artist, duration_ms)?;
    let id = id.to_string();
    let json = client
        .get("https://music.163.com/api/song/lyric/v1")
        .query(&[
            ("id", id.as_str()),
            ("cp", "false"),
            ("lv", "0"),
            ("kv", "0"),
            ("tv", "0"),
            ("rv", "0"),
            ("yv", "0"),
            ("ytv", "0"),
            ("yrv", "0"),
        ])
        .send()
        .await
        .ok()?;
    let json = crate::net::json_limited(json, crate::net::MAX_JSON).await?;
    parse_netease(&json)
}

fn same(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// The search hit that is this track: same title, same artist, length within 3 s. Wrong lyrics
/// are worse than none, so anything less is no match.
fn netease_match(
    json: &serde_json::Value,
    title: &str,
    artist: &str,
    duration_ms: u32,
) -> Option<u64> {
    json["result"]["songs"]
        .as_array()?
        .iter()
        .filter(|s| {
            s["name"]
                .as_str()
                .is_some_and(|n| same(&clean_title(n), title))
        })
        .filter(|s| {
            artist.is_empty()
                || s["artists"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|a| a["name"].as_str().is_some_and(|n| same(n, artist)))
        })
        .filter_map(|s| {
            let off = s["duration"]
                .as_u64()
                .map(|d| d.abs_diff(u64::from(duration_ms)));
            let ok = duration_ms == 0 || off.is_some_and(|o| o <= 3000);
            ok.then(|| (off.unwrap_or(0), s["id"].as_u64()))
        })
        .min_by_key(|(off, _)| *off)
        .and_then(|(_, id)| id)
}

fn parse_netease(json: &serde_json::Value) -> Option<Lyrics> {
    let provider = "NetEase".to_owned();
    if let Some(yrc) = json["yrc"]["lyric"].as_str() {
        let lines = parse_yrc(yrc);
        if !lines.is_empty() {
            return Some(Lyrics {
                lines,
                synced: true,
                word_synced: true,
                provider,
                ..Lyrics::default()
            });
        }
    }
    let lrc = json["lrc"]["lyric"].as_str()?;
    let lines: Vec<LyricLine> = parse_lrc(lrc)
        .into_iter()
        .filter(|l| !is_credit(&l.text))
        .map(|mut l| {
            l.words.clear();
            l
        })
        .collect();
    (!lines.is_empty()).then(|| Lyrics {
        lines,
        synced: true,
        provider,
        ..Lyrics::default()
    })
}

/// "作词 : …", "Composer: …": credits NetEase puts at the top as lyrics lines.
fn is_credit(text: &str) -> bool {
    let Some((head, _)) = text.split_once([':', '：']) else {
        return false;
    };
    let head = head.trim();
    head.chars().count() <= 12
        && (head.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
            || ["lyrics", "composer", "producer", "arranger", "written by"]
                .iter()
                .any(|k| head.to_lowercase().contains(k)))
}

/// YRC: `[start,length](start,length,0)word(start,length,0)word…`, times in ms; lines that start
/// with `{` are credits in JSON.
fn parse_yrc(yrc: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw in yrc.lines() {
        let Some(body) = raw.trim().strip_prefix('[') else {
            continue;
        };
        let Some((head, mut rest)) = body.split_once(']') else {
            continue;
        };
        let Some(start) = head.split(',').next().and_then(|v| v.trim().parse().ok()) else {
            continue;
        };
        let mut words = Vec::new();
        while let Some(after) = rest.strip_prefix('(') {
            let Some(end) = after.find(')') else { break };
            let mut nums = after[..end].split(',').map(|v| v.trim().parse::<u32>());
            let (Some(Ok(ws)), Some(Ok(wl))) = (nums.next(), nums.next()) else {
                break;
            };
            let tail = &after[end + 1..];
            let next = tail.find('(').unwrap_or(tail.len());
            let piece = tail[..next].trim();
            if !piece.is_empty() {
                words.push(LyricWord {
                    start_ms: ws,
                    end_ms: ws + wl.max(1),
                    text: piece.to_owned(),
                });
            }
            rest = &tail[next..];
        }
        let text = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if words.is_empty() || is_credit(&text) {
            continue;
        }
        out.push(LyricLine {
            start_ms: Some(start),
            text,
            words,
        });
    }
    out.sort_by_key(|l| l.start_ms);
    out
}

/// Keeps `found` if it beats `best`; true once the lyrics are synced (good enough to stop).
fn keep(found: Lyrics, best: &mut Option<Lyrics>) -> bool {
    if best.as_ref().is_none_or(|b| found.rank() > b.rank()) {
        *best = Some(found);
    }
    best.as_ref().is_some_and(|b| b.rank() >= 2)
}

async fn lrclib_get(
    client: &reqwest::Client,
    path: &str,
    query: &[(&str, &str)],
    duration_ms: u32,
) -> Fetch {
    let resp = match client
        .get(format!("https://lrclib.net/api/{path}"))
        .query(query)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(error = %e, "LRCLIB unreachable");
            return Fetch::Down;
        }
    };
    if !resp.status().is_success() {
        return if resp.status().is_server_error() {
            Fetch::Down
        } else {
            Fetch::Miss
        };
    }
    let Some(json) = crate::net::json_limited(resp, crate::net::MAX_JSON).await else {
        return Fetch::Miss;
    };
    let found = if json.is_array() {
        best_search_hit(&json, duration_ms).and_then(parse_lrclib)
    } else {
        parse_lrclib(&json)
    };
    found.map_or(Fetch::Miss, Fetch::Hit)
}

/// The title without what streaming services add to it: " - Remastered 2011",
/// "(feat. X)", "[Live]"…
fn clean_title(title: &str) -> String {
    let mut t = title.split(" - ").next().unwrap_or(title).to_owned();
    for (open, close) in [('(', ')'), ('[', ']')] {
        while let Some(a) = t.find(open) {
            let Some(len) = t[a..].find(close) else { break };
            t.replace_range(a..=a + len, "");
        }
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The search hit to use: word timings, then line timings, then the closest length.
fn best_search_hit(json: &serde_json::Value, duration_ms: u32) -> Option<&serde_json::Value> {
    let want = f64::from(duration_ms) / 1000.0;
    json.as_array()?
        .iter()
        .filter(|h| {
            // A hit far off in length is another version (or another song).
            duration_ms == 0
                || h["duration"]
                    .as_f64()
                    .is_none_or(|d| (d - want).abs() <= 8.0)
        })
        .min_by_key(|h| {
            let synced = h["syncedLyrics"].as_str().filter(|s| !s.is_empty());
            let words = synced.is_some_and(|s| s.contains('<'));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let off = h["duration"]
                .as_f64()
                .map_or(999, |d| (d - want).abs() as u32);
            (synced.is_none(), !words, off)
        })
}

fn parse_lrclib(json: &serde_json::Value) -> Option<Lyrics> {
    if let Some(lrc) = json["syncedLyrics"].as_str().filter(|s| !s.is_empty()) {
        let lines = parse_lrc(lrc);
        if !lines.is_empty() {
            let word_synced = lines.iter().any(|l| !l.words.is_empty());
            return Some(Lyrics {
                lines,
                synced: true,
                word_synced,
                provider: "LRCLIB".into(),
                ..Lyrics::default()
            });
        }
    }
    if let Some(plain) = json["plainLyrics"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    {
        return Some(Lyrics {
            lines: plain
                .lines()
                .map(|l| LyricLine {
                    text: l.trim().to_owned(),
                    ..LyricLine::default()
                })
                .collect(),
            provider: "LRCLIB".into(),
            ..Lyrics::default()
        });
    }
    json["instrumental"]
        .as_bool()
        .filter(|i| *i)
        .map(|_| Lyrics {
            instrumental: true,
            provider: "LRCLIB".into(),
            ..Lyrics::default()
        })
}

/// `[mm:ss.xx] text` lines, with optional `<mm:ss.xx>` word stamps (enhanced LRC); several
/// line stamps repeat the line.
fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in lrc.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(body) = rest.strip_prefix('[') {
            let Some(end) = body.find(']') else { break };
            if let Some(ms) = lrc_time(&body[..end]) {
                stamps.push(ms);
            }
            rest = body[end + 1..].trim_start();
        }
        let (text, words) = parse_words(rest);
        for ms in stamps {
            out.push(LyricLine {
                start_ms: Some(ms),
                text: text.clone(),
                // Repeated lines carry word stamps for one occurrence only: keep them there.
                words: words
                    .iter()
                    .filter(|_| {
                        words
                            .first()
                            .is_none_or(|w| w.start_ms.abs_diff(ms) <= 2000)
                    })
                    .cloned()
                    .collect(),
            });
        }
    }
    out.sort_by_key(|l| l.start_ms);
    out
}

/// Splits `<00:01.00>Hello <00:01.50>world<00:02.10>` into the plain text and timed words.
/// A stamp with no text after it ends the word before it.
fn parse_words(body: &str) -> (String, Vec<LyricWord>) {
    if !body.contains('<') {
        return (body.trim().to_owned(), Vec::new());
    }
    let mut words: Vec<LyricWord> = Vec::new();
    let mut text = String::new();
    let mut rest = body;
    // Text before the first stamp belongs to no word; keep it in the line only.
    if let Some(i) = rest.find('<') {
        text.push_str(&rest[..i]);
        rest = &rest[i..];
    }
    while let Some(after) = rest.strip_prefix('<') {
        let Some(end) = after.find('>') else { break };
        let Some(ms) = lrc_time(&after[..end]) else {
            break;
        };
        let tail = &after[end + 1..];
        let next = tail.find('<').unwrap_or(tail.len());
        let piece = &tail[..next];
        if let Some(prev) = words.last_mut()
            && prev.end_ms == 0
        {
            prev.end_ms = ms;
        }
        if !piece.trim().is_empty() {
            words.push(LyricWord {
                start_ms: ms,
                end_ms: 0,
                text: piece.to_owned(),
            });
        }
        text.push_str(piece);
        rest = &tail[next..];
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.is_empty() {
        return (text, Vec::new());
    }
    (text, words)
}

fn lrc_time(s: &str) -> Option<u32> {
    let (m, sec) = s.trim().split_once(':')?;
    let m: u32 = m.parse().ok()?;
    let sec: f64 = sec.parse().ok()?;
    if !(0.0..60.5).contains(&sec) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (sec * 1000.0).round() as u32;
    Some(m * 60_000 + ms)
}

/// Closes open-ended word stamps (the last word of a line) and turns breaks between verses
/// (empty lines of 3 s or more) into "• • •", the dots lighting up one by one. Lines without
/// word stamps get times at the song's pace with `estimate`, else stay without (lit whole).
pub fn fill_timings(lines: &mut [LyricLine], duration_ms: u32, estimate: bool) {
    let starts: Vec<Option<u32>> = lines.iter().map(|l| l.start_ms).collect();
    let next_of = |i: usize, start: u32| {
        starts[i + 1..]
            .iter()
            .flatten()
            .copied()
            .find(|&s| s > start)
            .or((duration_ms > start).then_some(duration_ms))
            .unwrap_or(start + 6000)
    };
    let pace = estimate.then(|| song_pace(lines, &next_of));
    for (i, line) in lines.iter_mut().enumerate() {
        let Some(start) = line.start_ms else { continue };
        let next = next_of(i, start);
        let gap = next - start;
        if !line.words.is_empty() {
            let n = line.words.len();
            for k in 0..n {
                if line.words[k].end_ms <= line.words[k].start_ms {
                    let following = line.words.get(k + 1).map_or(next, |w| w.start_ms);
                    let w = &mut line.words[k];
                    w.end_ms = following.min(w.start_ms + 1500).max(w.start_ms + 1);
                }
            }
            continue;
        }
        if line.text.is_empty() {
            if gap >= 3000 {
                let end = next.saturating_sub(300).max(start + 3);
                let step = (end - start) / 3;
                for k in 0..3 {
                    line.words.push(LyricWord {
                        start_ms: start + step * k,
                        end_ms: if k == 2 { end } else { start + step * (k + 1) },
                        text: "•".into(),
                    });
                }
            }
            continue;
        }
        if let Some(pace) = pace {
            spread_words(line, start, gap, pace);
        }
    }
}

/// Milliseconds per character of this song: the median over its lines (gap to the next line
/// divided by length). A fast song gets a fast pace, so its last words are lit in time.
fn song_pace(lines: &[LyricLine], next_of: &impl Fn(usize, u32) -> u32) -> u32 {
    let mut rates: Vec<u32> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let start = l.start_ms?;
            let chars = weight(&l.text);
            let gap = next_of(i, start) - start;
            (chars >= 6 && gap < 10_000).then(|| gap / chars)
        })
        .collect();
    rates.sort_unstable();
    rates
        .get(rates.len() / 2)
        .copied()
        .unwrap_or(70)
        .clamp(25, 140)
}

fn weight(text: &str) -> u32 {
    text.split_whitespace()
        .map(|p| u32::try_from(p.chars().count()).unwrap_or(1) + 1)
        .sum()
}

/// Times for the words of a line nobody timed by word: by length, at the song's pace, always
/// done before the next line starts.
fn spread_words(line: &mut LyricLine, start: u32, gap: u32, pace: u32) {
    let pieces: Vec<&str> = line.text.split_whitespace().collect();
    let total = weight(&line.text).max(1);
    let sung = (total * pace).min(gap * 9 / 10).max(1);
    let mut at = 0u32;
    for p in pieces {
        let w = u32::try_from(p.chars().count()).unwrap_or(1) + 1;
        let s = start + sung * at / total;
        at += w;
        let e = start + sung * at / total;
        line.words.push(LyricWord {
            start_ms: s,
            end_ms: e.max(s + 1),
            text: p.to_owned(),
        });
    }
}

/// Index of the line playing at `position_ms` (synced lyrics), or `None` before the first one.
#[must_use]
pub fn current_line(lines: &[LyricLine], position_ms: u32) -> Option<usize> {
    let n = lines.partition_point(|l| l.start_ms.is_some_and(|s| s <= position_ms));
    n.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(l: &[LyricLine]) -> Vec<(Option<u32>, &str)> {
        l.iter().map(|l| (l.start_ms, l.text.as_str())).collect()
    }

    #[test]
    fn lrc_lines_are_parsed_and_sorted() {
        let l = parse_lrc("[00:12.50] Second\n[00:01.00][00:20.00] Chorus\nnoise\n[ar:x]");
        assert_eq!(
            lines(&l),
            vec![
                (Some(1000), "Chorus"),
                (Some(12500), "Second"),
                (Some(20000), "Chorus")
            ]
        );
        assert!(l.iter().all(|l| l.words.is_empty()));
    }

    #[test]
    fn enhanced_lrc_gives_word_timings() {
        let l = parse_lrc(
            "[00:01.00]<00:01.00>Hello <00:01.50>big <00:02.00>world<00:03.00>\n[00:04.00]<00:04.00>Bye",
        );
        assert_eq!(l[0].text, "Hello big world");
        let w: Vec<(u32, u32, &str)> = l[0]
            .words
            .iter()
            .map(|w| (w.start_ms, w.end_ms, w.text.trim()))
            .collect();
        assert_eq!(
            w,
            vec![
                (1000, 1500, "Hello"),
                (1500, 2000, "big"),
                (2000, 3000, "world")
            ]
        );
        let mut l = l;
        fill_timings(&mut l, 10_000, false);
        assert_eq!(l[1].words[0].end_ms, 5500, "open last word is capped");
        assert!(
            parse_lrclib(&serde_json::json!({"syncedLyrics": "[00:01.00]<00:01.00>a"}))
                .unwrap()
                .word_synced
        );
    }

    #[test]
    fn lines_without_stamps_get_no_words() {
        let mut l = parse_lrc("[00:10.00]one two three\n[00:30.00]\n[00:40.00]end");
        fill_timings(&mut l, 45_000, false);
        assert!(
            l[0].words.is_empty(),
            "line mode: word times are not guessed"
        );
        assert_eq!(l[1].words.len(), 3);
        assert_eq!(l[1].words[0].text, "•");
        assert_eq!(l[1].words[2].end_ms, 39_700);
        assert!(l[2].words.is_empty());
    }

    #[test]
    fn yrc_gives_words() {
        let yrc = "{\"t\":0,\"c\":[{\"tx\":\"作词: \"}]}\n[1000,2000](1000,500,0)Hello (1500,700,0)world\n[4000,500](4000,500,0)作词 : X";
        let l = parse_yrc(yrc);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].text, "Hello world");
        assert_eq!(l[0].words[1].start_ms, 1500);
        assert_eq!(l[0].words[1].end_ms, 2200);
        let j = serde_json::json!({"lrc": {"lyric": "[00:00.00] 作曲 : Y\n[00:01.00]Hi"}, "yrc": {"lyric": ""}});
        let l = parse_netease(&j).unwrap();
        assert_eq!(l.lines.len(), 1);
        assert!(!l.word_synced);
    }

    #[test]
    fn netease_match_is_strict() {
        let j = serde_json::json!({"result": {"songs": [
            {"id": 1, "name": "Blinding Lights", "artists": [{"name": "Someone Else"}], "duration": 200000},
            {"id": 2, "name": "Blinding Lights (Remix)", "artists": [{"name": "The Weeknd"}], "duration": 260000},
            {"id": 3, "name": "Blinding Lights", "artists": [{"name": "The Weeknd"}], "duration": 201000}
        ]}});
        assert_eq!(
            netease_match(&j, "Blinding Lights", "The Weeknd", 200_040),
            Some(3)
        );
        assert_eq!(netease_match(&j, "Other", "The Weeknd", 200_040), None);
    }

    #[test]
    fn estimated_words_follow_the_song_pace() {
        // A fast song: 2 s per line of ~30 characters.
        let mut l = parse_lrc(
            "[00:10.00]one two three four five six\n[00:12.00]seven eight nine ten eleven\n[00:14.00]twelve thirteen fourteen\n[00:16.00]end",
        );
        fill_timings(&mut l, 20_000, true);
        let last = l[0].words.last().unwrap();
        assert!(
            last.end_ms <= 12_000,
            "lit before the next line: {}",
            last.end_ms
        );
        assert!(
            last.end_ms >= 11_000,
            "spread over the line: {}",
            last.end_ms
        );
        assert!(l.iter().all(|x| !x.words.is_empty()));
    }

    #[test]
    fn current_line_follows_position() {
        let l = parse_lrc("[00:01.00]a\n[00:05.00]b\n[00:09.00]c");
        assert_eq!(current_line(&l, 0), None);
        assert_eq!(current_line(&l, 1000), Some(0));
        assert_eq!(current_line(&l, 6000), Some(1));
        assert_eq!(current_line(&l, 60_000), Some(2));
    }

    #[test]
    fn spotify_json_is_read_leniently() {
        let j = serde_json::json!({"lyrics": {"syncType": "LINE_SYNCED", "providerDisplayName": "Musixmatch",
            "lines": [{"startTimeMs": "1500", "words": "Hello"}, {"startTimeMs": "3000", "words": "♪"}]}});
        let l = parse_spotify(&j).unwrap();
        assert!(l.synced);
        assert_eq!(l.lines[0].start_ms, Some(1500));
        assert_eq!(l.lines[1].text, "");
        assert_eq!(l.provider, "Musixmatch");
    }

    #[test]
    fn lrclib_plain_and_instrumental() {
        let j = serde_json::json!({"syncedLyrics": null, "plainLyrics": "a\nb"});
        let l = parse_lrclib(&j).unwrap();
        assert!(!l.synced);
        assert_eq!(l.lines.len(), 2);
        let j =
            serde_json::json!({"instrumental": true, "syncedLyrics": null, "plainLyrics": null});
        assert!(parse_lrclib(&j).unwrap().instrumental);
    }

    #[test]
    fn better_source_wins() {
        let plain = Lyrics {
            lines: vec![LyricLine::default()],
            provider: "Spotify".into(),
            ..Lyrics::default()
        };
        let synced = Lyrics {
            synced: true,
            provider: "LRCLIB".into(),
            ..plain.clone()
        };
        let got = pick(Some(plain.clone()), Some(synced.clone())).unwrap();
        assert_eq!(got.provider, "LRCLIB");
        let got = pick(Some(synced.clone()), Some(synced)).unwrap();
        assert_eq!(got.provider, "LRCLIB", "tie: the first (Spotify) argument");
        assert_eq!(pick(None, Some(plain)).unwrap().provider, "Spotify");
    }

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(
            clean_title("Come Together - Remastered 2009"),
            "Come Together"
        );
        assert_eq!(clean_title("Song (feat. Someone) [Live]"), "Song");
        assert_eq!(clean_title("Plain"), "Plain");
    }

    #[test]
    fn search_prefers_synced_and_close_length() {
        let j = serde_json::json!([
            {"duration": 200.0, "syncedLyrics": null, "plainLyrics": "a"},
            {"duration": 260.0, "syncedLyrics": "[00:01.00]far"},
            {"duration": 203.0, "syncedLyrics": "[00:01.00]near"},
            {"duration": 205.0, "syncedLyrics": "[00:01.00]<00:01.00>words"}
        ]);
        let hit = best_search_hit(&j, 201_000).unwrap();
        assert_eq!(hit["duration"], 205.0);
        let any = best_search_hit(&j, 0).unwrap();
        assert!(any["syncedLyrics"].is_string());
    }
}
