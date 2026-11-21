//! Extras on top of the library: track radio, the details of a single track (lyrics live in
//! [`super::lyrics`]).
//!
//! All of them go through the librespot session (no Client ID).

use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Metadata, Track};

use super::internal;
use super::metadata::{fill_album_covers, track_item};
use super::model::TrackItem;

/// Tracks in a radio queue (the seed comes first).
const RADIO_LEN: usize = 50;

/// Full details of one track, for a player that reported only part of them.
pub async fn track_meta(session: &Session, uri: &str) -> Option<TrackItem> {
    let id = SpotifyUri::from_uri(uri).ok()?;
    let t = Track::get(session, &id).await.ok()?;
    let mut item = track_item(&t);
    item.uri = uri.to_owned();
    fill_album_covers(session, std::slice::from_mut(&mut item)).await;
    Some(item)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_tracks() {
        let j = serde_json::json!({"tracks": [{"uri": "spotify:track:a"}, {"uri": "spotify:episode:b"}]});
        assert_eq!(station_uris(&j), vec!["spotify:track:a".to_owned()]);
    }
}
