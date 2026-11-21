//! Library service: sidebar playlists, home tiles, playlist / album / artist / Liked Songs pages, search.
//!
//! Runs next to the auth backend and gets the session through the same `watch` channel as the player.

pub mod extras;
mod internal;
mod metadata;
pub mod model;
pub mod webapi;
mod webtoken;

use std::time::Duration;

use futures_util::StreamExt;
use librespot_core::Session;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub use model::*;
use webapi::{ApiError, Http, HttpResponse, Method, TrackPage, WebApi};
pub use webtoken::{WEBAPI_REFRESH_KEY, forget_own_token, use_own_client_id};

const HOME_TILES: usize = 12;
/// Playlists without a picture that get a mosaic built at startup.
const MOSAIC_LIMIT: usize = 40;
/// Typing pause before a search request, so every keystroke does not hit the rate limit.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryCommand {
    /// Reload sidebar and home.
    Refresh,
    Open(PageRequest),
    /// Search `query`; `kind` limits it to one type (a tab), `offset` loads the next page.
    /// A new search cancels the previous one.
    Search {
        query: String,
        kind: Option<SearchKind>,
        offset: u32,
    },
    /// Add a track (or album, playlist…) to the library, or remove it.
    SetSaved {
        uri: String,
        saved: bool,
    },
    /// Full details (artists, length, cover) of a track the player reported only partly.
    TrackMeta(String),
    /// Similar tracks for "Радио по треку"; `title` names the queue.
    Radio {
        uri: String,
        title: String,
    },
    Lyrics {
        uri: String,
        title: String,
        artist: String,
        duration_ms: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryEvent {
    Playlists(Vec<PlaylistSummary>),
    LikedCount(u32),
    Home(Vec<Tile>),
    /// A page started loading; later events for other ids are stale.
    PageHeader {
        page_id: u64,
        header: PageHeader,
    },
    /// More tracks of the page; `done` on the last chunk.
    PageTracks {
        page_id: u64,
        tracks: Vec<TrackItem>,
        done: bool,
    },
    /// A picture for the page found after its header (mosaic of a playlist without one).
    PageImage {
        page_id: u64,
        url: String,
    },
    /// Discography on an artist page.
    PageTiles {
        page_id: u64,
        tiles: Vec<Tile>,
    },
    PageError {
        page_id: u64,
        message: String,
    },
    /// Results for `query`; `offset > 0` means "append to what is shown".
    Search {
        query: String,
        kind: Option<SearchKind>,
        offset: u32,
        results: SearchResults,
    },
    SearchError {
        query: String,
        message: String,
    },
    /// [`LibraryCommand::SetSaved`] went through.
    Saved {
        uri: String,
        saved: bool,
    },
    TrackMeta(TrackItem),
    Radio {
        title: String,
        tracks: Vec<TrackItem>,
    },
    /// `None`: no lyrics anywhere for this track.
    Lyrics {
        uri: String,
        lyrics: Option<extras::Lyrics>,
    },
    Error(String),
}

/// Web API over reqwest with the session's token (librespot `TokenProvider`).
#[derive(Clone)]
pub struct SessionHttp {
    client: reqwest::Client,
    session: Session,
}

impl SessionHttp {
    #[must_use]
    pub fn new(session: Session) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            session,
        }
    }
}

impl SessionHttp {
    async fn execute(&self, req: reqwest::RequestBuilder) -> Result<HttpResponse, ApiError> {
        let token = webtoken::access_token(&self.session).await?;
        let resp = req
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|e| ApiError::Network(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Network(e.without_url().to_string()))?
            .to_vec();
        Ok(HttpResponse {
            status,
            body,
            retry_after,
        })
    }
}

impl Http for SessionHttp {
    async fn get(&self, url: &str) -> Result<HttpResponse, ApiError> {
        self.execute(self.client.get(url)).await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<String>,
    ) -> Result<HttpResponse, ApiError> {
        let req = match method {
            Method::Put => self.client.put(url),
            Method::Post => self.client.post(url),
            Method::Delete => self.client.delete(url),
        };
        let req = match body {
            Some(b) => req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(b),
            // Spotify wants an explicit zero length on empty PUT/POST.
            None => req.header(reqwest::header::CONTENT_LENGTH, "0"),
        };
        self.execute(req).await
    }
}

struct Service {
    events: mpsc::Sender<LibraryEvent>,
    session: Option<Session>,
    page_id: u64,
    page_task: Option<JoinHandle<()>>,
    refresh_task: Option<JoinHandle<()>>,
    search_task: Option<JoinHandle<()>>,
}

pub async fn run(
    mut commands: mpsc::Receiver<LibraryCommand>,
    events: mpsc::Sender<LibraryEvent>,
    mut sessions: watch::Receiver<Option<Session>>,
) {
    let mut svc = Service {
        events,
        session: sessions.borrow_and_update().clone(),
        page_id: 0,
        page_task: None,
        refresh_task: None,
        search_task: None,
    };
    if svc.session.is_some() {
        svc.refresh();
    }
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None => break,
                Some(LibraryCommand::Refresh) => svc.refresh(),
                Some(LibraryCommand::Open(req)) => svc.open(req),
                Some(LibraryCommand::Search { query, kind, offset }) => svc.search(query, kind, offset),
                Some(LibraryCommand::SetSaved { uri, saved }) => svc.set_saved(uri, saved),
                Some(cmd) => svc.extra(cmd),
            },
            changed = sessions.changed() => {
                if changed.is_err() {
                    break;
                }
                let session = sessions.borrow_and_update().clone();
                let was_none = svc.session.is_none();
                svc.session = session;
                // Reload only after a fresh login, not on every reconnect.
                if was_none && svc.session.is_some() {
                    svc.refresh();
                }
            }
        }
    }
    for task in [
        svc.page_task.take(),
        svc.refresh_task.take(),
        svc.search_task.take(),
    ]
    .into_iter()
    .flatten()
    {
        task.abort();
    }
}

impl Service {
    fn refresh(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if let Some(t) = self.refresh_task.take() {
            t.abort();
        }
        let events = self.events.clone();
        if !webtoken::has_own_client_id() {
            self.refresh_task = Some(tokio::spawn(refresh_internal(session, events)));
            return;
        }
        self.refresh_task = Some(tokio::spawn(async move {
            let api = WebApi::new(SessionHttp::new(session));
            match api.my_playlists().await {
                Ok(lists) => {
                    let _ = events.send(LibraryEvent::Playlists(lists)).await;
                }
                Err(e) => {
                    let _ = events
                        .send(LibraryEvent::Error(format!(
                            "Плейлисты не загрузились: {e}"
                        )))
                        .await;
                }
            }
            if let Ok(page) = api.liked(None).await {
                let _ = events
                    .send(LibraryEvent::LikedCount(page.total.unwrap_or(0)))
                    .await;
            }
            // Recently played is optional: without it the home page shows playlists only.
            match api.recent_albums(HOME_TILES).await {
                Ok(tiles) => {
                    let _ = events.send(LibraryEvent::Home(tiles)).await;
                }
                Err(e) => tracing::info!(%e, "recently played unavailable"),
            }
        }));
    }

    fn open(&mut self, req: PageRequest) {
        if let Some(t) = self.page_task.take() {
            t.abort();
        }
        self.page_id += 1;
        let page_id = self.page_id;
        let events = self.events.clone();
        let Some(session) = self.session.clone() else {
            let _ = events.try_send(LibraryEvent::PageError {
                page_id,
                message: "Нет подключения к Spotify".into(),
            });
            return;
        };
        self.page_task = Some(tokio::spawn(async move {
            if let Err(message) = load_page(&session, page_id, req, &events).await {
                let _ = events
                    .send(LibraryEvent::PageError { page_id, message })
                    .await;
            }
        }));
    }
}

impl Service {
    /// Track cover, radio and lyrics: independent one-shot requests.
    fn extra(&mut self, cmd: LibraryCommand) {
        let events = self.events.clone();
        let Some(session) = self.session.clone() else {
            return;
        };
        let task = async move {
            let ev = match cmd {
                // Always answer, even empty: the UI holds lyrics and links until it hears back.
                LibraryCommand::TrackMeta(uri) => LibraryEvent::TrackMeta(
                    extras::track_meta(&session, &uri).await.unwrap_or_else(|| {
                        tracing::info!(%uri, "track details not available");
                        TrackItem {
                            uri,
                            ..TrackItem::default()
                        }
                    }),
                ),
                LibraryCommand::Radio { uri, title } => match extras::radio(&session, &uri).await {
                    Ok(tracks) => LibraryEvent::Radio { title, tracks },
                    Err(e) => LibraryEvent::Error(format!("Радио не запустилось: {e}")),
                },
                LibraryCommand::Lyrics {
                    uri,
                    title,
                    artist,
                    duration_ms,
                } => LibraryEvent::Lyrics {
                    lyrics: extras::lyrics(&session, &uri, &title, &artist, duration_ms).await,
                    uri,
                },
                _ => return None,
            };
            events.send(ev).await.ok()
        };
        tokio::spawn(task);
    }

    fn set_saved(&self, uri: String, saved: bool) {
        let events = self.events.clone();
        let Some(session) = self.session.clone() else {
            let _ = events.try_send(LibraryEvent::Error("Нет подключения к Spotify".into()));
            return;
        };
        tokio::spawn(async move {
            if !webtoken::has_own_client_id() {
                let ev = match internal::set_saved(&session, &uri, saved).await {
                    Ok(()) => LibraryEvent::Saved { uri, saved },
                    Err(e) => LibraryEvent::Error(format!("Не удалось изменить «Любимые»: {e}")),
                };
                let _ = events.send(ev).await;
                return;
            }
            let api = WebApi::new(SessionHttp::new(session));
            let ev = match api.set_saved(std::slice::from_ref(&uri), saved).await {
                Ok(()) => LibraryEvent::Saved { uri, saved },
                // The token comes from Spotify's own client id; it may not be granted write scopes.
                Err(ApiError::Forbidden(_)) => LibraryEvent::Error(
                    "Spotify не разрешил менять медиатеку из Tonearm. Сделайте это в другом клиенте".into(),
                ),
                Err(e) => LibraryEvent::Error(format!("Не удалось изменить «Любимые»: {e}")),
            };
            let _ = events.send(ev).await;
        });
    }

    fn search(&mut self, query: String, kind: Option<SearchKind>, offset: u32) {
        if let Some(t) = self.search_task.take() {
            t.abort();
        }
        let query = query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        let events = self.events.clone();
        let Some(session) = self.session.clone() else {
            let _ = events.try_send(LibraryEvent::SearchError {
                query,
                message: "Нет подключения к Spotify".into(),
            });
            return;
        };
        self.search_task = Some(tokio::spawn(async move {
            // Paging is an explicit click; only typing is debounced.
            if offset == 0 {
                tokio::time::sleep(SEARCH_DEBOUNCE).await;
            }
            let result = if webtoken::has_own_client_id() {
                let api = WebApi::new(SessionHttp::new(session));
                let kinds = kind.map_or(SearchKind::ALL.to_vec(), |k| vec![k]);
                api.search(&query, &kinds, offset)
                    .await
                    .map_err(|e| e.to_string())
            } else if offset > 0 {
                // The search context has no further pages here.
                Ok(SearchResults::default())
            } else {
                internal::search(&session, &query)
                    .await
                    .map_err(|e| e.to_string())
            };
            let ev = match result {
                Ok(results) => LibraryEvent::Search {
                    query,
                    kind,
                    offset,
                    results,
                },
                Err(e) => LibraryEvent::SearchError {
                    query,
                    message: format!("Поиск не удался: {e}"),
                },
            };
            let _ = events.send(ev).await;
        }));
    }
}

async fn send_tracks(
    events: &mpsc::Sender<LibraryEvent>,
    page_id: u64,
    page: &TrackPage,
) -> Result<(), String> {
    events
        .send(LibraryEvent::PageTracks {
            page_id,
            tracks: page.tracks.clone(),
            done: page.next.is_none(),
        })
        .await
        .map_err(|_| "UI закрыт".to_owned())
}

async fn load_page(
    session: &Session,
    page_id: u64,
    req: PageRequest,
    events: &mpsc::Sender<LibraryEvent>,
) -> Result<(), String> {
    if !webtoken::has_own_client_id() && !matches!(req, PageRequest::Artist(_)) {
        return load_page_internal(session, page_id, req, events).await;
    }
    let api = WebApi::new(SessionHttp::new(session.clone()));
    let header = |header: PageHeader| LibraryEvent::PageHeader { page_id, header };
    let err = |e: ApiError| e.to_string();
    match req {
        PageRequest::Liked => {
            let mut page = api.liked(None).await.map_err(err)?;
            let _ = events
                .send(header(PageHeader {
                    kind: CollectionKind::Liked,
                    uri: "liked".into(),
                    title: "Любимые треки".into(),
                    byline: session.username(),
                    byline_uri: None,
                    year: None,
                    total_tracks: page.total,
                    image_url: None,
                }))
                .await;
            send_tracks(events, page_id, &page).await?;
            while let Some(next) = page.next.take() {
                page = api.liked(Some(&next)).await.map_err(err)?;
                send_tracks(events, page_id, &page).await?;
            }
        }
        PageRequest::Playlist(uri) => {
            let id = uri_id(&uri).to_owned();
            let (h, mut page) = api.playlist(&id).await.map_err(err)?;
            let _ = events.send(header(h)).await;
            send_tracks(events, page_id, &page).await?;
            while let Some(next) = page.next.take() {
                page = api.playlist_items(&id, Some(&next)).await.map_err(err)?;
                send_tracks(events, page_id, &page).await?;
            }
        }
        PageRequest::Album(uri) => {
            let (h, mut page) = api.album(uri_id(&uri)).await.map_err(err)?;
            let (name, album_uri) = (h.title.clone(), h.uri.clone());
            let _ = events.send(header(h)).await;
            send_tracks(events, page_id, &page).await?;
            while let Some(next) = page.next.take() {
                page = api
                    .album_tracks(&name, &album_uri, &next)
                    .await
                    .map_err(err)?;
                send_tracks(events, page_id, &page).await?;
            }
        }
        PageRequest::Artist(uri) => {
            let a = metadata::artist(session, &uri)
                .await
                .map_err(|e| e.to_string())?;
            let _ = events.send(header(a.header)).await;
            let _ = events
                .send(LibraryEvent::PageTracks {
                    page_id,
                    tracks: a.top_tracks,
                    done: true,
                })
                .await;
            let _ = events
                .send(LibraryEvent::PageTiles {
                    page_id,
                    tiles: a.albums,
                })
                .await;
        }
    }
    Ok(())
}

/// Sidebar, Liked count and home through the internal endpoints.
async fn refresh_internal(session: Session, events: mpsc::Sender<LibraryEvent>) {
    match internal::playlists(&session).await {
        Ok(lists) => {
            let mut lists = lists;
            let _ = events.send(LibraryEvent::Playlists(lists.clone())).await;
            let _ = events
                .send(LibraryEvent::Home(internal::home_tiles(&lists, HOME_TILES)))
                .await;
            // Playlists without a picture get a mosaic of their albums (sidebar and home), a few
            // at a time, then one more update with all of them.
            let missing: Vec<usize> = (0..lists.len())
                .filter(|&i| lists[i].image_url.is_none())
                .take(MOSAIC_LIMIT)
                .collect();
            let jobs: Vec<(usize, String)> = missing
                .into_iter()
                .map(|i| (i, lists[i].uri.clone()))
                .collect();
            let mut found = futures_util::stream::iter(jobs)
                .map(|(i, uri)| {
                    let session = session.clone();
                    async move { (i, internal::fallback_cover(&session, &uri).await) }
                })
                .buffer_unordered(6);
            // The sidebar fills in as mosaics arrive: every few of them, and once at the end.
            let mut fresh = 0;
            while let Some((i, url)) = found.next().await {
                if url.is_none() {
                    tracing::info!(uri = %lists[i].uri, "no mosaic for playlist");
                    continue;
                }
                lists[i].image_url = url;
                fresh += 1;
                if fresh == 6 {
                    fresh = 0;
                    let _ = events.send(LibraryEvent::Playlists(lists.clone())).await;
                    let _ = events
                        .send(LibraryEvent::Home(internal::home_tiles(&lists, HOME_TILES)))
                        .await;
                }
            }
            drop(found);
            if fresh > 0 {
                let _ = events.send(LibraryEvent::Playlists(lists.clone())).await;
                let _ = events
                    .send(LibraryEvent::Home(internal::home_tiles(&lists, HOME_TILES)))
                    .await;
            }
        }
        Err(e) => {
            let _ = events
                .send(LibraryEvent::Error(format!(
                    "Плейлисты не загрузились: {e}"
                )))
                .await;
        }
    }
    match internal::liked_uris(&session).await {
        Ok(uris) => {
            let n = u32::try_from(uris.len()).unwrap_or(u32::MAX);
            let _ = events.send(LibraryEvent::LikedCount(n)).await;
        }
        Err(e) => tracing::info!(%e, "liked songs count unavailable"),
    }
}

/// Loads track metadata in chunks so long lists appear progressively.
async fn send_track_uris(
    session: &Session,
    events: &mpsc::Sender<LibraryEvent>,
    page_id: u64,
    uris: &[String],
) -> Result<(), String> {
    if uris.is_empty() {
        let empty = TrackPage {
            tracks: Vec::new(),
            next: None,
            total: Some(0),
        };
        return send_tracks(events, page_id, &empty).await;
    }
    let chunks: Vec<&[String]> = uris.chunks(internal::CHUNK).collect();
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.into_iter().enumerate() {
        let tracks = internal::tracks(session, chunk).await;
        events
            .send(LibraryEvent::PageTracks {
                page_id,
                tracks,
                done: i == last,
            })
            .await
            .map_err(|_| "UI закрыт".to_owned())?;
    }
    Ok(())
}

async fn load_page_internal(
    session: &Session,
    page_id: u64,
    req: PageRequest,
    events: &mpsc::Sender<LibraryEvent>,
) -> Result<(), String> {
    let (header, uris) = match req {
        PageRequest::Liked => {
            let uris = internal::liked_uris(session)
                .await
                .map_err(|e| e.to_string())?;
            let header = PageHeader {
                kind: CollectionKind::Liked,
                uri: "liked".into(),
                title: "Любимые треки".into(),
                byline: session.username(),
                byline_uri: None,
                year: None,
                total_tracks: Some(u32::try_from(uris.len()).unwrap_or(u32::MAX)),
                image_url: None,
            };
            (header, uris)
        }
        PageRequest::Playlist(uri) => internal::playlist(session, &uri)
            .await
            .map_err(|e| e.to_string())?,
        PageRequest::Album(uri) => internal::album(session, &uri)
            .await
            .map_err(|e| e.to_string())?,
        PageRequest::Artist(_) => return Err("артист грузится через metadata".into()),
    };
    let needs_mosaic = header.image_url.is_none() && header.kind == CollectionKind::Playlist;
    events
        .send(LibraryEvent::PageHeader { page_id, header })
        .await
        .map_err(|_| "UI закрыт".to_owned())?;
    if needs_mosaic && !uris.is_empty() {
        let (session, events, uris) = (session.clone(), events.clone(), uris.clone());
        tokio::spawn(async move {
            if let Some(url) = internal::mosaic_for(&session, &uris).await {
                let _ = events.send(LibraryEvent::PageImage { page_id, url }).await;
            }
        });
    }
    send_track_uris(session, events, page_id, &uris).await
}
