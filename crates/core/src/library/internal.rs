//! Library data through the session's internal endpoints (spclient, metadata), the same ones the
//! official client uses. Their limits are per user, unlike the public Web API, whose quota for
//! the shared librespot client id is exhausted (429) and whose own-app mode allows five users.
//!
//! Undocumented endpoints: every parse is lenient, and unknown items are skipped, not fatal.

use std::collections::HashSet;

use futures_util::stream::{self, StreamExt};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Album, Metadata, Track};
use librespot_protocol::context::Context;
use librespot_protocol::context_page::ContextPage;
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::Message;
use reqwest::Method;

use super::metadata::{image_url, track_item};
use super::model::{CollectionKind, PageHeader, PlaylistSummary, SearchResults, Tile, TrackItem};

/// Parallel metadata requests (per-user limit; the official client does far more).
const CONCURRENCY: usize = 32;
/// Tracks per chunk sent to the UI while a long list loads.
pub const CHUNK: usize = 50;
const ROOTLIST_LEN: usize = 500;
const SEARCH_TRACKS: usize = 30;
const SEARCH_TILES: usize = 10;
/// Safety stop for paged contexts (Liked Songs of ~100k tracks).
const MAX_PAGES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InternalError(String);

fn fail(what: &str, e: impl std::fmt::Display) -> InternalError {
    InternalError(format!("{what}: {e}"))
}

/// Image id bytes of a playlist picture → CDN url.
fn picture_url(picture: &[u8]) -> Option<String> {
    if picture.is_empty() {
        return None;
    }
    let hex: String = picture.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("https://i.scdn.co/image/{hex}"))
}

/// Cover of a playlist: the CDN url Spotify lists for its own playlists (daylist, mixes),
/// else the uploaded picture id. Playlists with neither use a mosaic in the official client.
fn list_cover(attrs: &librespot_protocol::playlist4_external::ListAttributes) -> Option<String> {
    let rank = |name: &str| match name {
        "large" | "xlarge" => 0,
        "default" => 1,
        _ => 2,
    };
    attrs
        .picture_size
        .iter()
        .filter(|p| p.url().starts_with("https://"))
        .min_by_key(|p| rank(p.target_name()))
        .map(|p| p.url().to_owned())
        .or_else(|| picture_url(attrs.picture()))
}

/// Sidebar playlists (rootlist). Folders and other non-playlist entries are skipped.
pub async fn playlists(session: &Session) -> Result<Vec<PlaylistSummary>, InternalError> {
    let bytes = session
        .spclient()
        .get_rootlist(0, Some(ROOTLIST_LEN))
        .await
        .map_err(|e| fail("список плейлистов", e))?;
    let msg =
        SelectedListContent::parse_from_bytes(&bytes).map_err(|e| fail("список плейлистов", e))?;
    let contents = msg.contents.get_or_default();
    Ok(contents
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.uri().starts_with("spotify:playlist:"))
        .map(|(i, item)| {
            let meta = contents.meta_items.get(i);
            let attrs = meta.map(|m| m.attributes.get_or_default());
            PlaylistSummary {
                uri: item.uri().to_owned(),
                name: attrs.map(|a| a.name().to_owned()).unwrap_or_default(),
                owner: meta
                    .map(|m| m.owner_username().to_owned())
                    .unwrap_or_default(),
                track_count: meta.map_or(0, |m| u32::try_from(m.length()).unwrap_or(0)),
                image_url: attrs.and_then(list_cover),
            }
        })
        .collect())
}

/// Home grid: the first sidebar playlists (there is no "recently played" without the Web API).
#[must_use]
pub fn home_tiles(lists: &[PlaylistSummary], n: usize) -> Vec<Tile> {
    lists
        .iter()
        .take(n)
        .map(|p| Tile {
            uri: p.uri.clone(),
            kind: CollectionKind::Playlist,
            title: p.name.clone(),
            subtitle: p.owner.clone(),
            image_url: p.image_url.clone(),
        })
        .collect()
}

/// Playlist header and the uris of its tracks (episodes and local files are left out).
pub async fn playlist(
    session: &Session,
    uri: &str,
) -> Result<(PageHeader, Vec<String>), InternalError> {
    let id = SpotifyUri::from_uri(uri).map_err(|e| fail("плейлист", e))?;
    let SpotifyUri::Playlist { id, .. } = id else {
        return Err(InternalError("это не плейлист".into()));
    };
    let bytes = session
        .spclient()
        .get_playlist(&id)
        .await
        .map_err(|e| fail("плейлист", e))?;
    let msg = SelectedListContent::parse_from_bytes(&bytes).map_err(|e| fail("плейлист", e))?;
    let attrs = msg.attributes.get_or_default();
    let uris: Vec<String> = msg
        .contents
        .get_or_default()
        .items
        .iter()
        .map(|i| i.uri().to_owned())
        .filter(|u| u.starts_with("spotify:track:"))
        .collect();
    let header = PageHeader {
        kind: CollectionKind::Playlist,
        uri: uri.to_owned(),
        title: attrs.name().to_owned(),
        byline: msg.owner_username().to_owned(),
        byline_uri: None,
        year: None,
        total_tracks: Some(u32::try_from(uris.len()).unwrap_or(0)),
        image_url: list_cover(attrs),
    };
    Ok((header, uris))
}

/// Track uris of a context, following lazily loaded pages.
async fn context_track_uris(
    session: &Session,
    uri: &str,
    limit: usize,
) -> Result<Vec<String>, InternalError> {
    let ctx: Context = session
        .spclient()
        .get_context(uri)
        .await
        .map_err(|e| fail("контекст", e))?;
    let mut uris = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let take_page = |page: &ContextPage, uris: &mut Vec<String>, pending: &mut Vec<String>| {
        if page.tracks.is_empty() && !page.page_url().is_empty() {
            pending.push(page.page_url().to_owned());
        }
        uris.extend(
            page.tracks
                .iter()
                .map(|t| t.uri().to_owned())
                .filter(|u| u.starts_with("spotify:track:")),
        );
        if !page.next_page_url().is_empty() {
            pending.push(page.next_page_url().to_owned());
        }
    };
    for page in &ctx.pages {
        take_page(page, &mut uris, &mut pending);
    }
    let mut fetched = 0;
    while let Some(url) = (!pending.is_empty()).then(|| pending.remove(0)) {
        if uris.len() >= limit || fetched >= MAX_PAGES {
            break;
        }
        fetched += 1;
        let bytes = session
            .spclient()
            .get_next_page(&url)
            .await
            .map_err(|e| fail("страница контекста", e))?;
        let json = String::from_utf8_lossy(&bytes);
        match protobuf_json_mapping::parse_from_str::<ContextPage>(&json) {
            Ok(page) => take_page(&page, &mut uris, &mut pending),
            Err(e) => {
                tracing::warn!(error = %e, "context page not parsed");
                break;
            }
        }
    }
    uris.truncate(limit);
    Ok(uris)
}

/// Cover for a playlist without a picture of its own: a 2×2 mosaic of the first four different
/// album covers, as the official client shows it, or the single album cover of a short list.
pub async fn fallback_cover(session: &Session, playlist_uri: &str) -> Option<String> {
    let (_, uris) = playlist(session, playlist_uri).await.ok()?;
    mosaic_for(session, &uris).await
}

/// Mosaic (or single cover) from the albums of the first tracks in `uris`.
pub async fn mosaic_for(session: &Session, uris: &[String]) -> Option<String> {
    let ids: Vec<SpotifyUri> = uris
        .iter()
        .take(12)
        .filter_map(|u| SpotifyUri::from_uri(u).ok())
        .collect();
    let tracks = futures_util::future::join_all(ids.iter().map(|id| Track::get(session, id))).await;
    let mut covers: Vec<String> = Vec::new();
    for t in tracks.into_iter().flatten() {
        if let Some(url) = image_url(&t.album.covers)
            && !covers.contains(&url)
        {
            covers.push(url);
            if covers.len() == 4 {
                break;
            }
        }
    }
    mosaic_url(&covers)
}

/// Four `i.scdn.co/image/<id>` urls → one mosaic url; fewer → the first cover.
fn mosaic_url(covers: &[String]) -> Option<String> {
    if covers.len() < 4 {
        return covers.first().cloned();
    }
    let ids: Option<Vec<&str>> = covers
        .iter()
        .map(|u| u.strip_prefix("https://i.scdn.co/image/"))
        .collect();
    match ids {
        Some(ids) => Some(format!("https://mosaic.scdn.co/640/{}", ids.concat())),
        None => covers.first().cloned(),
    }
}

/// Uris of Liked Songs, newest first (the order the collection context returns).
pub async fn liked_uris(session: &Session) -> Result<Vec<String>, InternalError> {
    let uri = format!("spotify:user:{}:collection", session.username());
    context_track_uris(session, &uri, usize::MAX).await
}

/// Track metadata for `uris`, in order; tracks that fail to load are skipped.
pub async fn tracks(session: &Session, uris: &[String]) -> Vec<TrackItem> {
    stream::iter(uris.iter().cloned())
        .map(|uri| async move {
            let id = SpotifyUri::from_uri(&uri).ok()?;
            Track::get(session, &id).await.ok()
        })
        .buffered(CONCURRENCY)
        .filter_map(|t| async move { t.as_ref().map(track_item) })
        .collect()
        .await
}

/// Album header and its tracks.
pub async fn album(
    session: &Session,
    uri: &str,
) -> Result<(PageHeader, Vec<String>), InternalError> {
    let id = SpotifyUri::from_uri(uri).map_err(|e| fail("альбом", e))?;
    let a = Album::get(session, &id)
        .await
        .map_err(|e| fail("альбом", e))?;
    let uris: Vec<String> = a.tracks().filter_map(|t| t.to_uri().ok()).collect();
    let year = u32::try_from(a.date.0.year()).ok().filter(|y| *y > 0);
    let header = PageHeader {
        kind: CollectionKind::Album,
        uri: uri.to_owned(),
        title: a.name.clone(),
        byline: a
            .artists
            .iter()
            .map(|x| x.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        byline_uri: a.artists.first().and_then(|x| x.id.to_uri().ok()),
        year,
        total_tracks: Some(u32::try_from(uris.len()).unwrap_or(0)),
        image_url: image_url(&a.covers),
    };
    Ok((header, uris))
}

/// Search through the `spotify:search:` context: tracks, plus albums and artists taken from
/// them. Playlists are not available this way.
pub async fn search(session: &Session, query: &str) -> Result<SearchResults, InternalError> {
    let q: String = query
        .split_whitespace()
        .map(urlencode)
        .collect::<Vec<_>>()
        .join("+");
    let uris = context_track_uris(session, &format!("spotify:search:{q}"), SEARCH_TRACKS).await?;
    let found: Vec<Track> = stream::iter(uris)
        .map(|uri| async move {
            let id = SpotifyUri::from_uri(&uri).ok()?;
            Track::get(session, &id).await.ok()
        })
        .buffered(CONCURRENCY)
        .filter_map(|t| async move { t })
        .collect()
        .await;

    let mut seen = HashSet::new();
    let albums: Vec<Tile> = found
        .iter()
        .filter(|t| seen.insert(t.album.id.to_uri().unwrap_or_default()))
        .take(SEARCH_TILES)
        .map(|t| Tile {
            uri: t.album.id.to_uri().unwrap_or_default(),
            kind: CollectionKind::Album,
            title: t.album.name.clone(),
            subtitle: t
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            image_url: image_url(&t.album.covers),
        })
        .collect();
    let mut seen = HashSet::new();
    let artists: Vec<Tile> = found
        .iter()
        .flat_map(|t| t.artists.iter())
        .filter(|a| seen.insert(a.id.to_uri().unwrap_or_default()))
        .take(SEARCH_TILES)
        .map(|a| Tile {
            uri: a.id.to_uri().unwrap_or_default(),
            kind: CollectionKind::Artist,
            title: a.name.clone(),
            subtitle: "Артист".into(),
            image_url: None,
        })
        .collect();
    Ok(SearchResults {
        tracks: found.iter().map(track_item).collect(),
        albums,
        artists,
        playlists: Vec::new(),
        more: [false; 4],
    })
}

fn urlencode(word: &str) -> String {
    word.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Adds to or removes from the library ("collection" set) via collection v2.
pub async fn set_saved(session: &Session, uri: &str, saved: bool) -> Result<(), InternalError> {
    let added_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let body = write_request(&session.username(), uri, added_at, !saved);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/vnd.collection-v2.spotify.proto"),
    );
    session
        .spclient()
        .request(
            &Method::POST,
            "/collection/v2/write",
            Some(headers),
            Some(&body),
        )
        .await
        .map(|_| ())
        .map_err(|e| fail("медиатека", e))
}

/// Protobuf `WriteRequest { username = 1; set = 2; items = 3 { uri = 1; added_at = 2;
/// is_removed = 3 } }` (collection2v2.proto; not compiled into librespot-protocol).
fn write_request(username: &str, uri: &str, added_at: u64, removed: bool) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    fn bytes_field(out: &mut Vec<u8>, field: u64, data: &[u8]) {
        varint(out, (field << 3) | 2);
        varint(out, data.len() as u64);
        out.extend_from_slice(data);
    }
    let mut item = Vec::new();
    bytes_field(&mut item, 1, uri.as_bytes());
    varint(&mut item, 2 << 3);
    varint(&mut item, added_at & 0x7fff_ffff);
    if removed {
        varint(&mut item, 3 << 3);
        varint(&mut item, 1);
    }
    let mut out = Vec::new();
    bytes_field(&mut out, 1, username.as_bytes());
    bytes_field(&mut out, 2, b"collection");
    bytes_field(&mut out, 3, &item);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mosaic_needs_four_covers() {
        let c = |id: &str| format!("https://i.scdn.co/image/{id}");
        assert_eq!(mosaic_url(&[]), None);
        assert_eq!(mosaic_url(&[c("a"), c("b")]), Some(c("a")));
        assert_eq!(
            mosaic_url(&[c("a"), c("b"), c("c"), c("d")]).as_deref(),
            Some("https://mosaic.scdn.co/640/abcd")
        );
    }

    #[test]
    fn write_request_encodes_like_protoc() {
        let b = write_request("u", "spotify:track:x", 1, true);
        // username "u", set "collection", item{uri, added_at=1, is_removed=1}
        let mut expected = vec![0x0a, 1, b'u', 0x12, 10];
        expected.extend_from_slice(b"collection");
        expected.extend_from_slice(&[0x1a, 21, 0x0a, 15]);
        expected.extend_from_slice(b"spotify:track:x");
        expected.extend_from_slice(&[0x10, 1, 0x18, 1]);
        assert_eq!(b, expected);
    }

    #[test]
    fn urlencode_keeps_safe_and_escapes_rest() {
        assert_eq!(urlencode("abc-1"), "abc-1");
        assert_eq!(urlencode("ä&"), "%C3%A4%26");
    }

    #[test]
    fn picture_hex() {
        assert_eq!(
            picture_url(&[0xab, 0x01]).as_deref(),
            Some("https://i.scdn.co/image/ab01")
        );
        assert_eq!(picture_url(&[]), None);
    }
}
