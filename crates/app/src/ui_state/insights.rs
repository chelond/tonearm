//! UI state of the smart playlists, the year summary and Last.fm.

use std::rc::Rc;

use slint::{Color, Global, Image, ModelRc, VecModel};
use tonearm_core::lastfm::{self, LastFmCommand, LastFmEvent, LastFmState};
use tonearm_core::library::{CollectionKind, LibraryEvent, PageHeader, PageRequest, TrackItem};

use super::{CachedPage, CoverTarget, Effect, UiState, VecModelExt, entry_track, plays_label};
use crate::history::{self, Entry};
use crate::smart::{self, Order, Played, Rule, Source};
use crate::wrapped::{self, Summary};
use crate::{AppWindow, MonthBar, PlaylistEntry, StatRow, Theme, WrappedInfo};

/// Choices of the rule editor, in the order of its buttons.
const DAYS: [u32; 5] = [7, 30, 90, 180, 365];
const MIN_PLAYS: [u32; 5] = [0, 2, 3, 5, 10];
const LIMITS: [u32; 5] = [25, 50, 100, 250, 0];
const PLAYED: [Played; 4] = [Played::Any, Played::Never, Played::NotFor, Played::Within];
const ORDERS: [Order; 5] = [
    Order::MostPlayed,
    Order::Forgotten,
    Order::RecentlyPlayed,
    Order::Random,
    Order::Source,
];
/// Rows shown in each top list of the summary (the play button takes all of them).
const WRAPPED_ROWS: usize = 10;
const SMART_ACCENT: Color = Color::from_rgb_u8(0x12, 0x8C, 0x7E);

/// A smart page waiting for Liked Songs.
pub struct SmartFetch {
    rule: Rule,
    token: u64,
    header: Option<PageHeader>,
    tracks: Vec<TrackItem>,
}

#[derive(Default)]
pub struct WrappedRows {
    tracks: Rc<VecModel<StatRow>>,
    artists: Rc<VecModel<StatRow>>,
    albums: Rc<VecModel<StatRow>>,
    /// Cover url of each row, per list.
    urls: [Vec<Option<String>>; 3],
}

/// The models of the summary page, set once on the window.
pub fn wrapped_rows(w: &AppWindow) {
    super::with(|st| {
        w.set_wrapped_tracks(ModelRc::from(st.wrapped_rows.tracks.clone()));
        w.set_wrapped_artists(ModelRc::from(st.wrapped_rows.artists.clone()));
        w.set_wrapped_albums(ModelRc::from(st.wrapped_rows.albums.clone()));
    });
}

fn index<T: PartialEq>(list: &[T], v: &T, default: i32) -> i32 {
    list.iter()
        .position(|x| x == v)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(default)
}

fn pick<T: Copy>(list: &[T], i: i32) -> T {
    list[usize::try_from(i).unwrap_or(0).min(list.len() - 1)]
}

/// A finished play as a scrobble, if it counts by Last.fm's rule.
pub fn scrobble(e: &Entry) -> Vec<Effect> {
    let artist = wrapped::first_artist(&e.artists);
    if artist.is_empty()
        || !e.uri.starts_with("spotify:track:")
        || !lastfm::counts(e.duration_ms, e.listened_ms)
    {
        return Vec::new();
    }
    vec![Effect::LastFm(LastFmCommand::Scrobble(lastfm::Scrobble {
        artist: artist.to_owned(),
        title: e.title.clone(),
        album: e.album.clone(),
        duration_secs: e.duration_ms / 1000,
        timestamp: e.at,
    }))]
}

fn minutes_label(min: u64) -> String {
    if min >= 120 {
        format!("{} ч", min / 60)
    } else {
        format!("{min} мин")
    }
}

fn date_label(at: u64) -> String {
    const MONTHS: [&str; 12] = [
        "января",
        "февраля",
        "марта",
        "апреля",
        "мая",
        "июня",
        "июля",
        "августа",
        "сентября",
        "октября",
        "ноября",
        "декабря",
    ];
    use chrono::Datelike;
    i64::try_from(at)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|t| t.with_timezone(&chrono::Local))
        .map(|t| format!("{} {}", t.day(), MONTHS[t.month0() as usize]))
        .unwrap_or_default()
}

fn days_word(n: usize) -> &'static str {
    match (n % 10, n % 100) {
        (1, x) if x != 11 => "день",
        (2..=4, x) if !(12..=14).contains(&x) => "дня",
        _ => "дней",
    }
}

impl UiState {
    pub(super) fn init_insights(&mut self, w: &AppWindow) {
        self.smart_rules = if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
            smart::presets()
        } else {
            smart::load()
        };
        w.set_smart_playlists(ModelRc::from(self.smart_rows.clone()));
        self.refresh_smart_sidebar();
    }

    fn refresh_smart_sidebar(&self) {
        let rows: Vec<PlaylistEntry> = self
            .smart_rules
            .iter()
            .map(|r| PlaylistEntry {
                uri: smart::uri(&r.id).into(),
                name: r.name.clone().into(),
                count: 0,
                owner: "Умный плейлист".into(),
                image: Image::default(),
                has_image: false,
            })
            .collect();
        super::sync_rows(&self.smart_rows, rows);
    }

    fn sample_plays(&self) -> Option<Vec<Entry>> {
        std::env::var_os("TONEARM_UI_PREVIEW").map(|_| self.store.entries.clone())
    }

    /// Opens a smart playlist page: the header now, the tracks once the rule has run.
    pub(super) fn open_smart(&mut self, w: &AppWindow, id: &str) -> Vec<Effect> {
        self.smart_seq += 1;
        let Some(rule) = self.smart_rules.iter().find(|r| r.id == id).cloned() else {
            w.set_page_loading(false);
            w.set_page_error("Этого умного плейлиста больше нет".into());
            return Vec::new();
        };
        self.page_kind = Some(CollectionKind::Playlist);
        self.page_title.clone_from(&rule.name);
        w.set_page_kind(1);
        w.set_page_smart(true);
        w.set_page_title(rule.name.clone().into());
        w.set_page_byline(rule.describe().into());
        w.set_coll_scroll(0.0);
        Theme::get(w).set_page_accent(SMART_ACCENT);
        let token = self.smart_seq;
        if rule.needs_liked() {
            let key = (0, "liked".to_owned());
            if let Some(c) = self.page_cache.iter().find(|c| c.key == key) {
                return vec![Effect::Smart {
                    token,
                    rule: Box::new(rule),
                    liked: c.tracks.clone(),
                    plays: self.sample_plays(),
                }];
            }
            self.smart_fetch = Some(SmartFetch {
                rule,
                token,
                header: None,
                tracks: Vec::new(),
            });
            return vec![Effect::Open(PageRequest::Liked)];
        }
        vec![Effect::Smart {
            token,
            rule: Box::new(rule),
            liked: Vec::new(),
            plays: self.sample_plays(),
        }]
    }

    /// Liked Songs loading for a smart page: collected here instead of shown.
    pub(super) fn smart_liked(
        &mut self,
        w: &AppWindow,
        ev: LibraryEvent,
    ) -> Result<Vec<Effect>, Box<LibraryEvent>> {
        let min = self.min_page_id;
        let Some(f) = self.smart_fetch.as_mut() else {
            return Err(Box::new(ev));
        };
        match ev {
            LibraryEvent::PageHeader { page_id, header }
                if page_id >= min && header.kind == CollectionKind::Liked =>
            {
                self.page_id = page_id;
                f.header = Some(header);
                Ok(Vec::new())
            }
            LibraryEvent::PageTracks {
                page_id,
                tracks,
                done,
            } if page_id == self.page_id && page_id >= min => {
                f.tracks.extend(tracks);
                if !done {
                    return Ok(Vec::new());
                }
                let Some(f) = self.smart_fetch.take() else {
                    return Ok(Vec::new());
                };
                // Kept like a visited Liked page: the next smart page or the Liked page itself
                // shows at once.
                self.push_cache(CachedPage {
                    key: (0, "liked".to_owned()),
                    header: f.header,
                    image_url: None,
                    rgb: None,
                    tracks: f.tracks.clone(),
                    tiles: Vec::new(),
                    scroll: 0.0,
                });
                Ok(vec![Effect::Smart {
                    token: f.token,
                    rule: Box::new(f.rule),
                    liked: f.tracks,
                    plays: self.sample_plays(),
                }])
            }
            LibraryEvent::PageImage { page_id, .. } | LibraryEvent::PageTiles { page_id, .. }
                if page_id >= min =>
            {
                Ok(Vec::new())
            }
            LibraryEvent::PageError { page_id, message } if page_id >= min => {
                self.smart_fetch = None;
                w.set_page_loading(false);
                w.set_page_error(message.into());
                Ok(Vec::new())
            }
            other => Err(Box::new(other)),
        }
    }

    /// The rule has run: show what it picked.
    pub fn on_smart(&mut self, w: &AppWindow, token: u64, tracks: Vec<TrackItem>) -> Vec<Effect> {
        let Some(id) = smart::id_of(&self.page_uri).map(str::to_owned) else {
            return Vec::new();
        };
        if token != self.smart_seq {
            return Vec::new();
        }
        let empty = tracks.is_empty();
        self.tracks = tracks;
        self.rebuild_view(w);
        self.finish_tracks(w);
        w.set_page_loading(false);
        if empty {
            let rule = self.smart_rules.iter().find(|r| r.id == id);
            let hint = match rule.map(|r| (r.source, r.played)) {
                Some((Source::Liked, Played::Never)) => {
                    "Вы уже слушали все любимые треки."
                }
                Some((Source::Liked, Played::NotFor)) => {
                    "История Tonearm ведётся с первого запуска. Плейлист заполнится, когда накопятся прослушивания."
                }
                _ => {
                    "Под эти правила пока ничего не подходит. Послушайте музыку или измените \
                     правила."
                }
            };
            w.set_page_empty_hint(hint.into());
        }
        if std::mem::take(&mut self.autoplay) {
            let fx = self.play_page(false);
            self.mark_context(w);
            return fx;
        }
        Vec::new()
    }

    fn show_editor(&self, w: &AppWindow, rule: &Rule, new: bool) {
        w.set_ed_new(new);
        w.set_ed_name(rule.name.clone().into());
        w.set_ed_source(i32::from(rule.source == Source::History));
        w.set_ed_played(index(&PLAYED, &rule.played, 0));
        w.set_ed_days(index(&DAYS, &rule.days, 1));
        w.set_ed_min(index(&MIN_PLAYS, &rule.min_plays, 0));
        w.set_ed_order(index(&ORDERS, &rule.order, 0));
        w.set_ed_limit(index(&LIMITS, &rule.limit, 2));
        w.set_smart_editor(true);
    }

    pub fn smart_new(&mut self, w: &AppWindow) {
        self.smart_editing = None;
        self.show_editor(w, &Rule::default(), true);
    }

    /// "Правила" on a smart page.
    pub fn smart_edit(&mut self, w: &AppWindow) {
        let Some(id) = smart::id_of(&self.page_uri).map(str::to_owned) else {
            return;
        };
        let Some(rule) = self.smart_rules.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        self.smart_editing = Some(id);
        self.show_editor(w, &rule, false);
    }

    fn read_editor(w: &AppWindow, id: String) -> Rule {
        let name = w.get_ed_name().trim().to_owned();
        Rule {
            id,
            name: if name.is_empty() {
                "Умный плейлист".into()
            } else {
                name
            },
            source: if w.get_ed_source() == 1 {
                Source::History
            } else {
                Source::Liked
            },
            played: pick(&PLAYED, w.get_ed_played()),
            days: pick(&DAYS, w.get_ed_days()),
            min_plays: pick(&MIN_PLAYS, w.get_ed_min()),
            order: pick(&ORDERS, w.get_ed_order()),
            limit: pick(&LIMITS, w.get_ed_limit()),
        }
    }

    pub fn smart_save(&mut self, w: &AppWindow) -> Vec<Effect> {
        w.set_smart_editor(false);
        let editing = self.smart_editing.take();
        let id = editing
            .clone()
            .unwrap_or_else(|| smart::new_id(&self.smart_rules, history::now()));
        let rule = Self::read_editor(w, id.clone());
        match self.smart_rules.iter_mut().find(|r| r.id == id) {
            Some(r) => *r = rule,
            None => self.smart_rules.push(rule),
        }
        self.refresh_smart_sidebar();
        let mut fx = vec![Effect::SaveRules(self.smart_rules.clone())];
        // Show the result: the new playlist, or the edited one again.
        let req = PageRequest::Playlist(smart::uri(&id));
        fx.extend(self.open(w, req, editing.is_none()));
        fx
    }

    pub fn smart_delete(&mut self, w: &AppWindow) -> Vec<Effect> {
        w.set_smart_editor(false);
        let Some(id) = self.smart_editing.take() else {
            return Vec::new();
        };
        self.smart_rules.retain(|r| r.id != id);
        self.refresh_smart_sidebar();
        if smart::id_of(&self.page_uri) == Some(id.as_str()) {
            self.current = None;
            w.set_page(crate::Page::Library);
        }
        vec![Effect::SaveRules(self.smart_rules.clone())]
    }

    /// "Now playing" for Last.fm, once per track and only with an artist to name.
    pub(super) fn now_playing(&mut self) -> Vec<Effect> {
        let t = &self.playing;
        if t.uri.is_empty() || t.artists.is_empty() || self.np_sent == t.uri {
            return Vec::new();
        }
        self.np_sent.clone_from(&t.uri);
        vec![Effect::LastFm(LastFmCommand::NowPlaying(lastfm::Track {
            artist: wrapped::first_artist(&t.artists).to_owned(),
            title: t.title.clone(),
            album: t.album.clone(),
            duration_secs: t.duration_ms / 1000,
        }))]
    }

    pub fn on_lastfm(&mut self, w: &AppWindow, ev: LastFmEvent) {
        match ev {
            LastFmEvent::State(state) => {
                let (code, user) = match state {
                    LastFmState::Unavailable => (0, String::new()),
                    LastFmState::SignedOut => (1, String::new()),
                    LastFmState::Waiting => (2, String::new()),
                    LastFmState::SignedIn { user } => (3, user),
                };
                w.set_lastfm_state(code);
                w.set_lastfm_user(user.into());
                self.lastfm_on = code == 3 && w.get_set_scrobble() == 1;
            }
            LastFmEvent::Pending(n) => w.set_lastfm_pending(i32::try_from(n).unwrap_or(i32::MAX)),
            LastFmEvent::OpenUrl(url) => {
                if let Err(e) = open::that_detached(&url) {
                    tracing::warn!(error = %e, "cannot open the browser");
                    w.set_notice("Не открылся браузер для входа в Last.fm".into());
                    w.set_notice_is_error(true);
                }
            }
            LastFmEvent::Error(m) => {
                w.set_notice(m.into());
                w.set_notice_is_error(true);
            }
        }
    }

    // ───── year summary ─────

    /// The summary page opened, or another year picked (`index` into the year chips).
    pub fn wrapped_opened(&mut self, w: &AppWindow, index: i32) -> Vec<Effect> {
        let year = usize::try_from(index)
            .ok()
            .and_then(|i| slint::Model::row_data(&w.get_wrapped_years(), i))
            .and_then(|y| y.parse().ok())
            .unwrap_or(0);
        w.set_wrapped_loading(true);
        vec![Effect::Wrapped {
            year,
            plays: self.sample_plays(),
        }]
    }

    pub fn on_wrapped(&mut self, w: &AppWindow, s: &Summary, years: &[i32]) -> Vec<Effect> {
        w.set_wrapped_loading(false);
        let labels: Vec<slint::SharedString> = years.iter().map(|y| y.to_string().into()).collect();
        w.set_wrapped_year_index(index(years, &s.year, 0));
        w.set_wrapped_years(ModelRc::new(VecModel::from(labels)));
        let top_month = (0..12)
            .filter(|&m| s.months[m] > 0)
            .max_by_key(|&m| s.months[m]);
        let first = s.first.as_ref();
        w.set_wrapped(WrappedInfo {
            year: s.year.to_string().into(),
            empty: s.plays == 0,
            hours: (s.minutes / 60).to_string().into(),
            minutes: s.minutes.to_string().into(),
            plays: s.plays.to_string().into(),
            tracks: s.tracks.to_string().into(),
            artists: s.artists.to_string().into(),
            days: format!("{} {}", s.days, days_word(s.days)).into(),
            streak: format!("{} {}", s.streak, days_word(s.streak)).into(),
            top_month: top_month
                .map(|m| wrapped::MONTHS[m].to_owned())
                .unwrap_or_default()
                .into(),
            time_of_day: s
                .top_hour
                .map(|h| format!("{}, около {h}:00", wrapped::part_of_day(h)))
                .unwrap_or_default()
                .into(),
            first_title: first.map(|e| e.title.clone()).unwrap_or_default().into(),
            first_artist: first.map(|e| e.artists.clone()).unwrap_or_default().into(),
            first_date: first.map(|e| date_label(e.at)).unwrap_or_default().into(),
            top_artist: s
                .top_artists
                .first()
                .map(|a| a.title.clone())
                .unwrap_or_default()
                .into(),
            top_artist_minutes: s
                .top_artists
                .first()
                .map(|a| minutes_label(a.minutes))
                .unwrap_or_default()
                .into(),
        });
        let max_month = s.months.iter().copied().max().unwrap_or(0).max(1);
        #[allow(clippy::cast_precision_loss)]
        let bars: Vec<MonthBar> = s
            .months
            .iter()
            .enumerate()
            .map(|(m, &min)| MonthBar {
                label: wrapped::MONTHS[m]
                    .chars()
                    .take(3)
                    .collect::<String>()
                    .into(),
                value: minutes_label(min).into(),
                share: min as f32 / max_month as f32,
                top: Some(m) == top_month,
            })
            .collect();
        w.set_wrapped_months(ModelRc::new(VecModel::from(bars)));

        self.wrapped_items = s.top_tracks.iter().map(|r| r.entry.clone()).collect();
        let mut fx = Vec::new();
        let mut urls: Vec<String> = Vec::new();
        let lists = [&s.top_tracks, &s.top_artists, &s.top_albums];
        let models = [
            self.wrapped_rows.tracks.clone(),
            self.wrapped_rows.artists.clone(),
            self.wrapped_rows.albums.clone(),
        ];
        for (n, (list, model)) in lists.iter().zip(models).enumerate() {
            let shown: Vec<&wrapped::Ranked> = list.iter().take(WRAPPED_ROWS).collect();
            let max = shown.first().map_or(1, |r| r.plays.max(1));
            #[allow(clippy::cast_precision_loss)]
            let rows: Vec<StatRow> = shown
                .iter()
                .map(|r| StatRow {
                    title: r.title.clone().into(),
                    subtitle: if n == 1 {
                        minutes_label(r.minutes).into()
                    } else {
                        r.subtitle.clone().into()
                    },
                    value: plays_label(r.plays).into(),
                    share: r.plays as f32 / max as f32,
                    uri: r.uri.clone().unwrap_or_default().into(),
                    image: Image::default(),
                    has_image: false,
                })
                .collect();
            model.set_vec(rows);
            self.wrapped_rows.urls[n] = shown.iter().map(|r| r.entry.image_url.clone()).collect();
            urls.extend(shown.iter().filter_map(|r| r.entry.image_url.clone()));
        }
        urls.sort();
        urls.dedup();
        fx.extend(urls.into_iter().map(|u| Effect::Cover {
            url: u.clone(),
            size: 160,
            target: CoverTarget::Wrapped(u),
        }));
        fx
    }

    pub(super) fn wrapped_cover(&self, url: &str, image: &Image) {
        let models = [
            &self.wrapped_rows.tracks,
            &self.wrapped_rows.artists,
            &self.wrapped_rows.albums,
        ];
        for (urls, model) in self.wrapped_rows.urls.iter().zip(models) {
            for (i, u) in urls.iter().enumerate() {
                if u.as_deref() == Some(url)
                    && let Some(mut row) = model.row(i)
                {
                    row.image = image.clone();
                    row.has_image = true;
                    model.set_row(i, row);
                }
            }
        }
    }

    /// Plays the year's top tracks from `index` (-1: from the first).
    pub fn wrapped_play(&mut self, index: i32) -> Vec<Effect> {
        let tracks: Vec<TrackItem> = self.wrapped_items.iter().map(entry_track).collect();
        let start = usize::try_from(index).ok();
        let name = "Топ года".to_owned();
        let shuffle = start.is_none().then_some(false);
        self.start_context(&tracks, start, &name, shuffle, history::Context::default())
    }

    /// A year of sample plays for the preview of the summary page.
    #[cfg(debug_assertions)]
    pub fn preview_year(&mut self) {
        use chrono::TimeZone;
        let tracks = [
            ("Огни над рекой", "Северный Ветер", "Ночные маршруты"),
            ("Тёплый асфальт", "Лиса Мороз", "Город летом"),
            ("Half Light", "Kite Harbor", "Paper Boats"),
            ("Шум прибоя", "Тихий Порт", "Маяк"),
            ("Северное сияние", "Мира Лес", "Сезоны"),
            ("Signal Lost", "Neon Fjord", "Депо"),
            ("Метро в 6:10", "Лиса Мороз", "Город летом"),
            ("Последний трамвай", "Пыль и Медь", "Холодный свет"),
        ];
        let year = history::year_of(history::now());
        let start = chrono::Local
            .with_ymd_and_hms(year, 1, 1, 8, 0, 0)
            .single()
            .map_or(0, |t| u64::try_from(t.timestamp()).unwrap_or(0));
        let now = history::now();
        let mut at = start;
        let mut n = 0usize;
        while at < now && self.store.entries.len() < 2900 {
            // Busier in spring and late autumn, a few quiet days.
            n += 1;
            let i = (n * 7 + n / 5) % tracks.len();
            let pick = if n.is_multiple_of(3) { 0 } else { i.min(n % 5) };
            let (t, a, al) = tracks[pick];
            self.store.add(Entry {
                uri: format!("spotify:track:y{pick}"),
                title: t.into(),
                artists: a.into(),
                artist_uri: Some(format!("spotify:artist:{a}")),
                album: al.into(),
                album_uri: Some(format!("spotify:album:{al}")),
                image_url: Some(format!("preview:{al}")),
                duration_ms: 210_000,
                listened_ms: 200_000,
                at,
                context: history::Context::default(),
            });
            let day = 24 * 3600;
            at += if n.is_multiple_of(9) {
                day
            } else {
                3 * 3600 + (n as u64 % 4) * 1800
            };
        }
    }
}
