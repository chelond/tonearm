//! Extras on top of the library: track radio, lyrics, the cover of a single track.
//!
//! All of them go through the librespot session (no Client ID). Lyrics fall back to LRCLIB, a
//! free public lyrics database, when Spotify has none or refuses the request.

use std::time::Duration;

use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::{Metadata, Track};

use super::internal;
use super::metadata::image_url_small;
use super::model::TrackItem;

/// Tracks in a radio queue (the seed comes first).
const RADIO_LEN: usize = 50;

/// One lyrics line; `start_ms` is `None` for unsynced lyrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub synced: bool,
    /// "Musixmatch", "LRCLIB"…
    pub provider: String,
}

/// Cover of one track (when the player did not report one).
pub async fn track_cover(session: &Session, uri: &str) -> Option<String> {
    let id = SpotifyUri::from_uri(uri).ok()?;
    let t = Track::get(session, &id).await.ok()?;
    image_url_small(&t.album.covers)
}

/// "Радио по треку": the seed, then similar tracks. Uses Spotify's "inspired by" mix and falls
/// back to the radio station endpoint.
pub async fn radio(session: &Session, seed: &str) -> Result<Vec<TrackItem>, String> {
    let id = SpotifyUri::from_uri(seed).map_err(|e| e.to_string())?;
    let mut uris = match inspired_by(session, &id).await {
        Some(u) if !u.is_empty() => u,
        _ => station(session, seed).await.unwrap_or_default(),
    };
    if uris.is_empty() {
        return Err("Spotify не дал радио для этого трека".into());
    }
    uris.retain(|u| u != seed);
    uris.insert(0, seed.to_owned());
    uris.truncate(RADIO_LEN);
    Ok(internal::tracks(session, &uris).await)
}

async fn inspired_by(session: &Session, id: &SpotifyUri) -> Option<Vec<String>> {
    let bytes = session.spclient().get_radio_for_track(id).await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let playlist = json["mediaItems"][0]["uri"].as_str()?;
    let (_, uris) = internal::playlist(session, playlist).await.ok()?;
    Some(uris)
}

async fn station(session: &Session, seed: &str) -> Option<Vec<String>> {
    let bytes = session
        .spclient()
        .get_apollo_station("stations", seed, Some(RADIO_LEN), Vec::new(), false)
        .await
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(station_uris(&json))
}

fn station_uris(json: &serde_json::Value) -> Vec<String> {
    json["tracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["uri"].as_str().or_else(|| t["original_gid"].as_str()))
        .filter(|u| u.starts_with("spotify:track:"))
        .map(str::to_owned)
        .collect()
}

/// Lyrics of a track: Spotify first, then LRCLIB by title, artist and duration.
pub async fn lyrics(
    session: &Session,
    uri: &str,
    title: &str,
    artist: &str,
    duration_ms: u32,
) -> Option<Lyrics> {
    if let Some(l) = spotify_lyrics(session, uri).await {
        return Some(l);
    }
    lrclib(title, artist, duration_ms).await
}

async fn spotify_lyrics(session: &Session, uri: &str) -> Option<Lyrics> {
    let SpotifyUri::Track { id } = SpotifyUri::from_uri(uri).ok()? else {
        return None;
    };
    let id: SpotifyId = id;
    let bytes = session.spclient().get_lyrics(&id).await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    parse_spotify(&json)
}

fn parse_spotify(json: &serde_json::Value) -> Option<Lyrics> {
    let inner = &json["lyrics"];
    let synced = inner["syncType"].as_str() == Some("LINE_SYNCED");
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
        })
        .collect();
    (!lines.is_empty()).then(|| Lyrics {
        lines,
        synced,
        provider: inner["providerDisplayName"]
            .as_str()
            .unwrap_or("Spotify")
            .to_owned(),
    })
}

async fn lrclib(title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("Tonearm/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let artist = artist.split(", ").next().unwrap_or(artist);
    let resp = client
        .get("https://lrclib.net/api/get")
        .query(&[
            ("track_name", title),
            ("artist_name", artist),
            ("duration", &(duration_ms / 1000).to_string()),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    parse_lrclib(&json)
}

fn parse_lrclib(json: &serde_json::Value) -> Option<Lyrics> {
    if let Some(lrc) = json["syncedLyrics"].as_str().filter(|s| !s.is_empty()) {
        let lines = parse_lrc(lrc);
        if !lines.is_empty() {
            return Some(Lyrics {
                lines,
                synced: true,
                provider: "LRCLIB".into(),
            });
        }
    }
    let plain = json["plainLyrics"].as_str().filter(|s| !s.is_empty())?;
    Some(Lyrics {
        lines: plain
            .lines()
            .map(|l| LyricLine {
                start_ms: None,
                text: l.trim().to_owned(),
            })
            .collect(),
        synced: false,
        provider: "LRCLIB".into(),
    })
}

/// `[mm:ss.xx] text` lines; several stamps on one line repeat it.
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
            rest = &body[end + 1..];
        }
        for ms in stamps {
            out.push(LyricLine {
                start_ms: Some(ms),
                text: rest.trim().to_owned(),
            });
        }
    }
    out.sort_by_key(|l| l.start_ms);
    out
}

fn lrc_time(s: &str) -> Option<u32> {
    let (m, sec) = s.split_once(':')?;
    let m: u32 = m.parse().ok()?;
    let sec: f64 = sec.parse().ok()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (sec * 1000.0).round() as u32;
    Some(m * 60_000 + ms)
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

    #[test]
    fn lrc_lines_are_parsed_and_sorted() {
        let l = parse_lrc("[00:12.50] Second\n[00:01.00][00:20.00] Chorus\nnoise\n[ar:x]");
        let got: Vec<(Option<u32>, &str)> =
            l.iter().map(|l| (l.start_ms, l.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (Some(1000), "Chorus"),
                (Some(12500), "Second"),
                (Some(20000), "Chorus")
            ]
        );
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
    fn lrclib_plain_fallback() {
        let j = serde_json::json!({"syncedLyrics": null, "plainLyrics": "a\nb"});
        let l = parse_lrclib(&j).unwrap();
        assert!(!l.synced);
        assert_eq!(l.lines.len(), 2);
    }

    #[test]
    fn station_tracks() {
        let j = serde_json::json!({"tracks": [{"uri": "spotify:track:a"}, {"uri": "spotify:episode:b"}]});
        assert_eq!(station_uris(&j), vec!["spotify:track:a".to_owned()]);
    }
}
