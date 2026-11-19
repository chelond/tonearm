//! UI-thread state: maps backend events onto Slint properties and models.
//!
//! Lives in a thread-local because Slint objects and `Rc` models are not `Send`; backend events
//! reach it through `slint::invoke_from_event_loop`.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tonearm_core::covers::Cover;
use tonearm_core::library::{
    CollectionKind, LibraryEvent, PageHeader, PageRequest, PlaylistSummary, SearchKind,
    SearchResults, Tile, TrackItem, format_duration, sleeve_code,
};
use tonearm_player::{PlaybackState, PlayerCommand, QueueItem, QueueSnapshot, TrackInfo};

use crate::{AppWindow, PlayView, PlaylistEntry, QueueRow, RowState, TileData, TrackData};

/// Requests from the UI state to the async side (covers, playback, navigation).
pub enum Effect {
    Cover {
        url: String,
        size: u32,
        target: CoverTarget,
    },
    Open(PageRequest),
    Player(PlayerCommand),
    Search {
        query: String,
        kind: Option<SearchKind>,
        offset: u32,
    },
    SetSaved {
        uri: String,
        saved: bool,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub enum CoverTarget {
    Home(String),
    Page(u64),
    Deck(String),
    Search(String),
}

#[derive(Default)]
pub struct UiState {
    pub page_id: u64,
    pub page_kind: Option<CollectionKind>,
    pub page_uri: String,
    pub page_title: String,
    pub byline_uri: Option<String>,
    pub tracks: Vec<TrackItem>,
    pub page_tiles: Vec<Tile>,
    pub home: Vec<Tile>,
    pub playlists: Vec<PlaylistSummary>,
    pub history: Vec<PageRequest>,
    pub current: Option<PageRequest>,
    pub playing_uri: String,
    pub track_rows: Rc<VecModel<TrackData>>,
    pub tile_rows: Rc<VecModel<TileData>>,
    pub home_rows: Rc<VecModel<TileData>>,
    pub playlist_rows: Rc<VecModel<PlaylistEntry>>,
    pub reopen_login: bool,
    pub search: SearchState,
    /// Name of what the queue plays, for the deck caption.
    pub context_name: String,
    /// Last queue state, to hand the queue over to a Connect device.
    pub queue: QueueSnapshot,
    pub devices: Vec<tonearm_core::connect::Device>,
}

/// What the search page shows.
#[derive(Default)]
pub struct SearchState {
    query: String,
    results: SearchResults,
    best: Option<Tile>,
    loading_more: bool,
    tracks: Rc<VecModel<TrackData>>,
    albums: Rc<VecModel<TileData>>,
    artists: Rc<VecModel<TileData>>,
    playlists: Rc<VecModel<TileData>>,
}

thread_local! {
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

pub fn init(w: &AppWindow) {
    let st = UiState::default();
    w.set_page_tracks(ModelRc::from(st.track_rows.clone()));
    w.set_page_tiles(ModelRc::from(st.tile_rows.clone()));
    w.set_home_tiles(ModelRc::from(st.home_rows.clone()));
    w.set_playlists(ModelRc::from(st.playlist_rows.clone()));
    w.set_search_tracks(ModelRc::from(st.search.tracks.clone()));
    w.set_search_albums(ModelRc::from(st.search.albums.clone()));
    w.set_search_artists(ModelRc::from(st.search.artists.clone()));
    w.set_search_playlists(ModelRc::from(st.search.playlists.clone()));
    STATE.with(|s| *s.borrow_mut() = Some(st));
}

/// Runs `f` with the UI state; returns the effects it produced.
pub fn with<R>(f: impl FnOnce(&mut UiState) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("ui state initialised")))
}

pub fn to_image(cover: &Cover, gray: bool) -> Image {
    let px = if gray { &cover.gray } else { &cover.color };
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(px, cover.width, cover.height);
    Image::from_rgba8(buf)
}

fn kind_code(kind: CollectionKind) -> i32 {
    match kind {
        CollectionKind::Liked => 0,
        CollectionKind::Playlist => 1,
        CollectionKind::Album => 2,
        CollectionKind::Artist => 3,
    }
}

pub fn request_from(kind: i32, uri: String) -> PageRequest {
    match kind {
        0 => PageRequest::Liked,
        2 => PageRequest::Album(uri),
        3 => PageRequest::Artist(uri),
        _ => PageRequest::Playlist(uri),
    }
}

fn tile_data(t: &Tile) -> TileData {
    TileData {
        uri: t.uri.clone().into(),
        kind: kind_code(t.kind),
        title: t.title.clone().into(),
        subtitle: t.subtitle.to_uppercase().into(),
        image: Image::default(),
        has_image: false,
    }
}

fn row_state(t: &TrackItem, playing: &str) -> RowState {
    if !t.playable {
        RowState::Unavailable
    } else if t.uri == playing {
        RowState::Playing
    } else {
        RowState::Normal
    }
}

impl UiState {
    fn track_row(&self, index: usize, t: &TrackItem) -> TrackData {
        let kind = self.page_kind.unwrap_or(CollectionKind::Playlist);
        TrackData {
            code: sleeve_code(kind, index, t).into(),
            title: t.title.clone().into(),
            artist: t.artists.clone().into(),
            duration: format_duration(u64::from(t.duration_ms)).into(),
            state: row_state(t, &self.playing_uri),
        }
    }

    /// Navigates to a page; the library loads it and answers with events.
    pub fn open(&mut self, w: &AppWindow, req: PageRequest, push_history: bool) -> Vec<Effect> {
        if push_history && let Some(cur) = self.current.take() {
            self.history.push(cur);
        }
        self.current = Some(req.clone());
        self.page_kind = None;
        self.tracks.clear();
        self.page_tiles.clear();
        self.track_rows.set_vec(Vec::new());
        self.tile_rows.set_vec(Vec::new());
        let (kind, uri) = match &req {
            PageRequest::Liked => (0, "liked".to_owned()),
            PageRequest::Playlist(u) => (1, u.clone()),
            PageRequest::Album(u) => (2, u.clone()),
            PageRequest::Artist(u) => (3, u.clone()),
        };
        self.page_uri = uri.clone();
        w.set_active_uri(uri.into());
        w.set_page_kind(kind);
        w.set_page_title("".into());
        w.set_page_byline("".into());
        w.set_page_year_label("".into());
        w.set_page_tracks_label("".into());
        w.set_page_length_label("".into());
        w.set_page_has_image(false);
        w.set_page_error("".into());
        w.set_page_loading(true);
        w.set_page(crate::Page::Collection);
        vec![Effect::Open(req)]
    }

    pub fn back(&mut self, w: &AppWindow) -> Vec<Effect> {
        match self.history.pop() {
            Some(prev) => self.open(w, prev, false),
            None => {
                w.set_page(crate::Page::Library);
                Vec::new()
            }
        }
    }

    /// Plays the page from row `index`: the whole page becomes the queue context.
    pub fn play_row(&self, index: usize) -> Vec<Effect> {
        context(&self.tracks, Some(index), &self.page_title, None)
    }

    pub fn play_page(&self, shuffle: bool) -> Vec<Effect> {
        context(&self.tracks, None, &self.page_title, Some(shuffle))
    }

    /// Right-click menu: 0 add to queue, 1 go to artist, 2 go to album.
    pub fn row_action(
        &mut self,
        w: &AppWindow,
        source: i32,
        index: usize,
        action: i32,
    ) -> Vec<Effect> {
        let list = if source == 1 {
            &self.search.results.tracks
        } else {
            &self.tracks
        };
        let Some(t) = list.get(index).cloned() else {
            return Vec::new();
        };
        match action {
            0 if t.playable => vec![Effect::Player(PlayerCommand::Enqueue(queue_item(&t)))],
            1 => t
                .artist_uri
                .map(|u| self.open(w, PageRequest::Artist(u), true))
                .unwrap_or_default(),
            2 => t
                .album_uri
                .map(|u| self.open(w, PageRequest::Album(u), true))
                .unwrap_or_default(),
            // On the «Любимые» page the same item removes the track.
            3 => vec![Effect::SetSaved {
                saved: !(source == 0 && self.current == Some(PageRequest::Liked)),
                uri: t.uri,
            }],
            _ => Vec::new(),
        }
    }

    pub fn on_queue(&mut self, w: &AppWindow, q: &QueueSnapshot) {
        let rows: Vec<QueueRow> = q
            .upcoming
            .iter()
            .enumerate()
            .map(|(i, item)| QueueRow {
                code: format!("{:02}", i + 1).into(),
                title: if item.title.is_empty() {
                    item.uri.clone().into()
                } else {
                    item.title.clone().into()
                },
                artist: item.artists.to_uppercase().into(),
                manual: i < q.manual,
            })
            .collect();
        w.set_queue_items(ModelRc::from(Rc::new(VecModel::from(rows))));
        w.set_queue_context(q.context_name.to_uppercase().into());
        w.set_shuffle_on(q.shuffle);
        w.set_repeat_mode(match q.repeat {
            tonearm_player::RepeatMode::Off => 0,
            tonearm_player::RepeatMode::All => 1,
            tonearm_player::RepeatMode::One => 2,
        });
        self.context_name.clone_from(&q.context_name);
        self.queue = q.clone();
    }

    /// URIs to continue on another device: the current track, then what is queued here.
    pub fn handoff(&self) -> (Vec<String>, Option<u32>) {
        if self.playing_uri.is_empty() {
            return (Vec::new(), None);
        }
        let uris = std::iter::once(self.playing_uri.clone())
            .chain(self.queue.upcoming.iter().map(|q| q.uri.clone()))
            .collect();
        (uris, None)
    }

    pub fn on_devices(&mut self, w: &AppWindow, list: Vec<tonearm_core::connect::Device>) {
        let rows: Vec<crate::DeviceRow> = list
            .iter()
            .map(|d| crate::DeviceRow {
                id: d.id.clone().into(),
                name: d.name.clone().into(),
                kind: d.kind.to_uppercase().into(),
                active: d.is_active,
            })
            .collect();
        w.set_devices(ModelRc::from(Rc::new(VecModel::from(rows))));
        self.devices = list;
    }

    pub fn device_name(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| "УСТРОЙСТВО".into(), |d| d.name.clone())
    }

    pub fn on_library(&mut self, w: &AppWindow, ev: LibraryEvent) -> Vec<Effect> {
        let mut fx = Vec::new();
        match ev {
            LibraryEvent::Playlists(lists) => {
                self.playlist_rows.set_vec(
                    lists
                        .iter()
                        .map(|p| PlaylistEntry {
                            uri: p.uri.clone().into(),
                            name: p.name.clone().into(),
                            count: i32::try_from(p.track_count).unwrap_or(i32::MAX),
                        })
                        .collect::<Vec<_>>(),
                );
                self.playlists = lists;
                if self.home.is_empty() {
                    fx.extend(self.set_home(w, self.playlist_tiles()));
                }
            }
            LibraryEvent::LikedCount(n) => w.set_liked_count(i32::try_from(n).unwrap_or(i32::MAX)),
            LibraryEvent::Home(tiles) if !tiles.is_empty() => fx.extend(self.set_home(w, tiles)),
            LibraryEvent::Home(_) => {}
            LibraryEvent::PageHeader { page_id, header } => {
                self.page_id = page_id;
                fx.extend(self.set_header(w, &header));
            }
            LibraryEvent::PageTracks {
                page_id,
                tracks,
                done,
            } if page_id == self.page_id => {
                let start = self.tracks.len();
                for (i, t) in tracks.iter().enumerate() {
                    self.track_rows.push(self.track_row(start + i, t));
                }
                self.tracks.extend(tracks);
                if done {
                    w.set_page_loading(false);
                    let total: u64 = self.tracks.iter().map(|t| u64::from(t.duration_ms)).sum();
                    w.set_page_length_label(format_duration(total).into());
                    w.set_page_tracks_label(self.tracks.len().to_string().into());
                }
            }
            LibraryEvent::PageTiles { page_id, tiles } if page_id == self.page_id => {
                self.tile_rows
                    .set_vec(tiles.iter().map(tile_data).collect::<Vec<_>>());
                self.page_tiles = tiles;
                w.set_page_loading(false);
            }
            LibraryEvent::PageError { page_id, message } if page_id >= self.page_id => {
                w.set_page_loading(false);
                w.set_page_error(message.to_uppercase().into());
            }
            LibraryEvent::Search {
                query,
                offset,
                results,
                ..
            } if query == self.search.query => fx.extend(self.on_search(w, offset, results)),
            LibraryEvent::SearchError { query, message } if query == self.search.query => {
                self.search.loading_more = false;
                w.set_search_loading_more(false);
                w.set_search_hint("".into());
                if self.search.results.is_empty() {
                    w.set_search_status(message.to_uppercase().into());
                    w.set_search_status_error(true);
                } else {
                    w.set_notice(message.into());
                    w.set_notice_is_error(true);
                }
            }
            LibraryEvent::Saved { uri, saved } => {
                let n = w.get_liked_count();
                w.set_liked_count(if saved {
                    n.saturating_add(1)
                } else {
                    (n - 1).max(0)
                });
                if !saved
                    && self.current == Some(PageRequest::Liked)
                    && let Some(i) = self.tracks.iter().position(|t| t.uri == uri)
                {
                    self.tracks.remove(i);
                    self.track_rows.remove(i);
                }
                w.set_notice(
                    if saved {
                        "Добавлено в «Любимые»"
                    } else {
                        "Убрано из «Любимых»"
                    }
                    .into(),
                );
                w.set_notice_is_error(false);
            }
            LibraryEvent::Error(m) => {
                w.set_notice(m.into());
                w.set_notice_is_error(true);
            }
            _ => {} // stale page events
        }
        fx
    }

    fn playlist_tiles(&self) -> Vec<Tile> {
        self.playlists
            .iter()
            .take(6)
            .map(|p| Tile {
                uri: p.uri.clone(),
                kind: CollectionKind::Playlist,
                title: p.name.clone(),
                subtitle: format!("{} треков", p.track_count),
                image_url: p.image_url.clone(),
            })
            .collect()
    }

    fn set_home(&mut self, _w: &AppWindow, tiles: Vec<Tile>) -> Vec<Effect> {
        self.home_rows
            .set_vec(tiles.iter().map(tile_data).collect::<Vec<_>>());
        let fx = tiles
            .iter()
            .filter_map(|t| {
                t.image_url.as_ref().map(|u| Effect::Cover {
                    url: u.clone(),
                    size: 320,
                    target: CoverTarget::Home(t.uri.clone()),
                })
            })
            .collect();
        self.home = tiles;
        fx
    }

    fn set_header(&mut self, w: &AppWindow, h: &PageHeader) -> Vec<Effect> {
        self.page_kind = Some(h.kind);
        self.page_title = h.title.clone();
        self.byline_uri = h.byline_uri.clone();
        w.set_page_kind(kind_code(h.kind));
        w.set_page_title(h.title.clone().into());
        w.set_page_byline(h.byline.clone().into());
        w.set_page_year_label(h.year.map(|y| y.to_string()).unwrap_or_default().into());
        if let Some(n) = h.total_tracks {
            w.set_page_tracks_label(n.to_string().into());
        }
        h.image_url
            .as_ref()
            .map(|u| {
                vec![Effect::Cover {
                    url: u.clone(),
                    size: 600,
                    target: CoverTarget::Page(self.page_id),
                }]
            })
            .unwrap_or_default()
    }

    pub fn on_cover(&mut self, w: &AppWindow, target: &CoverTarget, cover: &Cover) {
        match target {
            CoverTarget::Home(uri) => {
                for i in 0..self.home_rows.row_count_() {
                    if let Some(mut row) = self.home_rows.row(i)
                        && row.uri.as_str() == uri
                    {
                        row.image = to_image(cover, true);
                        row.has_image = true;
                        self.home_rows.set_row(i, row);
                    }
                }
            }
            CoverTarget::Page(id) if *id == self.page_id => {
                w.set_page_image(to_image(cover, true));
                w.set_page_has_image(true);
            }
            CoverTarget::Search(uri) => {
                for model in [
                    &self.search.albums,
                    &self.search.artists,
                    &self.search.playlists,
                ] {
                    for i in 0..model.row_count_() {
                        if let Some(mut row) = model.row(i)
                            && row.uri.as_str() == uri
                        {
                            row.image = to_image(cover, true);
                            row.has_image = true;
                            model.set_row(i, row);
                        }
                    }
                }
                let mut best = w.get_search_best();
                if best.uri.as_str() == uri {
                    best.image = to_image(cover, true);
                    best.has_image = true;
                    w.set_search_best(best);
                }
            }
            CoverTarget::Deck(uri) if *uri == self.playing_uri => {
                // The only coloured cover: what is playing now.
                w.set_track_image(to_image(cover, false));
                w.set_track_has_image(true);
            }
            _ => {}
        }
    }

    pub fn on_track(&mut self, w: &AppWindow, track: &TrackInfo) -> Vec<Effect> {
        let previous = std::mem::replace(&mut self.playing_uri, track.uri.clone());
        // Re-mark only rows whose state changes.
        for (i, t) in self.tracks.iter().enumerate() {
            if t.uri == previous || t.uri == track.uri {
                let row = self.track_row(i, t);
                self.track_rows.set_row(i, row);
            }
        }
        for (i, t) in self.search.results.tracks.iter().enumerate() {
            if t.uri == previous || t.uri == track.uri {
                self.search
                    .tracks
                    .set_row(i, search_row(i, t, &self.playing_uri));
            }
        }
        w.set_track_title(track.title.clone().into());
        w.set_track_artists(track.artists.clone().into());
        w.set_duration_ms(i32::try_from(track.duration_ms).unwrap_or(i32::MAX));
        let code = self
            .tracks
            .iter()
            .position(|t| t.uri == track.uri)
            .map(|i| {
                format!(
                    "{} · {}",
                    self.track_row(i, &self.tracks[i]).code,
                    self.page_title
                )
            });
        w.set_track_context(
            code.unwrap_or_else(|| track.album.clone())
                .to_uppercase()
                .into(),
        );
        w.set_track_has_image(false);
        track
            .cover_url
            .as_ref()
            .map(|u| {
                vec![Effect::Cover {
                    url: u.clone(),
                    size: 112,
                    target: CoverTarget::Deck(track.uri.clone()),
                }]
            })
            .unwrap_or_default()
    }
}

fn queue_item(t: &TrackItem) -> QueueItem {
    QueueItem {
        uri: t.uri.clone(),
        title: t.title.clone(),
        artists: t.artists.clone(),
        duration_ms: t.duration_ms,
    }
}

/// Playable tracks as a queue context starting at row `start` (first playable if `None`).
fn context(
    tracks: &[TrackItem],
    start: Option<usize>,
    name: &str,
    shuffle: Option<bool>,
) -> Vec<Effect> {
    if let Some(i) = start
        && !tracks.get(i).is_some_and(|t| t.playable)
    {
        return Vec::new();
    }
    let start_uri = start.map(|i| tracks[i].uri.as_str());
    let items: Vec<QueueItem> = tracks
        .iter()
        .filter(|t| t.playable)
        .map(queue_item)
        .collect();
    if items.is_empty() {
        return Vec::new();
    }
    let start = start_uri
        .and_then(|u| items.iter().position(|q| q.uri == u))
        .unwrap_or(0);
    vec![Effect::Player(PlayerCommand::PlayContext {
        items,
        start,
        name: name.to_owned(),
        shuffle,
    })]
}

/// Search track rows are numbered 01, 02… like the mockup.
fn search_row(index: usize, t: &TrackItem, playing: &str) -> TrackData {
    TrackData {
        code: format!("{:02}", index + 1).into(),
        title: t.title.clone().into(),
        artist: t.artists.clone().into(),
        duration: format_duration(u64::from(t.duration_ms)).into(),
        state: row_state(t, playing),
    }
}

fn tab_kind(tab: i32) -> Option<SearchKind> {
    usize::try_from(tab - 1)
        .ok()
        .and_then(|i| SearchKind::ALL.get(i).copied())
}

/// What a pasted link points at.
pub enum LinkTarget {
    Play(String),
    Open(PageRequest),
}

pub fn link_target(text: &str) -> Option<LinkTarget> {
    let (kind, uri) = tonearm_player::uri::parse_link(text).ok()?;
    Some(match kind.as_str() {
        "track" | "episode" => LinkTarget::Play(uri),
        "album" => LinkTarget::Open(PageRequest::Album(uri)),
        "artist" => LinkTarget::Open(PageRequest::Artist(uri)),
        "playlist" => LinkTarget::Open(PageRequest::Playlist(uri)),
        _ => return None,
    })
}

impl UiState {
    /// The query changed (typing). Links are not searched: Enter opens them.
    pub fn search_edited(&mut self, w: &AppWindow, text: &str) -> Vec<Effect> {
        let query = text.trim().to_owned();
        if link_target(&query).is_some() {
            self.search.query.clear();
            w.set_search_hint("ENTER — ОТКРЫТЬ ССЫЛКУ".into());
            return Vec::new();
        }
        if query == self.search.query {
            return Vec::new();
        }
        self.search.query.clone_from(&query);
        self.search.loading_more = false;
        w.set_search_loading_more(false);
        if query.is_empty() {
            self.set_search_results(w, SearchResults::default());
            w.set_search_hint("".into());
            w.set_search_status("ВВЕДИТЕ ЗАПРОС".into());
            w.set_search_status_error(false);
            return Vec::new();
        }
        if self.search.results.is_empty() {
            w.set_search_status("ИЩЕМ…".into());
            w.set_search_status_error(false);
        }
        w.set_search_hint("● ИЩЕМ".into());
        vec![Effect::Search {
            query,
            kind: None,
            offset: 0,
        }]
    }

    /// Enter: open a pasted link.
    pub fn search_accepted(&mut self, w: &AppWindow, text: &str) -> Vec<Effect> {
        match link_target(text) {
            Some(LinkTarget::Play(uri)) => vec![Effect::Player(PlayerCommand::Load(uri))],
            Some(LinkTarget::Open(req)) => self.open(w, req, true),
            None => Vec::new(),
        }
    }

    pub fn search_tab_changed(&self, w: &AppWindow, tab: i32) {
        w.set_search_more(tab_kind(tab).is_some_and(|k| self.search.results.more[k.index()]));
    }

    pub fn search_load_more(&mut self, w: &AppWindow) -> Vec<Effect> {
        let Some(kind) = tab_kind(w.get_search_tab()) else {
            return Vec::new();
        };
        if self.search.loading_more || self.search.query.is_empty() {
            return Vec::new();
        }
        let r = &self.search.results;
        let loaded = match kind {
            SearchKind::Track => r.tracks.len(),
            SearchKind::Album => r.albums.len(),
            SearchKind::Artist => r.artists.len(),
            SearchKind::Playlist => r.playlists.len(),
        };
        self.search.loading_more = true;
        w.set_search_loading_more(true);
        vec![Effect::Search {
            query: self.search.query.clone(),
            kind: Some(kind),
            offset: u32::try_from(loaded).unwrap_or(u32::MAX),
        }]
    }

    pub fn search_play(&self, index: usize) -> Vec<Effect> {
        let name = format!("Поиск: {}", self.search.query);
        context(&self.search.results.tracks, Some(index), &name, None)
    }

    fn on_search(&mut self, w: &AppWindow, offset: u32, page: SearchResults) -> Vec<Effect> {
        w.set_search_hint("".into());
        self.search.loading_more = false;
        w.set_search_loading_more(false);
        let new_tiles: Vec<Tile> = page
            .albums
            .iter()
            .chain(&page.artists)
            .chain(&page.playlists)
            .cloned()
            .collect();
        let results = if offset == 0 {
            page
        } else {
            let mut all = std::mem::take(&mut self.search.results);
            all.extend(page);
            all
        };
        if results.is_empty() {
            w.set_search_status("НИЧЕГО НЕ НАЙДЕНО".into());
            w.set_search_status_error(false);
        } else {
            w.set_search_status("".into());
        }
        if offset == 0 {
            self.search.best = results.best(&self.search.query);
        }
        self.set_search_results(w, results);
        let mut fx: Vec<Effect> = new_tiles
            .iter()
            .chain(self.search.best.iter())
            .filter_map(|t| {
                t.image_url.as_ref().map(|u| Effect::Cover {
                    url: u.clone(),
                    size: 200,
                    target: CoverTarget::Search(t.uri.clone()),
                })
            })
            .collect();
        fx.dedup_by(|a, b| matches!((a, b), (Effect::Cover { url: x, .. }, Effect::Cover { url: y, .. }) if x == y));
        fx
    }

    fn set_search_results(&mut self, w: &AppWindow, r: SearchResults) {
        let s = &self.search;
        s.tracks.set_vec(
            r.tracks
                .iter()
                .enumerate()
                .map(|(i, t)| search_row(i, t, &self.playing_uri))
                .collect::<Vec<_>>(),
        );
        // Keep covers that are already loaded.
        let keep = |model: &VecModel<TileData>, tiles: &[Tile]| {
            let old: Vec<TileData> = (0..model.row_count_())
                .filter_map(|i| model.row(i))
                .collect();
            let rows: Vec<TileData> = tiles
                .iter()
                .map(|t| {
                    old.iter()
                        .find(|o| o.uri.as_str() == t.uri && o.has_image)
                        .cloned()
                        .unwrap_or_else(|| tile_data(t))
                })
                .collect();
            model.set_vec(rows);
        };
        keep(&s.albums, &r.albums);
        keep(&s.artists, &r.artists);
        keep(&s.playlists, &r.playlists);
        match &s.best {
            Some(b) => {
                let current = w.get_search_best();
                if current.uri.as_str() != b.uri {
                    w.set_search_best(tile_data(b));
                }
                w.set_search_has_best(true);
            }
            None => w.set_search_has_best(false),
        }
        let count = |n: usize, more: bool| {
            if n == 0 {
                String::new()
            } else {
                format!(" · {n}{}", if more { "+" } else { "" })
            }
        };
        let total = r.tracks.len() + r.albums.len() + r.artists.len() + r.playlists.len();
        let labels: Vec<slint::SharedString> = [
            format!("ВСЕ{}", count(total, r.more.iter().any(|m| *m))),
            format!("ТРЕКИ{}", count(r.tracks.len(), r.more[0])),
            format!("АЛЬБОМЫ{}", count(r.albums.len(), r.more[1])),
            format!("ИСПОЛНИТЕЛИ{}", count(r.artists.len(), r.more[2])),
            format!("ПЛЕЙЛИСТЫ{}", count(r.playlists.len(), r.more[3])),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        w.set_search_tab_labels(ModelRc::from(Rc::new(VecModel::from(labels))));
        w.set_search_more(tab_kind(w.get_search_tab()).is_some_and(|k| r.more[k.index()]));
        self.search.results = r;
    }
}

pub fn play_view(state: PlaybackState) -> PlayView {
    match state {
        PlaybackState::Stopped => PlayView::Stopped,
        PlaybackState::Loading => PlayView::Loading,
        PlaybackState::Playing => PlayView::Playing,
        PlaybackState::Paused => PlayView::Paused,
    }
}

/// Small helpers on `VecModel` without importing the `Model` trait everywhere.
trait VecModelExt<T> {
    fn row_count_(&self) -> usize;
    fn row(&self, i: usize) -> Option<T>;
    fn set_row(&self, i: usize, v: T);
}

impl<T: Clone + 'static> VecModelExt<T> for VecModel<T> {
    fn row_count_(&self) -> usize {
        slint::Model::row_count(self)
    }
    fn row(&self, i: usize) -> Option<T> {
        slint::Model::row_data(self, i)
    }
    fn set_row(&self, i: usize, v: T) {
        slint::Model::set_row_data(self, i, v);
    }
}
