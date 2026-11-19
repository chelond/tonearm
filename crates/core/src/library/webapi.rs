//! Minimal Spotify Web API client for the library.
//!
//! Parsing is lenient: unknown or missing fields default, and an item that fails to parse is
//! skipped instead of failing the whole page, so changes on Spotify's side degrade gracefully.
//! Endpoints follow the February 2026 renames (`/playlists/{id}/items`, `item` instead of `track`),
//! with the old names as fallback.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::model::{
    CollectionKind, PageHeader, PlaylistSummary, SearchKind, SearchResults, Tile, TrackItem,
};

pub const API: &str = "https://api.spotify.com/v1";
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    #[error("нет связи со Spotify: {0}")]
    Network(String),
    #[error("Spotify ограничил запросы, попробуйте позже")]
    RateLimited,
    #[error("нет доступа ({0})")]
    Forbidden(u16),
    #[error("не найдено")]
    NotFound,
    #[error("Spotify ответил ошибкой {0}")]
    Status(u16),
    #[error("не удалось разобрать ответ Spotify")]
    Parse,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Put,
    Post,
    Delete,
}

/// Authenticated HTTP. Abstracted so the client can be tested with canned JSON.
pub trait Http: Send + Sync + 'static {
    fn get(&self, url: &str) -> impl Future<Output = Result<HttpResponse, ApiError>> + Send;

    /// PUT/POST with an optional JSON body (player control). Read-only fakes need not implement it.
    fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<String>,
    ) -> impl Future<Output = Result<HttpResponse, ApiError>> + Send {
        let _ = (method, url, body);
        async { Err(ApiError::Status(405)) }
    }
}

pub struct WebApi<H> {
    pub(crate) http: H,
}

// ---- JSON shapes (only what we use) ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct Paging<T> {
    items: Vec<T>,
    next: Option<String>,
    total: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Image {
    url: String,
    width: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Named {
    name: String,
    uri: String,
    display_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Total {
    total: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiPlaylist {
    uri: String,
    name: String,
    owner: Named,
    images: Option<Vec<Image>>,
    #[serde(alias = "items")]
    tracks: Option<Total>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiAlbumRef {
    name: String,
    uri: String,
    images: Vec<Image>,
    artists: Vec<Named>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiTrack {
    uri: String,
    name: String,
    artists: Vec<Named>,
    album: Option<ApiAlbumRef>,
    duration_ms: u32,
    explicit: bool,
    is_playable: Option<bool>,
    is_local: bool,
    disc_number: u32,
    track_number: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wrapped {
    /// Playlist items use `item` (2026) or `track` (before); saved tracks use `track`.
    #[serde(alias = "track")]
    item: Option<ApiTrack>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiAlbum {
    uri: String,
    name: String,
    artists: Vec<Named>,
    images: Vec<Image>,
    release_date: String,
    total_tracks: Option<u32>,
    tracks: Paging<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiArtist {
    uri: String,
    name: String,
    images: Vec<Image>,
    genres: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiSearch {
    tracks: Option<Paging<serde_json::Value>>,
    albums: Option<Paging<serde_json::Value>>,
    artists: Option<Paging<serde_json::Value>>,
    playlists: Option<Paging<serde_json::Value>>,
}

/// Results per type and request: the Development Mode maximum since February 2026.
pub const SEARCH_LIMIT: u32 = 10;
/// Most URIs one `/me/library` request takes.
pub const LIBRARY_BATCH: usize = 40;

/// `/search` URL for a query; `kinds` become the comma-separated `type`.
#[must_use]
pub fn search_url(query: &str, kinds: &[SearchKind], offset: u32) -> String {
    let types = kinds
        .iter()
        .map(|k| k.api_name())
        .collect::<Vec<_>>()
        .join(",");
    let limit = SEARCH_LIMIT.to_string();
    let offset = offset.to_string();
    reqwest::Url::parse_with_params(
        &format!("{API}/search"),
        [
            ("q", query),
            ("type", types.as_str()),
            ("limit", limit.as_str()),
            ("offset", offset.as_str()),
        ],
    )
    .map(String::from)
    .unwrap_or_default()
}

/// Lenient list conversion: nulls and malformed entries are skipped.
fn items_of<T: DeserializeOwned>(page: Option<&Paging<serde_json::Value>>) -> Vec<T> {
    page.map(|p| {
        p.items
            .iter()
            .filter_map(|v| serde_json::from_value::<T>(v.clone()).ok())
            .collect()
    })
    .unwrap_or_default()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiPlaylistFull {
    uri: String,
    name: String,
    owner: Named,
    images: Option<Vec<Image>>,
    #[serde(alias = "items")]
    tracks: Paging<serde_json::Value>,
}

/// Picks the image closest to `want` px wide (Spotify lists them largest first).
fn pick_image(images: &[Image], want: u32) -> Option<String> {
    images
        .iter()
        .min_by_key(|i| i.width.unwrap_or(want).abs_diff(want))
        .map(|i| i.url.clone())
}

fn join_artists(artists: &[Named]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn convert_track(t: ApiTrack) -> Option<TrackItem> {
    if t.uri.is_empty() {
        return None;
    }
    let album = t.album.unwrap_or_default();
    Some(TrackItem {
        playable: !t.is_local
            && t.is_playable.unwrap_or(true)
            && t.uri.starts_with("spotify:track:"),
        artist_uri: t
            .artists
            .first()
            .map(|a| a.uri.clone())
            .filter(|u| !u.is_empty()),
        artists: join_artists(&t.artists),
        album_uri: Some(album.uri).filter(|u| !u.is_empty()),
        album: album.name,
        uri: t.uri,
        title: t.name,
        duration_ms: t.duration_ms,
        explicit: t.explicit,
        disc_number: t.disc_number,
        track_number: t.track_number,
    })
}

/// Lenient item conversion: one malformed entry does not drop the page.
fn tracks_from<T: DeserializeOwned>(
    items: Vec<serde_json::Value>,
    unwrap: impl Fn(T) -> Option<ApiTrack>,
) -> Vec<TrackItem> {
    items
        .into_iter()
        .filter_map(|v| serde_json::from_value::<T>(v).ok())
        .filter_map(unwrap)
        .filter_map(convert_track)
        .collect()
}

fn wrapped(w: Wrapped) -> Option<ApiTrack> {
    w.item
}

fn year_of(date: &str) -> Option<u32> {
    date.get(..4)?.parse().ok()
}

/// A page of tracks plus the URL of the next page.
pub struct TrackPage {
    pub tracks: Vec<TrackItem>,
    pub next: Option<String>,
    pub total: Option<u32>,
}

impl<H: Http> WebApi<H> {
    pub fn new(http: H) -> Self {
        Self { http }
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ApiError> {
        let mut attempt = 0;
        loop {
            let resp = self.http.get(url).await?;
            match resp.status {
                200..=299 => {
                    return serde_json::from_slice(&resp.body).map_err(|e| {
                        tracing::warn!(%url, error = %e, "unexpected Web API response");
                        ApiError::Parse
                    });
                }
                429 if attempt < MAX_RETRIES => {
                    attempt += 1;
                    let wait = resp
                        .retry_after
                        .unwrap_or(Duration::from_secs(2u64.pow(attempt)))
                        .min(MAX_RETRY_WAIT);
                    tracing::info!(?wait, "rate limited by Web API");
                    tokio::time::sleep(wait).await;
                }
                429 => return Err(ApiError::RateLimited),
                401 | 403 => return Err(ApiError::Forbidden(resp.status)),
                404 => return Err(ApiError::NotFound),
                s => return Err(ApiError::Status(s)),
            }
        }
    }

    /// All playlists of the user (followed and own).
    pub async fn my_playlists(&self) -> Result<Vec<PlaylistSummary>, ApiError> {
        let mut out = Vec::new();
        let mut url = Some(format!("{API}/me/playlists?limit=50"));
        while let Some(u) = url {
            let page: Paging<serde_json::Value> = self.get_json(&u).await?;
            out.extend(
                page.items
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<ApiPlaylist>(v).ok())
                    .filter(|p| !p.uri.is_empty())
                    .map(|p| PlaylistSummary {
                        image_url: pick_image(p.images.as_deref().unwrap_or_default(), 300),
                        owner: p.owner.display_name.unwrap_or(p.owner.name),
                        track_count: p.tracks.and_then(|t| t.total).unwrap_or(0),
                        uri: p.uri,
                        name: p.name,
                    }),
            );
            url = page.next;
        }
        Ok(out)
    }

    /// A page of Liked Songs. Pass `None` for the first page.
    pub async fn liked(&self, next: Option<&str>) -> Result<TrackPage, ApiError> {
        let url = next.map_or_else(|| format!("{API}/me/tracks?limit=50"), str::to_owned);
        let page: Paging<serde_json::Value> = self.get_json(&url).await?;
        Ok(TrackPage {
            tracks: tracks_from(page.items, wrapped),
            next: page.next,
            total: page.total,
        })
    }

    /// Playlist header plus its first page of tracks.
    pub async fn playlist(&self, id: &str) -> Result<(PageHeader, TrackPage), ApiError> {
        let p: ApiPlaylistFull = self.get_json(&format!("{API}/playlists/{id}")).await?;
        let header = PageHeader {
            kind: CollectionKind::Playlist,
            uri: p.uri,
            title: p.name,
            byline: p.owner.display_name.unwrap_or(p.owner.name),
            byline_uri: None,
            year: None,
            total_tracks: p.tracks.total,
            image_url: pick_image(p.images.as_deref().unwrap_or_default(), 640),
        };
        let mut first = TrackPage {
            tracks: tracks_from(p.tracks.items, wrapped),
            next: p.tracks.next,
            total: p.tracks.total,
        };
        // Some responses omit embedded items; fetch them explicitly.
        if first.tracks.is_empty() && first.total.unwrap_or(0) > 0 {
            first = self.playlist_items(id, None).await?;
        }
        Ok((header, first))
    }

    /// A page of playlist items: `/items` (2026), falling back to `/tracks`.
    pub async fn playlist_items(
        &self,
        id: &str,
        next: Option<&str>,
    ) -> Result<TrackPage, ApiError> {
        let fetch = |url: String| async move {
            let page: Paging<serde_json::Value> = self.get_json(&url).await?;
            Ok::<_, ApiError>(TrackPage {
                tracks: tracks_from(page.items, wrapped),
                next: page.next,
                total: page.total,
            })
        };
        if let Some(n) = next {
            return fetch(n.to_owned()).await;
        }
        match fetch(format!("{API}/playlists/{id}/items?limit=100")).await {
            Err(ApiError::NotFound) => {
                fetch(format!("{API}/playlists/{id}/tracks?limit=100")).await
            }
            other => other,
        }
    }

    /// Album header plus its first page of tracks.
    pub async fn album(&self, id: &str) -> Result<(PageHeader, TrackPage), ApiError> {
        let a: ApiAlbum = self.get_json(&format!("{API}/albums/{id}")).await?;
        let album_ref = || ApiAlbumRef {
            name: a.name.clone(),
            uri: a.uri.clone(),
            images: Vec::new(),
            artists: Vec::new(),
        };
        // Album track objects are "simplified": no album field, so attach it.
        let tracks = a
            .tracks
            .items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<ApiTrack>(v).ok())
            .filter_map(|mut t| {
                t.album = Some(album_ref());
                convert_track(t)
            })
            .collect();
        let header = PageHeader {
            kind: CollectionKind::Album,
            uri: a.uri.clone(),
            title: a.name.clone(),
            byline: join_artists(&a.artists),
            byline_uri: a.artists.first().map(|x| x.uri.clone()),
            year: year_of(&a.release_date),
            total_tracks: a.total_tracks.or(a.tracks.total),
            image_url: pick_image(&a.images, 640),
        };
        Ok((
            header,
            TrackPage {
                tracks,
                next: a.tracks.next,
                total: a.tracks.total,
            },
        ))
    }

    /// Next page of album tracks (`next` URL from [`WebApi::album`]).
    pub async fn album_tracks(
        &self,
        album: &str,
        album_uri: &str,
        next: &str,
    ) -> Result<TrackPage, ApiError> {
        let page: Paging<serde_json::Value> = self.get_json(next).await?;
        let tracks = page
            .items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<ApiTrack>(v).ok())
            .filter_map(|mut t| {
                t.album = Some(ApiAlbumRef {
                    name: album.to_owned(),
                    uri: album_uri.to_owned(),
                    ..ApiAlbumRef::default()
                });
                convert_track(t)
            })
            .collect();
        Ok(TrackPage {
            tracks,
            next: page.next,
            total: page.total,
        })
    }

    /// One page of search results. `kinds` limits the types (one tab), `offset` pages through them.
    /// `PUT` / `DELETE /me/library` (Feb 2026 replacement of `/me/tracks`; up to 40 URIs).
    pub async fn set_saved(&self, uris: &[String], saved: bool) -> Result<(), ApiError> {
        if uris.is_empty() {
            return Ok(());
        }
        let list = uris
            .iter()
            .take(LIBRARY_BATCH)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let url = reqwest::Url::parse_with_params(&format!("{API}/me/library"), [("uris", list)])
            .map_err(|_| ApiError::Parse)?;
        let method = if saved { Method::Put } else { Method::Delete };
        let resp = self.http.send(method, url.as_str(), None).await?;
        match resp.status {
            200..=299 => Ok(()),
            401 | 403 => Err(ApiError::Forbidden(resp.status)),
            404 => Err(ApiError::NotFound),
            429 => Err(ApiError::RateLimited),
            s => Err(ApiError::Status(s)),
        }
    }

    pub async fn search(
        &self,
        query: &str,
        kinds: &[SearchKind],
        offset: u32,
    ) -> Result<SearchResults, ApiError> {
        let r: ApiSearch = self.get_json(&search_url(query, kinds, offset)).await?;
        let more = |p: &Option<Paging<serde_json::Value>>| {
            p.as_ref().is_some_and(|p| {
                p.next.is_some() && p.total.is_none_or(|t| t > offset + SEARCH_LIMIT)
            })
        };
        let year = |date: &str| year_of(date).map(|y| y.to_string());
        Ok(SearchResults {
            more: [
                more(&r.tracks),
                more(&r.albums),
                more(&r.artists),
                more(&r.playlists),
            ],
            tracks: items_of::<ApiTrack>(r.tracks.as_ref())
                .into_iter()
                .filter_map(convert_track)
                .collect(),
            albums: items_of::<ApiAlbum>(r.albums.as_ref())
                .into_iter()
                .filter(|a| !a.uri.is_empty())
                .map(|a| Tile {
                    kind: CollectionKind::Album,
                    subtitle: [Some(join_artists(&a.artists)), year(&a.release_date)]
                        .into_iter()
                        .flatten()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                    image_url: pick_image(&a.images, 300),
                    uri: a.uri,
                    title: a.name,
                })
                .collect(),
            artists: items_of::<ApiArtist>(r.artists.as_ref())
                .into_iter()
                .filter(|a| !a.uri.is_empty())
                .map(|a| Tile {
                    kind: CollectionKind::Artist,
                    subtitle: a.genres.first().cloned().unwrap_or_default(),
                    image_url: pick_image(&a.images, 300),
                    uri: a.uri,
                    title: a.name,
                })
                .collect(),
            playlists: items_of::<ApiPlaylist>(r.playlists.as_ref())
                .into_iter()
                .filter(|p| !p.uri.is_empty())
                .map(|p| Tile {
                    kind: CollectionKind::Playlist,
                    image_url: pick_image(p.images.as_deref().unwrap_or_default(), 300),
                    subtitle: p.owner.display_name.unwrap_or(p.owner.name),
                    uri: p.uri,
                    title: p.name,
                })
                .collect(),
        })
    }

    /// Albums of recently played tracks, newest first, without duplicates.
    pub async fn recent_albums(&self, limit: usize) -> Result<Vec<Tile>, ApiError> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Played {
            track: Option<ApiTrack>,
        }
        let page: Paging<serde_json::Value> = self
            .get_json(&format!("{API}/me/player/recently-played?limit=50"))
            .await?;
        let mut seen = std::collections::HashSet::new();
        Ok(page
            .items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Played>(v).ok()?.track?.album)
            .filter(|a| !a.uri.is_empty() && seen.insert(a.uri.clone()))
            .take(limit)
            .map(|a| Tile {
                kind: CollectionKind::Album,
                subtitle: join_artists(&a.artists),
                image_url: pick_image(&a.images, 300),
                uri: a.uri,
                title: a.name,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Serves canned responses by URL; unknown URLs are 404.
    #[derive(Default)]
    struct Fake {
        routes: HashMap<String, (u16, String)>,
        hits: Mutex<Vec<String>>,
    }

    impl Fake {
        fn route(mut self, url: &str, status: u16, body: &str) -> Self {
            self.routes
                .insert(url.to_owned(), (status, body.to_owned()));
            self
        }
    }

    impl Http for Fake {
        async fn get(&self, url: &str) -> Result<HttpResponse, ApiError> {
            self.hits.lock().unwrap().push(url.to_owned());
            let (status, body) = self
                .routes
                .get(url)
                .cloned()
                .unwrap_or((404, String::new()));
            Ok(HttpResponse {
                status,
                body: body.into_bytes(),
                retry_after: Some(Duration::from_millis(1)),
            })
        }
    }

    const TRACK: &str = r#"{"uri":"spotify:track:1","name":"Огни","duration_ms":221000,"explicit":false,
        "artists":[{"name":"А","uri":"spotify:artist:a"},{"name":"Б","uri":"spotify:artist:b"}],
        "album":{"name":"Альбом","uri":"spotify:album:x","images":[]},"disc_number":1,"track_number":2}"#;

    #[tokio::test]
    async fn playlists_paginate_and_tolerate_both_field_names() {
        let page1 = format!(
            r#"{{"items":[{{"uri":"spotify:playlist:1","name":"P1","owner":{{"display_name":"Егор"}},"tracks":{{"total":5}}}}],
               "next":"{API}/me/playlists?offset=50"}}"#
        );
        let page2 = r#"{"items":[{"uri":"spotify:playlist:2","name":"P2","owner":{"display_name":"X"},"items":{"total":7}},
                         {"garbage":true}, 42],"next":null}"#;
        let api = WebApi::new(
            Fake::default()
                .route(&format!("{API}/me/playlists?limit=50"), 200, &page1)
                .route(&format!("{API}/me/playlists?offset=50"), 200, page2),
        );
        let lists = api.my_playlists().await.unwrap();
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[0].owner, "Егор");
        assert_eq!(lists[1].track_count, 7);
    }

    #[tokio::test]
    async fn liked_tracks_parse() {
        let body = format!(
            r#"{{"items":[{{"track":{TRACK}}},{{"track":null}}],"total":1284,"next":null}}"#
        );
        let api =
            WebApi::new(Fake::default().route(&format!("{API}/me/tracks?limit=50"), 200, &body));
        let page = api.liked(None).await.unwrap();
        assert_eq!(page.total, Some(1284));
        assert_eq!(page.tracks.len(), 1);
        let t = &page.tracks[0];
        assert_eq!(t.artists, "А, Б");
        assert_eq!(t.artist_uri.as_deref(), Some("spotify:artist:a"));
        assert!(t.playable);
    }

    #[tokio::test]
    async fn playlist_items_fall_back_to_old_endpoint_and_field() {
        let header = r#"{"uri":"spotify:playlist:p","name":"Ночные","owner":{"display_name":"Егор"},
                         "tracks":{"items":[],"total":1}}"#;
        let old = format!(r#"{{"items":[{{"track":{TRACK}}}],"total":1}}"#);
        let api = WebApi::new(
            Fake::default()
                .route(&format!("{API}/playlists/p"), 200, header)
                .route(&format!("{API}/playlists/p/tracks?limit=100"), 200, &old),
        );
        let (h, page) = api.playlist("p").await.unwrap();
        assert_eq!(h.title, "Ночные");
        assert_eq!(page.tracks.len(), 1);
    }

    #[tokio::test]
    async fn album_tracks_get_album_attached() {
        let body = r#"{"uri":"spotify:album:x","name":"Альбом","release_date":"2019-04-01","total_tracks":1,
            "artists":[{"name":"А","uri":"spotify:artist:a"}],"images":[{"url":"big","width":640},{"url":"small","width":64}],
            "tracks":{"items":[{"uri":"spotify:track:1","name":"T","duration_ms":1000,"disc_number":2,"track_number":3,"artists":[]}]}}"#;
        let api = WebApi::new(Fake::default().route(&format!("{API}/albums/x"), 200, body));
        let (h, page) = api.album("x").await.unwrap();
        assert_eq!(h.year, Some(2019));
        assert_eq!(h.image_url.as_deref(), Some("big"));
        assert_eq!(page.tracks[0].album_uri.as_deref(), Some("spotify:album:x"));
    }

    #[tokio::test]
    async fn rate_limit_retries_then_gives_up() {
        let api = WebApi::new(Fake::default().route(&format!("{API}/me/tracks?limit=50"), 429, ""));
        assert_eq!(api.liked(None).await.err(), Some(ApiError::RateLimited));
        assert_eq!(api.http.hits.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn search_parses_all_types_and_skips_nulls() {
        let url = search_url("ночь & день", &SearchKind::ALL, 0);
        assert!(url.contains("q=%D0%BD"), "{url}");
        assert!(url.contains("%26"), "{url}");
        assert!(
            url.contains("type=track%2Calbum%2Cartist%2Cplaylist")
                || url.contains("type=track,album,artist,playlist"),
            "{url}"
        );
        assert!(url.ends_with("limit=10&offset=0"), "{url}");
        let body = format!(
            r#"{{"tracks":{{"items":[{TRACK}],"next":"x","total":40}},
                "albums":{{"items":[{{"uri":"spotify:album:a","name":"Ночь","release_date":"2024-01-01",
                    "artists":[{{"name":"Мира Лес"}}],"images":[]}}],"next":null,"total":1}},
                "artists":{{"items":[{{"uri":"spotify:artist:z","name":"Night Ferry","genres":["indie"]}}],"total":1}},
                "playlists":{{"items":[null,{{"uri":"spotify:playlist:p","name":"Ночные","owner":{{"display_name":"Егор"}}}}],"next":"y","total":5}}}}"#
        );
        let api = WebApi::new(Fake::default().route(&url, 200, &body));
        let r = api
            .search("ночь & день", &SearchKind::ALL, 0)
            .await
            .unwrap();
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.albums[0].subtitle, "Мира Лес · 2024");
        assert_eq!(r.artists[0].subtitle, "indie");
        assert_eq!(r.playlists.len(), 1);
        assert_eq!(r.more, [true, false, false, false]);
    }

    #[tokio::test]
    async fn recent_albums_dedup() {
        let body = format!(r#"{{"items":[{{"track":{TRACK}}},{{"track":{TRACK}}}]}}"#);
        let api = WebApi::new(Fake::default().route(
            &format!("{API}/me/player/recently-played?limit=50"),
            200,
            &body,
        ));
        let tiles = api.recent_albums(6).await.unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].uri, "spotify:album:x");
    }
}
