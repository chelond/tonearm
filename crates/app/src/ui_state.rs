//! UI-thread state: maps backend events onto Slint properties and models.
//!
//! Lives in a thread-local because Slint objects and `Rc` models are not `Send`; backend events
//! reach it through `slint::invoke_from_event_loop`.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{Color, Global, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tonearm_core::covers::Cover;
use tonearm_core::library::extras::{LyricLine, current_line};
use tonearm_core::library::{
    CollectionKind, LibraryCommand, LibraryEvent, PageHeader, PageRequest, PlaylistSummary,
    SearchKind, SearchResults, Tile, TrackItem, format_duration,
};
use tonearm_player::{PlaybackState, PlayerCommand, QueueItem, QueueSnapshot, TrackInfo};

use crate::history::{self, Context, Entry, QueuedTrack, Resume, Store};
use crate::themes;
use crate::{
    AppWindow, LyricRow, PaletteRow, PlayView, PlaylistEntry, QueueRow, RowState, StatRow, Theme,
    ThemeCard, TileData, TrackData,
};

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
    Library(LibraryCommand),
    /// Write the history and resume files (off the UI thread).
    Save(Box<Store>),
    /// Write only the resume point (cheap; every few seconds while playing).
    SaveResume(Box<Resume>),
}

#[derive(Clone, PartialEq, Eq)]
pub enum CoverTarget {
    Home(String),
    Recent(String),
    Page(u64),
    Deck(String),
    Search(String),
    SearchTrack(String),
    Playlist(String),
    History(String),
}

const DEFAULT_PAGE_ACCENT: Color = Color::from_rgb_u8(0x4A, 0x4A, 0x58);
const RECENT_TILES: usize = 8;
const HISTORY_ROWS: usize = 30;

/// Something the quick-jump palette can do.
#[derive(Clone)]
enum PaletteAction {
    Page(crate::Page),
    Open(PageRequest),
    Search(String),
    Radio,
    Lyrics,
    Shuffle,
}

#[derive(Default)]
pub struct UiState {
    pub page_id: u64,
    /// Events of pages older than this belong to a page we left (still in flight when we
    /// navigated): ignore them.
    min_page_id: u64,
    pub page_kind: Option<CollectionKind>,
    pub page_uri: String,
    pub page_title: String,
    page_image_url: Option<String>,
    pub byline_uri: Option<String>,
    pub tracks: Vec<TrackItem>,
    /// Rows shown (indices into `tracks`) after the filter and sort.
    view: Vec<usize>,
    filter: String,
    sort: i32,
    /// Play the page as soon as its tracks are in ("play" on a tile).
    autoplay: bool,
    pub page_tiles: Vec<Tile>,
    pub home: Vec<Tile>,
    pub playlists: Vec<PlaylistSummary>,
    pub history: Vec<PageRequest>,
    pub current: Option<PageRequest>,
    pub playing_uri: String,
    playing: TrackInfo,
    /// Playback time of the current track, for the history (counted by the 1 s clock).
    listened_ms: u32,
    started_at: u64,
    /// What the queue plays (for history, the sidebar marker and resume).
    context: Context,
    /// The context items as last started, for resume.
    context_items: Vec<QueueItem>,
    pub track_rows: Rc<VecModel<TrackData>>,
    pub tile_rows: Rc<VecModel<TileData>>,
    pub home_rows: Rc<VecModel<TileData>>,
    recent_rows: Rc<VecModel<TileData>>,
    pub playlist_rows: Rc<VecModel<PlaylistEntry>>,
    history_rows: Rc<VecModel<TrackData>>,
    stat_track_rows: Rc<VecModel<StatRow>>,
    history_items: Vec<Entry>,
    top_items: Vec<Entry>,
    pub reopen_login: bool,
    pub search: SearchState,
    /// Name of what the queue plays, for the deck caption.
    pub context_name: String,
    /// Last queue state, to hand the queue over to a Connect device.
    pub queue: QueueSnapshot,
    pub devices: Vec<tonearm_core::connect::Device>,
    store: Store,
    /// Last session's track, shown paused until play is pressed.
    resume: Option<Resume>,
    lyrics: Vec<LyricLine>,
    lyrics_uri: String,
    lyrics_index: Option<usize>,
    /// The player reported the track only partly; its details are on the way.
    meta_pending: bool,
    /// The theme's accent when it does not follow the cover.
    theme_accent: Option<Color>,
    theme_light: bool,
    /// Colour picked from the playing cover, kept for theme switches.
    cover_rgb: Option<[u8; 3]>,
    themes: Option<themes::Catalog>,
    /// When the active theme file last changed (custom themes reload on edit).
    theme_mtime: Option<std::time::SystemTime>,
    theme_check: u32,
    /// The playing track's cover already fell back to the metadata once.
    deck_retried: bool,
    /// Album and first artist of the playing track (the player bar links to them).
    playing_album: Option<String>,
    playing_artist: Option<String>,
    /// A click on the title (false) or artist (true) waits for the track's details.
    open_when_known: Option<bool>,
    palette: Vec<PaletteAction>,
    /// Seconds since the resume state was last written.
    since_save: u32,
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
    let st = UiState {
        store: if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
            Store::default()
        } else {
            Store::load()
        },
        ..UiState::default()
    };
    w.set_page_tracks(ModelRc::from(st.track_rows.clone()));
    w.set_page_tiles(ModelRc::from(st.tile_rows.clone()));
    w.set_home_tiles(ModelRc::from(st.home_rows.clone()));
    w.set_recent_tiles(ModelRc::from(st.recent_rows.clone()));
    w.set_playlists(ModelRc::from(st.playlist_rows.clone()));
    w.set_search_tracks(ModelRc::from(st.search.tracks.clone()));
    w.set_search_albums(ModelRc::from(st.search.albums.clone()));
    w.set_search_artists(ModelRc::from(st.search.artists.clone()));
    w.set_search_playlists(ModelRc::from(st.search.playlists.clone()));
    w.set_history_tracks(ModelRc::from(st.history_rows.clone()));
    w.set_stats_tracks(ModelRc::from(st.stat_track_rows.clone()));
    STATE.with(|s| *s.borrow_mut() = Some(st));
}

/// Runs `f` with the UI state; returns the effects it produced.
pub fn with<R>(f: impl FnOnce(&mut UiState) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("ui state initialised")))
}

pub fn to_image(cover: &Cover) -> Image {
    // Covers are opaque: RGB takes a quarter less memory than RGBA.
    if cover.color.chunks_exact(4).all(|p| p[3] == 255) {
        let rgb: Vec<u8> = cover
            .color
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let buf = SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(
            &rgb,
            cover.width,
            cover.height,
        );
        return Image::from_rgb8(buf);
    }
    let buf =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&cover.color, cover.width, cover.height);
    Image::from_rgba8(buf)
}

/// Blurred backdrop: a 12×12 average of the cover, smoothly enlarged to 96×96 here (the software
/// renderer scales up without filtering, so the smoothing has to be in the pixels). Costs 36 KB
/// instead of a real blur pass.
pub fn backdrop(cover: &Cover) -> Image {
    const N: usize = 12;
    const OUT: usize = 96;
    let (w, h) = (cover.width.max(1) as usize, cover.height.max(1) as usize);
    let mut cells = [[0f32; 3]; N * N];
    for (by, row) in cells.chunks_mut(N).enumerate() {
        for (bx, cell) in row.iter_mut().enumerate() {
            let (x0, x1) = (bx * w / N, ((bx + 1) * w / N).max(bx * w / N + 1).min(w));
            let (y0, y1) = (by * h / N, ((by + 1) * h / N).max(by * h / N + 1).min(h));
            let mut n = 0f32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    if let Some(p) = cover.color.get(i..i + 3) {
                        for c in 0..3 {
                            cell[c] += f32::from(p[c]);
                        }
                        n += 1.0;
                    }
                }
            }
            for v in cell.iter_mut() {
                *v /= n.max(1.0);
            }
        }
    }
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(OUT as u32, OUT as u32);
    let px = out.make_mut_slice();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    for y in 0..OUT {
        let fy = ((y as f32 + 0.5) * N as f32 / OUT as f32 - 0.5).clamp(0.0, (N - 1) as f32);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(N - 1);
        for x in 0..OUT {
            let fx = ((x as f32 + 0.5) * N as f32 / OUT as f32 - 0.5).clamp(0.0, (N - 1) as f32);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(N - 1);
            let mut rgb = [0u8; 3];
            for (c, out_c) in rgb.iter_mut().enumerate() {
                let top = cells[y0 * N + x0][c] * (1.0 - tx) + cells[y0 * N + x1][c] * tx;
                let bot = cells[y1 * N + x0][c] * (1.0 - tx) + cells[y1 * N + x1][c] * tx;
                *out_c = (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8;
            }
            px[y * OUT + x] = Rgba8Pixel {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
                a: 255,
            };
        }
    }
    Image::from_rgba8(out)
}

fn rgba(c: themes::Rgba) -> Color {
    Color::from_argb_u8(c[3], c[0], c[1], c[2])
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

fn request_uri(req: &PageRequest) -> (i32, String) {
    match req {
        PageRequest::Liked => (0, "liked".to_owned()),
        PageRequest::Playlist(u) => (1, u.clone()),
        PageRequest::Album(u) => (2, u.clone()),
        PageRequest::Artist(u) => (3, u.clone()),
    }
}

fn tile_data(t: &Tile) -> TileData {
    TileData {
        uri: t.uri.clone().into(),
        kind: kind_code(t.kind),
        title: t.title.clone().into(),
        subtitle: t.subtitle.clone().into(),
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

fn track_data(code: String, t: &TrackItem, playing: &str) -> TrackData {
    TrackData {
        code: code.into(),
        title: t.title.clone().into(),
        artist: t.artists.clone().into(),
        album: t.album.clone().into(),
        duration: format_duration(u64::from(t.duration_ms)).into(),
        state: row_state(t, playing),
        image: Image::default(),
        has_image: false,
    }
}

fn entry_track(e: &Entry) -> TrackItem {
    TrackItem {
        uri: e.uri.clone(),
        title: e.title.clone(),
        artists: e.artists.clone(),
        artist_uri: e.artist_uri.clone(),
        album: e.album.clone(),
        duration_ms: e.duration_ms,
        playable: true,
        image_url: e.image_url.clone(),
        ..TrackItem::default()
    }
}

/// Case- and diacritics-light match used by the filter and the palette.
fn matches(hay: &str, needle_lower: &str) -> bool {
    hay.to_lowercase().replace('ё', "е").contains(needle_lower)
}

fn norm(q: &str) -> String {
    q.trim().to_lowercase().replace('ё', "е")
}

impl UiState {
    fn track_code(&self, index: usize, t: &TrackItem) -> String {
        match self.page_kind {
            Some(CollectionKind::Album) if t.track_number > 0 && self.sort == 0 => {
                t.track_number.to_string()
            }
            _ => (index + 1).to_string(),
        }
    }

    /// Recomputes which rows are shown (filter + sort) and refills the model.
    fn rebuild_view(&mut self, w: &AppWindow) {
        let needle = norm(&self.filter);
        let mut view: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| {
                let t = &self.tracks[i];
                needle.is_empty()
                    || matches(&t.title, &needle)
                    || matches(&t.artists, &needle)
                    || matches(&t.album, &needle)
            })
            .collect();
        let key = |i: &usize| {
            let t = &self.tracks[*i];
            match self.sort {
                1 => t.title.to_lowercase(),
                2 => t.artists.to_lowercase(),
                3 => t.album.to_lowercase(),
                _ => String::new(),
            }
        };
        match self.sort {
            1..=3 => view.sort_by_cached_key(key),
            4 => view.sort_by_key(|&i| self.tracks[i].duration_ms),
            _ => {}
        }
        let rows: Vec<TrackData> = view
            .iter()
            .enumerate()
            .map(|(n, &i)| {
                let t = &self.tracks[i];
                let code = if self.sort == 0 && needle.is_empty() {
                    self.track_code(i, t)
                } else {
                    (n + 1).to_string()
                };
                track_data(code, t, &self.playing_uri)
            })
            .collect();
        self.track_rows.set_vec(rows);
        w.set_page_shown(i32::try_from(view.len()).unwrap_or(i32::MAX));
        self.view = view;
    }

    fn view_is_plain(&self) -> bool {
        self.filter.is_empty() && self.sort == 0
    }

    pub fn page_filter(&mut self, w: &AppWindow, text: &str) {
        self.filter = text.to_owned();
        self.rebuild_view(w);
    }

    pub fn page_sort(&mut self, w: &AppWindow, sort: i32) {
        self.sort = sort;
        self.rebuild_view(w);
    }

    /// Navigates to a page; the library loads it and answers with events.
    pub fn open(&mut self, w: &AppWindow, req: PageRequest, push_history: bool) -> Vec<Effect> {
        if push_history && let Some(cur) = self.current.take() {
            self.history.push(cur);
        }
        self.current = Some(req.clone());
        self.min_page_id = self.page_id + 1;
        self.page_kind = None;
        self.tracks.clear();
        self.view.clear();
        self.page_tiles.clear();
        self.track_rows.set_vec(Vec::new());
        self.tile_rows.set_vec(Vec::new());
        self.filter.clear();
        self.sort = 0;
        self.autoplay = false;
        self.page_image_url = None;
        w.set_page_filter("".into());
        w.set_page_sort(0);
        w.set_page_shown(0);
        let (kind, uri) = request_uri(&req);
        self.page_uri.clone_from(&uri);
        self.page_title.clear();
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
        Theme::get(w).set_page_accent(if kind == 0 {
            Color::from_rgb_u8(0x5B, 0x3C, 0xF0)
        } else {
            DEFAULT_PAGE_ACCENT
        });
        w.set_page(crate::Page::Collection);
        vec![Effect::Open(req)]
    }

    /// The play button on a tile or card: play that collection (opening it to load its tracks).
    pub fn play_collection(&mut self, w: &AppWindow, kind: i32, uri: String) -> Vec<Effect> {
        let req = request_from(kind, uri.clone());
        let (_, page_uri) = request_uri(&req);
        if self.page_uri == page_uri && !self.tracks.is_empty() {
            return self.play_page(false);
        }
        let fx = self.open(w, req, true);
        self.autoplay = true;
        fx
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

    fn page_context(&self) -> Context {
        Context {
            uri: self.page_uri.clone(),
            kind: self.page_kind.map_or(1, kind_code),
            title: self.page_title.clone(),
            image_url: self.page_image_url.clone(),
        }
    }

    fn start_context(
        &mut self,
        tracks: &[TrackItem],
        start: Option<usize>,
        name: &str,
        shuffle: Option<bool>,
        ctx: Context,
    ) -> Vec<Effect> {
        let fx = context(tracks, start, name, shuffle);
        if let Some(Effect::Player(PlayerCommand::PlayContext { items, .. })) = fx.first() {
            self.context_items.clone_from(items);
            self.context = ctx;
            self.resume = None;
        }
        fx
    }

    /// Plays the page from shown row `index`: the shown rows become the queue context.
    pub fn play_row(&mut self, w: &AppWindow, index: usize) -> Vec<Effect> {
        let shown: Vec<TrackItem> = self.view.iter().map(|&i| self.tracks[i].clone()).collect();
        let (name, ctx) = (self.page_title.clone(), self.page_context());
        let fx = self.start_context(&shown, Some(index), &name, None, ctx);
        self.mark_context(w);
        fx
    }

    pub fn play_page(&mut self, shuffle: bool) -> Vec<Effect> {
        let shown: Vec<TrackItem> = self.view.iter().map(|&i| self.tracks[i].clone()).collect();
        let (name, ctx) = (self.page_title.clone(), self.page_context());
        self.start_context(&shown, None, &name, Some(shuffle), ctx)
    }

    pub fn mark_context(&self, w: &AppWindow) {
        w.set_context_uri(self.context.uri.clone().into());
    }

    fn list_for(&self, source: i32) -> Vec<TrackItem> {
        match source {
            1 => self.search.results.tracks.clone(),
            2 => self.history_items.iter().map(entry_track).collect(),
            _ => self.view.iter().map(|&i| self.tracks[i].clone()).collect(),
        }
    }

    /// Right-click menu: 0 add to queue, 1 go to artist, 2 go to album, 3 like, 4 radio.
    pub fn row_action(
        &mut self,
        w: &AppWindow,
        source: i32,
        index: usize,
        action: i32,
    ) -> Vec<Effect> {
        let Some(t) = self.list_for(source).get(index).cloned() else {
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
            4 => self.radio(w, &t.uri, &t.title),
            _ => Vec::new(),
        }
    }

    fn radio(&mut self, w: &AppWindow, uri: &str, title: &str) -> Vec<Effect> {
        if !uri.starts_with("spotify:track:") {
            return Vec::new();
        }
        w.set_notice(format!("Собираем радио по «{title}»…").into());
        w.set_notice_is_error(false);
        vec![Effect::Library(LibraryCommand::Radio {
            uri: uri.to_owned(),
            title: title.to_owned(),
        })]
    }

    pub fn radio_current(&mut self, w: &AppWindow) -> Vec<Effect> {
        let (uri, title) = (self.playing.uri.clone(), self.playing.title.clone());
        self.radio(w, &uri, &title)
    }

    /// Plays from the history list (`index >= 0`) or the week's top tracks (`-1 - n`).
    pub fn history_play(&mut self, index: i32) -> Vec<Effect> {
        let (items, start, name) = if index >= 0 {
            (&self.history_items, index, "Недавно слушали")
        } else {
            (&self.top_items, -1 - index, "Топ недели")
        };
        let tracks: Vec<TrackItem> = items.iter().map(entry_track).collect();
        let start = usize::try_from(start).ok();
        self.start_context(&tracks, start, name, None, Context::default())
    }

    pub fn queue_move(from: i32, to: i32) -> Vec<Effect> {
        match (usize::try_from(from), usize::try_from(to)) {
            (Ok(from), Ok(to)) if from != to => {
                vec![Effect::Player(PlayerCommand::MoveInQueue { from, to })]
            }
            _ => Vec::new(),
        }
    }

    pub fn on_queue(&mut self, w: &AppWindow, q: &QueueSnapshot) {
        let rows: Vec<QueueRow> = q
            .upcoming
            .iter()
            .enumerate()
            .map(|(i, item)| QueueRow {
                code: format!("{}", i + 1).into(),
                title: if item.title.is_empty() {
                    item.uri.clone().into()
                } else {
                    item.title.clone().into()
                },
                artist: item.artists.clone().into(),
                manual: i < q.manual,
            })
            .collect();
        w.set_queue_items(ModelRc::from(Rc::new(VecModel::from(rows))));
        w.set_queue_context(q.context_name.clone().into());
        w.set_shuffle_on(q.shuffle);
        w.set_repeat_mode(match q.repeat {
            tonearm_player::RepeatMode::Off => 0,
            tonearm_player::RepeatMode::All => 1,
            tonearm_player::RepeatMode::One => 2,
        });
        if q.context_name != self.context_name && q.context_name != self.context.title {
            // A context we did not start (a link, Connect hand-back): no page to return to.
            self.context = Context {
                title: q.context_name.clone(),
                ..Context::default()
            };
            self.mark_context(w);
        }
        self.mark_context(w);
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

    pub fn on_devices(&mut self, w: &AppWindow, mut list: Vec<tonearm_core::connect::Device>) {
        // Our own Soloist is "this computer", not a remote device.
        list.retain(|d| d.name != tonearm_player::soloist::DEVICE_NAME);
        let rows: Vec<crate::DeviceRow> = list
            .iter()
            .map(|d| crate::DeviceRow {
                id: d.id.clone().into(),
                name: d.name.clone().into(),
                kind: d.kind.clone().into(),
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
            .map_or_else(|| "устройство".into(), |d| d.name.clone())
    }

    fn set_playlists(&mut self, lists: Vec<PlaylistSummary>) -> Vec<Effect> {
        let old: Vec<PlaylistEntry> = (0..self.playlist_rows.row_count_())
            .filter_map(|i| self.playlist_rows.row(i))
            .collect();
        let mut fx = Vec::new();
        let rows: Vec<PlaylistEntry> = lists
            .iter()
            .map(|p| {
                let prev = old.iter().find(|o| o.uri.as_str() == p.uri && o.has_image);
                let same_cover = self
                    .playlists
                    .iter()
                    .any(|q| q.uri == p.uri && q.image_url == p.image_url);
                if (prev.is_none() || !same_cover)
                    && let Some(u) = &p.image_url
                {
                    fx.push(Effect::Cover {
                        url: u.clone(),
                        size: 96,
                        target: CoverTarget::Playlist(p.uri.clone()),
                    });
                }
                PlaylistEntry {
                    uri: p.uri.clone().into(),
                    name: p.name.clone().into(),
                    count: i32::try_from(p.track_count).unwrap_or(i32::MAX),
                    owner: String::new().into(),
                    image: prev
                        .filter(|_| same_cover)
                        .map(|o| o.image.clone())
                        .unwrap_or_default(),
                    has_image: prev.is_some() && same_cover,
                }
            })
            .collect();
        update_rows(&self.playlist_rows, rows);
        self.playlists = lists;
        fx
    }

    pub fn on_library(&mut self, w: &AppWindow, ev: LibraryEvent) -> Vec<Effect> {
        let mut fx = Vec::new();
        match ev {
            LibraryEvent::Playlists(lists) => {
                fx.extend(self.set_playlists(lists));
                if self.home.is_empty() {
                    fx.extend(self.set_home(w, self.playlist_tiles()));
                }
                fx.extend(self.refresh_recent(w));
            }
            LibraryEvent::LikedCount(n) => w.set_liked_count(i32::try_from(n).unwrap_or(i32::MAX)),
            LibraryEvent::Home(tiles) if !tiles.is_empty() => fx.extend(self.set_home(w, tiles)),
            LibraryEvent::Home(_) => {}
            LibraryEvent::PageHeader { page_id, header } if page_id >= self.min_page_id => {
                self.page_id = page_id;
                fx.extend(self.set_header(w, &header));
            }
            LibraryEvent::PageImage { page_id, url }
                if page_id == self.page_id && page_id >= self.min_page_id =>
            {
                self.page_image_url = Some(url.clone());
                fx.push(Effect::Cover {
                    url,
                    size: 400,
                    target: CoverTarget::Page(page_id),
                });
            }
            LibraryEvent::PageTracks {
                page_id,
                tracks,
                done,
            } if page_id == self.page_id && page_id >= self.min_page_id => {
                let start = self.tracks.len();
                self.tracks.extend(tracks);
                if self.view_is_plain() {
                    for i in start..self.tracks.len() {
                        let row = track_data(
                            self.track_code(i, &self.tracks[i]),
                            &self.tracks[i],
                            &self.playing_uri,
                        );
                        self.track_rows.push(row);
                        self.view.push(i);
                    }
                    w.set_page_shown(i32::try_from(self.view.len()).unwrap_or(i32::MAX));
                } else {
                    self.rebuild_view(w);
                }
                if done {
                    w.set_page_loading(false);
                    let total: u64 = self.tracks.iter().map(|t| u64::from(t.duration_ms)).sum();
                    w.set_page_length_label(format_duration(total).into());
                    w.set_page_tracks_label(self.tracks.len().to_string().into());
                    if std::mem::take(&mut self.autoplay) {
                        fx.extend(self.play_page(false));
                        self.mark_context(w);
                    }
                }
            }
            LibraryEvent::PageTiles { page_id, tiles }
                if page_id == self.page_id && page_id >= self.min_page_id =>
            {
                self.tile_rows
                    .set_vec(tiles.iter().map(tile_data).collect::<Vec<_>>());
                fx.extend(tiles.iter().filter_map(|t| {
                    t.image_url.as_ref().map(|u| Effect::Cover {
                        url: u.clone(),
                        size: 240,
                        target: CoverTarget::Search(t.uri.clone()),
                    })
                }));
                self.page_tiles = tiles;
                w.set_page_loading(false);
            }
            LibraryEvent::PageError { page_id, message } if page_id >= self.min_page_id => {
                w.set_page_loading(false);
                w.set_page_error(message.into());
                self.autoplay = false;
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
                    w.set_search_status(message.into());
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
                    self.rebuild_view(w);
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
            LibraryEvent::TrackMeta(item) if item.uri == self.playing_uri => {
                fx.extend(self.on_track_meta(w, &item, false));
            }
            LibraryEvent::Radio { title, tracks } => {
                w.set_notice(format!("Радио по «{title}»").into());
                w.set_notice_is_error(false);
                let ctx = Context {
                    title: format!("Радио: {title}"),
                    ..Context::default()
                };
                let name = ctx.title.clone();
                fx.extend(self.start_context(&tracks, Some(0), &name, Some(false), ctx));
                self.mark_context(w);
            }
            LibraryEvent::Lyrics { uri, lyrics } if uri == self.playing_uri => {
                self.lyrics_uri = uri;
                self.lyrics_index = None;
                match lyrics {
                    Some(l) => {
                        let rows: Vec<LyricRow> = l
                            .lines
                            .iter()
                            .map(|line| LyricRow {
                                text: line.text.clone().into(),
                            })
                            .collect();
                        w.set_lyrics(ModelRc::from(Rc::new(VecModel::from(rows))));
                        w.set_lyrics_synced(l.synced);
                        w.set_lyrics_provider(l.provider.clone().into());
                        w.set_lyrics_status("".into());
                        self.lyrics = l.lines;
                        self.update_lyrics_line(w);
                    }
                    None => {
                        self.lyrics.clear();
                        w.set_lyrics_status("Для этого трека текста нет".into());
                        w.set_lyrics_provider("".into());
                    }
                }
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
            .take(12)
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
        // Owners of internal playlists come as user ids: show the track count instead.
        let tiles: Vec<Tile> = tiles
            .into_iter()
            .map(|mut t| {
                if t.kind == CollectionKind::Playlist
                    && let Some(p) = self.playlists.iter().find(|p| p.uri == t.uri)
                {
                    t.subtitle = format!("{} треков", p.track_count);
                }
                t
            })
            .collect();
        let old: Vec<TileData> = (0..self.home_rows.row_count_())
            .filter_map(|i| self.home_rows.row(i))
            .collect();
        let mut fx = Vec::new();
        let rows: Vec<TileData> = tiles
            .iter()
            .map(|t| {
                let same = self
                    .home
                    .iter()
                    .any(|h| h.uri == t.uri && h.image_url == t.image_url);
                if let Some(o) = old.iter().find(|o| o.uri.as_str() == t.uri && o.has_image)
                    && same
                {
                    let mut row = tile_data(t);
                    row.image = o.image.clone();
                    row.has_image = true;
                    return row;
                }
                if let Some(u) = &t.image_url {
                    fx.push(Effect::Cover {
                        url: u.clone(),
                        size: 320,
                        target: CoverTarget::Home(t.uri.clone()),
                    });
                }
                tile_data(t)
            })
            .collect();
        update_rows(&self.home_rows, rows);
        self.home = tiles;
        fx
    }

    /// "Недавно слушали" on the home page, from the local history.
    fn refresh_recent(&mut self, _w: &AppWindow) -> Vec<Effect> {
        let contexts = self.store.recent_contexts(RECENT_TILES);
        let mut fx = Vec::new();
        let rows: Vec<TileData> = contexts
            .iter()
            .map(|c| {
                // Prefer the sidebar's current cover (playlists change covers).
                let url = self
                    .playlists
                    .iter()
                    .find(|p| p.uri == c.uri)
                    .and_then(|p| p.image_url.clone())
                    .or_else(|| c.image_url.clone());
                if let Some(u) = url {
                    fx.push(Effect::Cover {
                        url: u,
                        size: 320,
                        target: CoverTarget::Recent(c.uri.clone()),
                    });
                }
                TileData {
                    uri: c.uri.clone().into(),
                    kind: c.kind,
                    title: c.title.clone().into(),
                    subtitle: match c.kind {
                        0 => "Ваши любимые треки",
                        2 => "Альбом",
                        3 => "Исполнитель",
                        _ => "Плейлист",
                    }
                    .into(),
                    image: Image::default(),
                    has_image: false,
                }
            })
            .collect();
        self.recent_rows.set_vec(rows);
        fx
    }

    fn set_header(&mut self, w: &AppWindow, h: &PageHeader) -> Vec<Effect> {
        self.page_kind = Some(h.kind);
        self.page_title = h.title.clone();
        self.byline_uri = h.byline_uri.clone();
        self.page_image_url = h.image_url.clone();
        w.set_page_kind(kind_code(h.kind));
        w.set_page_title(h.title.clone().into());
        // Internal playlist owners come as user ids; long random ids read as noise.
        let byline = if h.kind == CollectionKind::Playlist && h.byline.len() > 20 {
            String::new()
        } else {
            h.byline.clone()
        };
        w.set_page_byline(byline.into());
        w.set_page_year_label(h.year.map(|y| y.to_string()).unwrap_or_default().into());
        if let Some(n) = h.total_tracks {
            w.set_page_tracks_label(n.to_string().into());
        }
        h.image_url
            .as_ref()
            .map(|u| {
                vec![Effect::Cover {
                    url: u.clone(),
                    size: 400,
                    target: CoverTarget::Page(self.page_id),
                }]
            })
            .unwrap_or_default()
    }

    /// A cover colour made to read on the theme: covers give light tints, light themes need
    /// darker ones.
    fn tint(&self, [r, g, b]: [u8; 3]) -> Color {
        let c = Color::from_rgb_u8(r, g, b);
        if self.theme_light { c.darker(0.45) } else { c }
    }

    /// Reads the built-in and user themes, lists them and applies `id`.
    pub fn load_themes(&mut self, w: &AppWindow, id: &str) {
        let catalog = themes::Catalog::load(themes::dir().as_deref());
        let cards: Vec<ThemeCard> = catalog
            .themes
            .iter()
            .map(|t| ThemeCard {
                id: t.id.clone().into(),
                name: t.name.clone().into(),
                bg: rgba(t.colors.get("bg")),
                surface: rgba(t.colors.get("surface-2")),
                text: rgba(t.colors.get("text-1")),
                muted: rgba(t.colors.get("text-3")),
                accent: rgba(t.colors.get("accent")),
                custom: t.is_custom(),
            })
            .collect();
        w.set_themes(ModelRc::from(Rc::new(VecModel::from(cards))));
        w.set_theme_errors(catalog.errors.join("; ").into());
        let theme = catalog.get(id).clone();
        w.set_theme_id(theme.id.clone().into());
        self.theme_mtime = themes::modified(&theme.id);
        self.apply_theme(w, &theme);
        self.themes = Some(catalog);
    }

    /// A theme was picked on the settings page.
    pub fn pick_theme(&mut self, w: &AppWindow, id: &str) {
        let Some(theme) = self.themes.as_ref().map(|c| c.get(id).clone()) else {
            return;
        };
        w.set_theme_id(theme.id.clone().into());
        self.theme_mtime = themes::modified(&theme.id);
        self.apply_theme(w, &theme);
    }

    /// "Создать из текущей": a copy in the themes folder, picked right away.
    pub fn new_theme(&mut self, w: &AppWindow) {
        let id = w.get_theme_id().to_string();
        let Some(current) = self.themes.as_ref().map(|c| c.get(&id).clone()) else {
            return;
        };
        match themes::save_copy(&current) {
            Ok(new_id) => {
                self.load_themes(w, &new_id);
                w.set_notice("Тема сохранена в папке тем: откройте файл и меняйте цвета".into());
                w.set_notice_is_error(false);
            }
            Err(e) => {
                w.set_notice(format!("Не удалось создать тему: {e}").into());
                w.set_notice_is_error(true);
            }
        }
    }

    /// Called every second: re-reads the active custom theme when its file changed.
    pub fn watch_theme(&mut self, w: &AppWindow) {
        self.theme_check += 1;
        if self.theme_check < 2 {
            return;
        }
        self.theme_check = 0;
        let id = w.get_theme_id().to_string();
        if !id.starts_with("file:") {
            return;
        }
        let now = themes::modified(&id);
        if now != self.theme_mtime {
            self.load_themes(w, &id);
        }
    }

    /// Puts the theme's colours on the window.
    pub fn apply_theme(&mut self, w: &AppWindow, t: &themes::ThemeDef) {
        let th = Theme::get(w);
        let c = |k: &str| rgba(t.colors.get(k));
        th.set_bg(c("bg"));
        th.set_sidebar(c("sidebar"));
        th.set_surface(c("surface"));
        th.set_surface_2(c("surface-2"));
        th.set_hover(c("hover"));
        th.set_pressed(c("pressed"));
        th.set_selected(c("selected"));
        th.set_line(c("line"));
        th.set_line_2(c("line-2"));
        th.set_text_1(c("text-1"));
        th.set_text_2(c("text-2"));
        th.set_text_3(c("text-3"));
        th.set_danger(c("danger"));
        th.set_on_accent(c("on-accent"));
        th.set_light(t.light);
        self.theme_light = t.light;
        self.theme_accent = (!t.accent_from_cover).then(|| c("accent"));
        let accent = match (self.theme_accent, self.cover_rgb) {
            (Some(fixed), _) => fixed,
            (None, Some(rgb)) => self.tint(rgb),
            (None, None) => c("accent"),
        };
        th.set_accent(accent);
    }

    pub fn on_cover(&mut self, w: &AppWindow, target: &CoverTarget, cover: &Cover) {
        let set_tiles = |model: &VecModel<TileData>, uri: &str| {
            for i in 0..model.row_count_() {
                if let Some(mut row) = model.row(i)
                    && row.uri.as_str() == uri
                {
                    row.image = to_image(cover);
                    row.has_image = true;
                    model.set_row(i, row);
                }
            }
        };
        let set_tracks = |model: &VecModel<TrackData>, uris: &[&str], uri: &str| {
            for (i, u) in uris.iter().enumerate() {
                if *u == uri
                    && let Some(mut row) = model.row(i)
                {
                    row.image = to_image(cover);
                    row.has_image = true;
                    model.set_row(i, row);
                }
            }
        };
        match target {
            CoverTarget::Home(uri) => set_tiles(&self.home_rows, uri),
            CoverTarget::Recent(uri) => set_tiles(&self.recent_rows, uri),
            CoverTarget::Playlist(uri) => {
                for i in 0..self.playlist_rows.row_count_() {
                    if let Some(mut row) = self.playlist_rows.row(i)
                        && row.uri.as_str() == uri
                    {
                        row.image = to_image(cover);
                        row.has_image = true;
                        self.playlist_rows.set_row(i, row);
                    }
                }
            }
            CoverTarget::Page(id) if *id == self.page_id => {
                w.set_page_image(to_image(cover));
                w.set_page_has_image(true);
                Theme::get(w).set_page_accent(self.tint(cover.accent));
            }
            CoverTarget::Search(uri) => {
                for model in [
                    &self.search.albums,
                    &self.search.artists,
                    &self.search.playlists,
                    &self.tile_rows,
                ] {
                    set_tiles(model, uri);
                }
                let mut best = w.get_search_best();
                if best.uri.as_str() == uri {
                    best.image = to_image(cover);
                    best.has_image = true;
                    w.set_search_best(best);
                }
            }
            CoverTarget::SearchTrack(url) => {
                let urls: Vec<&str> = self
                    .search
                    .results
                    .tracks
                    .iter()
                    .map(|t| t.image_url.as_deref().unwrap_or(""))
                    .collect();
                set_tracks(&self.search.tracks, &urls, url);
            }
            CoverTarget::History(url) => {
                let urls: Vec<&str> = self
                    .history_items
                    .iter()
                    .map(|t| t.image_url.as_deref().unwrap_or(""))
                    .collect();
                set_tracks(&self.history_rows, &urls, url);
                for i in 0..self.stat_track_rows.row_count_() {
                    if self.top_items.get(i).and_then(|e| e.image_url.as_deref()) == Some(url)
                        && let Some(mut row) = self.stat_track_rows.row(i)
                    {
                        row.image = to_image(cover);
                        row.has_image = true;
                        self.stat_track_rows.set_row(i, row);
                    }
                }
            }
            CoverTarget::Deck(uri) if *uri == self.playing_uri => {
                w.set_track_image(to_image(cover));
                w.set_track_backdrop(backdrop(cover));
                w.set_track_has_image(true);
                self.cover_rgb = Some(cover.accent);
                if self.theme_accent.is_none() {
                    Theme::get(w).set_accent(self.tint(cover.accent));
                }
            }
            _ => {}
        }
    }

    /// Fills what the player left out (Soloist often reports only the title) from the lists
    /// we showed, the queue and the history.
    fn fill_known(&self, track: &mut TrackInfo) {
        if let Some(t) = self
            .tracks
            .iter()
            .chain(&self.search.results.tracks)
            .find(|t| t.uri == track.uri)
        {
            merge_track(track, t);
            return;
        }
        if let Some(q) = self.context_items.iter().find(|q| q.uri == track.uri) {
            fill(&mut track.title, &q.title);
            fill(&mut track.artists, &q.artists);
            if track.duration_ms == 0 {
                track.duration_ms = q.duration_ms;
            }
        }
        if let Some(e) = self.store.entries.iter().rev().find(|e| e.uri == track.uri) {
            fill(&mut track.title, &e.title);
            fill(&mut track.artists, &e.artists);
            fill(&mut track.album, &e.album);
            if track.duration_ms == 0 {
                track.duration_ms = e.duration_ms;
            }
            if track.cover_url.is_none() {
                track.cover_url.clone_from(&e.image_url);
            }
        }
    }

    /// Details of the playing track arrived from the metadata service.
    /// Album and first artist of a track from the lists we showed or the history.
    fn known_links(&self, uri: &str) -> (Option<String>, Option<String>) {
        if let Some(t) = self
            .tracks
            .iter()
            .chain(&self.search.results.tracks)
            .find(|t| t.uri == uri)
        {
            return (t.album_uri.clone(), t.artist_uri.clone());
        }
        let artist = self
            .store
            .entries
            .iter()
            .rev()
            .find(|e| e.uri == uri)
            .and_then(|e| e.artist_uri.clone());
        (None, artist)
    }

    /// A click on the title (album) or the artist in the player bar.
    pub fn open_playing(&mut self, w: &AppWindow, artist: bool) -> Vec<Effect> {
        let target = if artist {
            self.playing_artist.clone().map(PageRequest::Artist)
        } else {
            self.playing_album.clone().map(PageRequest::Album)
        };
        match target {
            Some(req) => self.open(w, req, true),
            None if self.playing_uri.starts_with("spotify:track:") => {
                self.open_when_known = Some(artist);
                if self.meta_pending {
                    Vec::new()
                } else {
                    self.meta_pending = true;
                    vec![Effect::Library(LibraryCommand::TrackMeta(
                        self.playing_uri.clone(),
                    ))]
                }
            }
            None => Vec::new(),
        }
    }

    /// The playing track's cover did not load: ask the metadata service for the album's, once.
    pub fn deck_cover_failed(&mut self, _w: &AppWindow, uri: &str, url: &str) -> Vec<Effect> {
        if uri != self.playing_uri || self.playing.cover_url.as_deref() != Some(url) {
            return Vec::new();
        }
        self.playing.cover_url = None;
        if self.deck_retried || self.meta_pending || !uri.starts_with("spotify:track:") {
            return Vec::new();
        }
        self.deck_retried = true;
        self.meta_pending = true;
        vec![Effect::Library(LibraryCommand::TrackMeta(uri.to_owned()))]
    }

    /// `still_pending`: a fuller report is expected (lyrics wait for it unless the track is
    /// complete now).
    fn on_track_meta(
        &mut self,
        w: &AppWindow,
        item: &TrackItem,
        still_pending: bool,
    ) -> Vec<Effect> {
        let mut fx = Vec::new();
        let had_cover = self.playing.cover_url.is_some();
        merge_track(&mut self.playing, item);
        if self.playing_album.is_none() {
            self.playing_album.clone_from(&item.album_uri);
        }
        if self.playing_artist.is_none() {
            self.playing_artist.clone_from(&item.artist_uri);
        }
        if let Some(artist) = self.open_when_known.take() {
            let known = if artist {
                &self.playing_artist
            } else {
                &self.playing_album
            };
            if known.is_some() {
                fx.extend(self.open_playing(w, artist));
            }
        }
        let t = &self.playing;
        w.set_track_title(t.title.clone().into());
        w.set_track_artists(t.artists.clone().into());
        w.set_duration_ms(i32::try_from(t.duration_ms).unwrap_or(i32::MAX));
        if self.context.title.is_empty() {
            w.set_track_context(t.album.clone().into());
        }
        if !had_cover && let Some(u) = t.cover_url.clone() {
            fx.push(Effect::Cover {
                url: u,
                size: 320,
                target: CoverTarget::Deck(t.uri.clone()),
            });
        }
        let t = &self.playing;
        let complete = t.duration_ms > 0 && !t.artists.is_empty() && t.cover_url.is_some();
        self.meta_pending = still_pending && !complete;
        if w.get_lyrics_open() {
            fx.extend(self.lyrics_opened(w));
        }
        fx
    }

    pub fn on_track(&mut self, w: &AppWindow, track: &TrackInfo) -> Vec<Effect> {
        // The same track again with more details (Soloist fills them in later): merge, keep
        // the listening time and the lyrics.
        if !track.uri.is_empty() && track.uri == self.playing_uri {
            let item = TrackItem {
                uri: track.uri.clone(),
                title: track.title.clone(),
                artists: track.artists.clone(),
                album: track.album.clone(),
                duration_ms: track.duration_ms,
                image_url: track.cover_url.clone(),
                ..TrackItem::default()
            };
            // A metadata request may still be on its way; it is harmless when it lands.
            let pending = self.meta_pending;
            let fx = self.on_track_meta(w, &item, pending);
            return fx;
        }
        let mut fx = self.finish_listen(w);
        self.deck_retried = false;
        self.open_when_known = None;
        let mut track = track.clone();
        self.fill_known(&mut track);
        let track = &track;
        (self.playing_album, self.playing_artist) = self.known_links(&track.uri);
        self.meta_pending = track.uri.starts_with("spotify:track:")
            && (track.duration_ms == 0
                || track.artists.is_empty()
                || track.cover_url.is_none()
                || self.playing_album.is_none()
                || self.playing_artist.is_none());
        let previous = std::mem::replace(&mut self.playing_uri, track.uri.clone());
        self.playing = track.clone();
        self.listened_ms = 0;
        self.started_at = history::now();
        self.resume = None;
        w.set_resume_pending(false);
        // Re-mark only rows whose state changes.
        for (n, &i) in self.view.iter().enumerate() {
            let t = &self.tracks[i];
            if (t.uri == previous || t.uri == track.uri)
                && let Some(mut row) = self.track_rows.row(n)
            {
                row.state = row_state(t, &self.playing_uri);
                self.track_rows.set_row(n, row);
            }
        }
        for (i, t) in self.search.results.tracks.iter().enumerate() {
            if (t.uri == previous || t.uri == track.uri)
                && let Some(mut row) = self.search.tracks.row(i)
            {
                row.state = row_state(t, &self.playing_uri);
                self.search.tracks.set_row(i, row);
            }
        }
        w.set_track_title(track.title.clone().into());
        w.set_track_artists(track.artists.clone().into());
        w.set_duration_ms(i32::try_from(track.duration_ms).unwrap_or(i32::MAX));
        w.set_track_context(
            if self.context.title.is_empty() {
                track.album.clone()
            } else {
                self.context.title.clone()
            }
            .into(),
        );
        w.set_track_has_image(false);
        if let Some(u) = track.cover_url.clone() {
            fx.push(Effect::Cover {
                url: u,
                size: 320,
                target: CoverTarget::Deck(track.uri.clone()),
            });
        }
        if self.meta_pending {
            fx.push(Effect::Library(LibraryCommand::TrackMeta(
                track.uri.clone(),
            )));
        }
        // Lyrics: reload for the new track if the panel is open, otherwise when it opens.
        self.lyrics.clear();
        self.lyrics_uri.clear();
        w.set_lyrics(ModelRc::default());
        w.set_lyrics_index(-1);
        if w.get_lyrics_open() {
            fx.extend(self.lyrics_opened(w));
        }
        fx.push(self.save_resume(0));
        fx
    }

    /// Records the track that just ended in the history if it played long enough.
    fn finish_listen(&mut self, w: &AppWindow) -> Vec<Effect> {
        if self.playing.uri.is_empty() || self.listened_ms < history::COUNT_AFTER_MS {
            return Vec::new();
        }
        let t = &self.playing;
        let known = self
            .tracks
            .iter()
            .chain(&self.search.results.tracks)
            .find(|x| x.uri == t.uri);
        self.store.add(Entry {
            uri: t.uri.clone(),
            title: t.title.clone(),
            artists: t.artists.clone(),
            artist_uri: known.and_then(|k| k.artist_uri.clone()),
            album: t.album.clone(),
            image_url: t.cover_url.clone(),
            duration_ms: t.duration_ms,
            listened_ms: self.listened_ms,
            at: self.started_at,
            context: self.context.clone(),
        });
        self.listened_ms = 0;
        self.refresh_recent(w)
    }

    /// Called by the 1 s clock while the local player plays.
    pub fn tick(&mut self, w: &AppWindow, playing: bool) -> Vec<Effect> {
        let mut fx = Vec::new();
        if playing && !self.playing_uri.is_empty() {
            self.listened_ms = self.listened_ms.saturating_add(1000);
            self.since_save += 1;
            if self.since_save >= 15 {
                let pos = u32::try_from(w.get_position_ms()).unwrap_or(0);
                drop(self.save_resume(pos));
                if let Some(r) = &self.store.resume {
                    fx.push(Effect::SaveResume(Box::new(r.clone())));
                }
            }
        }
        self.update_lyrics_line(w);
        fx
    }

    pub fn position_changed(&mut self, w: &AppWindow) {
        self.update_lyrics_line(w);
    }

    fn update_lyrics_line(&mut self, w: &AppWindow) {
        if self.lyrics.is_empty() || !w.get_lyrics_synced() {
            return;
        }
        // Show a line slightly early: reading lags behind hearing.
        let pos = u32::try_from(w.get_position_ms() + 250).unwrap_or(0);
        let idx = current_line(&self.lyrics, pos);
        if idx != self.lyrics_index {
            self.lyrics_index = idx;
            w.set_lyrics_index(idx.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1));
        }
    }

    pub fn lyrics_opened(&mut self, w: &AppWindow) -> Vec<Effect> {
        if self.playing.uri.is_empty() {
            w.set_lyrics_status("Сначала включите трек".into());
            return Vec::new();
        }
        if self.lyrics_uri == self.playing.uri {
            return Vec::new();
        }
        w.set_lyrics_status("Загружаем текст…".into());
        if self.meta_pending {
            // Asked again once the artist and length are known: LRCLIB matches on them.
            return Vec::new();
        }
        w.set_lyrics_provider("".into());
        vec![Effect::Library(LibraryCommand::Lyrics {
            uri: self.playing.uri.clone(),
            title: self.playing.title.clone(),
            artist: self.playing.artists.clone(),
            duration_ms: self.playing.duration_ms,
        })]
    }

    /// Snapshot for "continue where you left off"; written off the UI thread.
    fn save_resume(&mut self, position_ms: u32) -> Effect {
        self.since_save = 0;
        let t = &self.playing;
        let items: Vec<QueuedTrack> = self
            .context_items
            .iter()
            .map(|q| QueuedTrack {
                uri: q.uri.clone(),
                title: q.title.clone(),
                artists: q.artists.clone(),
                duration_ms: q.duration_ms,
            })
            .collect();
        let start = items.iter().position(|q| q.uri == t.uri).unwrap_or(0);
        if !t.uri.is_empty() {
            self.store.resume = Some(Resume {
                uri: t.uri.clone(),
                title: t.title.clone(),
                artists: t.artists.clone(),
                album: t.album.clone(),
                cover_url: t.cover_url.clone(),
                duration_ms: t.duration_ms,
                position_ms,
                items,
                start,
                context: self.context.clone(),
            });
        }
        Effect::Save(Box::new(Store {
            entries: self.store.entries.clone(),
            resume: self.store.resume.clone(),
        }))
    }

    /// Final save on exit, with the exact position.
    pub fn save_now(&mut self, w: &AppWindow) -> Option<Store> {
        let pos = u32::try_from(w.get_position_ms()).unwrap_or(0);
        if self.resume.is_some() {
            // Nothing played this session: keep last session's state as it was.
            return Some(std::mem::take(&mut self.store));
        }
        drop(self.finish_listen(w));
        match self.save_resume(pos) {
            Effect::Save(s) => Some(*s),
            _ => None,
        }
    }

    /// Shows last session's track paused (after login); play continues it.
    pub fn show_resume(&mut self, w: &AppWindow) -> Vec<Effect> {
        let Some(r) = self.store.resume.clone() else {
            return Vec::new();
        };
        if r.uri.is_empty() || !self.playing_uri.is_empty() {
            return Vec::new();
        }
        w.set_track_title(r.title.clone().into());
        w.set_track_artists(r.artists.clone().into());
        w.set_duration_ms(i32::try_from(r.duration_ms).unwrap_or(i32::MAX));
        w.set_position_ms(i32::try_from(r.position_ms).unwrap_or(0));
        w.set_play_view(PlayView::Paused);
        w.set_resume_pending(true);
        self.context = r.context.clone();
        self.mark_context(w);
        self.playing = TrackInfo {
            uri: r.uri.clone(),
            title: r.title.clone(),
            artists: r.artists.clone(),
            album: r.album.clone(),
            duration_ms: r.duration_ms,
            cover_url: r.cover_url.clone(),
        };
        self.playing_uri = r.uri.clone();
        let fx = r
            .cover_url
            .clone()
            .map(|u| Effect::Cover {
                url: u,
                size: 320,
                target: CoverTarget::Deck(r.uri.clone()),
            })
            .into_iter()
            .collect();
        self.resume = Some(r);
        fx
    }

    /// Play pressed while last session's track is shown: start its queue there, then jump to
    /// the saved position. `None` = nothing to resume, toggle as usual.
    pub fn take_resume(&mut self) -> Option<Vec<Effect>> {
        let r = self.resume.take()?;
        let mut items: Vec<QueueItem> = r
            .items
            .iter()
            .map(|q| QueueItem {
                uri: q.uri.clone(),
                title: q.title.clone(),
                artists: q.artists.clone(),
                duration_ms: q.duration_ms,
            })
            .collect();
        let mut start = r.start;
        if !items.iter().any(|q| q.uri == r.uri) {
            items = vec![QueueItem {
                uri: r.uri.clone(),
                title: r.title.clone(),
                artists: r.artists.clone(),
                duration_ms: r.duration_ms,
            }];
            start = 0;
        }
        self.context_items.clone_from(&items);
        self.playing_uri.clear();
        Some(vec![
            Effect::Player(PlayerCommand::PlayContext {
                items,
                start,
                name: r.context.title.clone(),
                shuffle: None,
            }),
            Effect::Player(PlayerCommand::PlayAt {
                uri: r.uri,
                position_ms: r.position_ms,
            }),
        ])
    }

    pub fn stats_opened(&mut self, w: &AppWindow) -> Vec<Effect> {
        let st = self.store.stats(history::now());
        w.set_stats_minutes(st.minutes.to_string().into());
        w.set_stats_plays(st.plays.to_string().into());
        w.set_stats_artists_count(st.artists.to_string().into());
        let mut fx = Vec::new();
        let max_t = st.top_tracks.first().map_or(1, |t| t.1.max(1));
        #[allow(clippy::cast_precision_loss)]
        let rows: Vec<StatRow> = st
            .top_tracks
            .iter()
            .map(|(e, n, _)| StatRow {
                title: e.title.clone().into(),
                subtitle: e.artists.clone().into(),
                value: plays_label(*n).into(),
                share: *n as f32 / max_t as f32,
                uri: e.uri.clone().into(),
                image: Image::default(),
                has_image: false,
            })
            .collect();
        self.stat_track_rows.set_vec(rows);
        self.top_items = st.top_tracks.iter().map(|t| t.0.clone()).collect();
        let max_a = st.top_artists.first().map_or(1, |a| a.2.max(1));
        #[allow(clippy::cast_precision_loss)]
        let artists: Vec<StatRow> = st
            .top_artists
            .iter()
            .map(|(name, uri, n, min)| StatRow {
                title: name.clone().into(),
                subtitle: format!("{min} мин").into(),
                value: plays_label(*n).into(),
                share: *n as f32 / max_a as f32,
                uri: uri.clone().unwrap_or_default().into(),
                image: Image::default(),
                has_image: false,
            })
            .collect();
        w.set_stats_artists(ModelRc::from(Rc::new(VecModel::from(artists))));
        self.history_items = self.store.recent_tracks(HISTORY_ROWS);
        let rows: Vec<TrackData> = self
            .history_items
            .iter()
            .enumerate()
            .map(|(i, e)| track_data((i + 1).to_string(), &entry_track(e), &self.playing_uri))
            .collect();
        self.history_rows.set_vec(rows);
        let mut urls: Vec<String> = self
            .history_items
            .iter()
            .chain(&self.top_items)
            .filter_map(|e| e.image_url.clone())
            .collect();
        urls.sort();
        urls.dedup();
        fx.extend(urls.into_iter().map(|u| Effect::Cover {
            url: u.clone(),
            size: 96,
            target: CoverTarget::History(u),
        }));
        fx
    }

    /// Sample listening history for the UI preview.
    #[cfg(debug_assertions)]
    pub fn preview_history(&mut self, w: &AppWindow) -> Vec<Effect> {
        let now = history::now();
        let tracks = [
            (
                "Огни над рекой",
                "Северный Ветер",
                "p0",
                "Ночные маршруты",
                9,
            ),
            ("Тёплый асфальт", "Лиса Мороз", "p1", "Утренний кофе", 7),
            ("Half Light", "Kite Harbor", "p2", "Фокус без слов", 6),
            ("Шум прибоя", "Тихий Порт", "p3", "Дорога на север", 4),
            ("Северное сияние", "Мира Лес", "p0", "Ночные маршруты", 3),
            ("Signal Lost", "Neon Fjord", "p2", "Фокус без слов", 2),
        ];
        let mut at = now - 6 * 24 * 3600;
        for round in 0..9 {
            for (i, (t, a, ctx, title, n)) in tracks.iter().enumerate() {
                if round < *n {
                    at += 1800;
                    self.store.add(Entry {
                        uri: format!("spotify:track:h{i}"),
                        title: (*t).into(),
                        artists: (*a).into(),
                        album: (*title).into(),
                        image_url: Some(format!("preview:{a}")),
                        duration_ms: 200_000,
                        listened_ms: 190_000,
                        at,
                        context: Context {
                            uri: format!("spotify:playlist:{}", &ctx[1..]),
                            kind: 1,
                            title: (*title).into(),
                            image_url: Some(format!("preview:{ctx}")),
                        },
                        ..Entry::default()
                    });
                }
            }
        }
        self.refresh_recent(w)
    }

    // ───── quick jump palette ─────

    pub fn palette_edited(&mut self, w: &AppWindow, text: &str) {
        let q = norm(text);
        let mut rows: Vec<PaletteRow> = Vec::new();
        let mut actions: Vec<PaletteAction> = Vec::new();
        let mut push = |row: PaletteRow, a: PaletteAction| {
            rows.push(row);
            actions.push(a);
        };
        let row = |kind: i32, icon: &str, title: &str, subtitle: &str| PaletteRow {
            kind,
            icon: icon.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            uri: "".into(),
        };
        let pages = [
            ("home", "Главная", crate::Page::Home),
            ("search", "Поиск", crate::Page::Search),
            ("library", "Медиатека", crate::Page::Library),
            ("stats", "Статистика", crate::Page::Stats),
            ("settings", "Настройки", crate::Page::Settings),
        ];
        let has_track = !self.playing.uri.is_empty();
        let commands: Vec<(&str, String, PaletteAction)> = [
            has_track.then(|| {
                (
                    "radio",
                    format!("Радио по треку «{}»", self.playing.title),
                    PaletteAction::Radio,
                )
            }),
            has_track.then(|| ("lyrics", "Текст песни".to_owned(), PaletteAction::Lyrics)),
            (!self.tracks.is_empty() && w.get_page() == crate::Page::Collection).then(|| {
                (
                    "shuffle",
                    format!("Перемешать «{}»", self.page_title),
                    PaletteAction::Shuffle,
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        if q.is_empty() {
            // Recent pages first, then the main pages and commands.
            for c in self.store.recent_contexts(4) {
                let icon = if c.kind == 0 { "heart-fill" } else { "library" };
                push(
                    row(if c.kind == 0 { 5 } else { 1 }, icon, &c.title, "Недавно"),
                    PaletteAction::Open(request_from(c.kind, c.uri.clone())),
                );
            }
            for (icon, title, page) in pages {
                push(row(0, icon, title, "Страница"), PaletteAction::Page(page));
            }
            for (icon, title, a) in commands {
                push(row(6, icon, &title, "Команда"), a);
            }
        } else {
            if matches("любимые треки liked", &q) {
                push(
                    row(5, "heart-fill", "Любимые треки", "Плейлист"),
                    PaletteAction::Open(PageRequest::Liked),
                );
            }
            for p in &self.playlists {
                if matches(&p.name, &q) {
                    push(
                        row(
                            1,
                            "library",
                            &p.name,
                            &format!("Плейлист · {} треков", p.track_count),
                        ),
                        PaletteAction::Open(PageRequest::Playlist(p.uri.clone())),
                    );
                }
            }
            for c in self.store.recent_contexts(30) {
                if c.kind == 2 && matches(&c.title, &q) {
                    push(
                        row(2, "library", &c.title, "Альбом"),
                        PaletteAction::Open(PageRequest::Album(c.uri.clone())),
                    );
                }
            }
            for (icon, title, page) in pages {
                if matches(title, &q) {
                    push(row(0, icon, title, "Страница"), PaletteAction::Page(page));
                }
            }
            for (icon, title, a) in commands {
                if matches(&title, &q) {
                    push(row(6, icon, &title, "Команда"), a);
                }
            }
            push(
                row(
                    4,
                    "search",
                    &format!("Искать «{}»", text.trim()),
                    "Поиск по Spotify",
                ),
                PaletteAction::Search(text.trim().to_owned()),
            );
        }
        rows.truncate(9);
        actions.truncate(9);
        w.set_palette_items(ModelRc::from(Rc::new(VecModel::from(rows))));
        self.palette = actions;
    }

    pub fn palette_activate(&mut self, w: &AppWindow, index: usize) -> Vec<Effect> {
        let Some(action) = self.palette.get(index).cloned() else {
            return Vec::new();
        };
        match action {
            PaletteAction::Page(p) => {
                w.set_page(p);
                Vec::new()
            }
            PaletteAction::Open(req) => self.open(w, req, true),
            PaletteAction::Search(q) => {
                w.set_page(crate::Page::Search);
                w.set_search_query(q.clone().into());
                self.search_edited(w, &q)
            }
            PaletteAction::Radio => self.radio_current(w),
            PaletteAction::Lyrics => {
                w.set_lyrics_open(true);
                self.lyrics_opened(w)
            }
            PaletteAction::Shuffle => self.play_page(true),
        }
    }
}

fn plays_label(n: usize) -> String {
    let word = match (n % 10, n % 100) {
        (1, x) if x != 11 => "раз",
        (2..=4, x) if !(12..=14).contains(&x) => "раза",
        _ => "раз",
    };
    format!("{n} {word}")
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

fn search_row(index: usize, t: &TrackItem, playing: &str) -> TrackData {
    track_data((index + 1).to_string(), t, playing)
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

const SEARCH_IDLE: &str = "Найдите трек, альбом, исполнителя или вставьте ссылку";

impl UiState {
    /// The query changed (typing). Links are not searched: Enter opens them.
    pub fn search_edited(&mut self, w: &AppWindow, text: &str) -> Vec<Effect> {
        let query = text.trim().to_owned();
        if link_target(&query).is_some() {
            self.search.query.clear();
            w.set_search_hint("Enter: открыть ссылку".into());
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
            w.set_search_status(SEARCH_IDLE.into());
            w.set_search_status_error(false);
            return Vec::new();
        }
        if self.search.results.is_empty() {
            w.set_search_status("Ищем…".into());
            w.set_search_status_error(false);
        }
        w.set_search_hint("Ищем…".into());
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

    pub fn search_play(&mut self, index: usize) -> Vec<Effect> {
        let name = format!("Поиск: {}", self.search.query);
        let tracks = self.search.results.tracks.clone();
        self.start_context(&tracks, Some(index), &name, None, Context::default())
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
        let mut track_urls: Vec<String> = page
            .tracks
            .iter()
            .filter_map(|t| t.image_url.clone())
            .collect();
        let results = if offset == 0 {
            page
        } else {
            let mut all = std::mem::take(&mut self.search.results);
            all.extend(page);
            all
        };
        if results.is_empty() {
            w.set_search_status("Ничего не нашлось".into());
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
                    size: 240,
                    target: CoverTarget::Search(t.uri.clone()),
                })
            })
            .collect();
        fx.dedup_by(|a, b| matches!((a, b), (Effect::Cover { url: x, .. }, Effect::Cover { url: y, .. }) if x == y));
        track_urls.sort();
        track_urls.dedup();
        fx.extend(track_urls.into_iter().map(|u| Effect::Cover {
            url: u.clone(),
            size: 96,
            target: CoverTarget::SearchTrack(u),
        }));
        fx
    }

    fn set_search_results(&mut self, w: &AppWindow, r: SearchResults) {
        let s = &self.search;
        // Keep thumbnails that are already loaded.
        let old_tracks: Vec<TrackData> = (0..s.tracks.row_count_())
            .filter_map(|i| s.tracks.row(i))
            .collect();
        s.tracks.set_vec(
            r.tracks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut row = search_row(i, t, &self.playing_uri);
                    if let Some(old) = self
                        .search
                        .results
                        .tracks
                        .iter()
                        .position(|o| o.uri == t.uri)
                        .and_then(|j| old_tracks.get(j))
                        .filter(|o| o.has_image)
                    {
                        row.image = old.image.clone();
                        row.has_image = true;
                    }
                    row
                })
                .collect::<Vec<_>>(),
        );
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
                format!(" {n}{}", if more { "+" } else { "" })
            }
        };
        let labels: Vec<slint::SharedString> = [
            "Всё".to_owned(),
            format!("Треки{}", count(r.tracks.len(), r.more[0])),
            format!("Альбомы{}", count(r.albums.len(), r.more[1])),
            format!("Исполнители{}", count(r.artists.len(), r.more[2])),
            format!("Плейлисты{}", count(r.playlists.len(), r.more[3])),
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

/// Replaces a list's rows, touching only the ones that changed when the length is the same:
/// a reset would re-create every tile and replay its entrance animation.
fn update_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    use slint::Model as _;
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

fn fill(field: &mut String, value: &str) {
    if field.is_empty() && !value.is_empty() {
        value.clone_into(field);
    }
}

/// Copies the fields the player left empty from a fuller track.
fn merge_track(track: &mut TrackInfo, from: &TrackItem) {
    fill(&mut track.title, &from.title);
    fill(&mut track.artists, &from.artists);
    fill(&mut track.album, &from.album);
    if track.duration_ms == 0 {
        track.duration_ms = from.duration_ms;
    }
    if track.cover_url.is_none() {
        track.cover_url.clone_from(&from.image_url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_are_declined() {
        assert_eq!(plays_label(1), "1 раз");
        assert_eq!(plays_label(3), "3 раза");
        assert_eq!(plays_label(12), "12 раз");
        assert_eq!(plays_label(22), "22 раза");
    }

    #[test]
    fn filter_ignores_case_and_yo() {
        assert!(matches("Ёлка", &norm("елк")));
        assert!(matches("Night Ferry", &norm("FERRY")));
        assert!(!matches("Day", &norm("night")));
    }
}
