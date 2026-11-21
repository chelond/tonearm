//! Data the library shows. Plain types, no librespot or JSON details leak to the UI.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackItem {
    /// `spotify:track:…`
    pub uri: String,
    pub title: String,
    pub artists: String,
    /// First artist, to open the artist page from a row.
    pub artist_uri: Option<String>,
    pub album: String,
    pub album_uri: Option<String>,
    pub duration_ms: u32,
    pub explicit: bool,
    /// Local files and region-blocked tracks cannot be played.
    pub playable: bool,
    /// Disc and track number for album sleeves (A1, B3…).
    pub disc_number: u32,
    pub track_number: u32,
    /// Album cover (about 300 px), for row thumbnails and the player bar.
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlaylistSummary {
    /// `spotify:playlist:…`
    pub uri: String,
    pub name: String,
    pub owner: String,
    pub track_count: u32,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Liked,
    Playlist,
    Album,
    Artist,
}

/// An item of the home grid or an artist discography.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub uri: String,
    pub kind: CollectionKind,
    pub title: String,
    pub subtitle: String,
    pub image_url: Option<String>,
}

/// Header of a playlist / album / artist page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageHeader {
    pub kind: CollectionKind,
    pub uri: String,
    pub title: String,
    /// Owner of a playlist or artists of an album.
    pub byline: String,
    pub byline_uri: Option<String>,
    /// Release year of an album.
    pub year: Option<u32>,
    /// Total tracks if known before all pages are loaded.
    pub total_tracks: Option<u32>,
    pub image_url: Option<String>,
}

/// What to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRequest {
    Liked,
    Playlist(String),
    Album(String),
    Artist(String),
}

/// Search result types, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchKind {
    Track,
    Album,
    Artist,
    Playlist,
}

impl SearchKind {
    pub const ALL: [Self; 4] = [Self::Track, Self::Album, Self::Artist, Self::Playlist];

    /// Value of the Web API `type` parameter.
    #[must_use]
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
        }
    }

    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Track => 0,
            Self::Album => 1,
            Self::Artist => 2,
            Self::Playlist => 3,
        }
    }
}

/// One page of search results (Web API returns at most 10 per type since February 2026).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResults {
    pub tracks: Vec<TrackItem>,
    pub albums: Vec<Tile>,
    pub artists: Vec<Tile>,
    pub playlists: Vec<Tile>,
    /// Whether Spotify has more results of each type, in [`SearchKind`] order.
    pub more: [bool; 4],
}

impl SearchResults {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
            && self.albums.is_empty()
            && self.artists.is_empty()
            && self.playlists.is_empty()
    }

    /// Appends a further page of the same query.
    pub fn extend(&mut self, other: Self) {
        self.tracks.extend(other.tracks);
        self.albums.extend(other.albums);
        self.artists.extend(other.artists);
        self.playlists.extend(other.playlists);
        self.more = other.more;
    }

    /// "Best result" heuristic (the Web API has none): an artist or album named exactly like the
    /// query, then one whose name starts with it, then the album of the first track.
    #[must_use]
    pub fn best(&self, query: &str) -> Option<Tile> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return None;
        }
        let candidates = || self.artists.iter().chain(self.albums.iter());
        candidates()
            .find(|t| t.title.to_lowercase() == q)
            .or_else(|| candidates().find(|t| t.title.to_lowercase().starts_with(&q)))
            .cloned()
            .or_else(|| {
                let t = self.tracks.first()?;
                Some(Tile {
                    uri: t.album_uri.clone()?,
                    kind: CollectionKind::Album,
                    title: t.album.clone(),
                    subtitle: t.artists.clone(),
                    image_url: None,
                })
            })
            .or_else(|| candidates().next().cloned())
    }
}

/// `spotify:playlist:ID` → `ID` (also accepts a bare id).
#[must_use]
pub fn uri_id(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or(uri)
}

/// Sleeve code for a track: playlists 001…, albums A1/B2 by side (two discs = sides A/B, C/D…).
#[must_use]
pub fn sleeve_code(kind: CollectionKind, index: usize, track: &TrackItem) -> String {
    match kind {
        CollectionKind::Album if track.track_number > 0 => {
            let disc = track.disc_number.max(1) - 1;
            let side = char::from(b'A' + u8::try_from(disc % 26).unwrap_or(0));
            format!("{side}{}", track.track_number)
        }
        _ => format!("{:03}", index + 1),
    }
}

/// `h:mm:ss` or `m:ss`.
#[must_use]
pub fn format_duration(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_durations() {
        let t = TrackItem {
            disc_number: 2,
            track_number: 3,
            ..TrackItem::default()
        };
        assert_eq!(sleeve_code(CollectionKind::Album, 0, &t), "B3");
        assert_eq!(sleeve_code(CollectionKind::Playlist, 6, &t), "007");
        assert_eq!(format_duration(221_000), "3:41");
        assert_eq!(format_duration(47_112_000), "13:05:12");
        assert_eq!(uri_id("spotify:playlist:abc"), "abc");
    }

    fn tile(kind: CollectionKind, title: &str) -> Tile {
        Tile {
            uri: format!("spotify:x:{title}"),
            kind,
            title: title.into(),
            subtitle: String::new(),
            image_url: None,
        }
    }

    #[test]
    fn best_result_prefers_exact_then_prefix_then_track_album() {
        let mut r = SearchResults {
            artists: vec![tile(CollectionKind::Artist, "Ночные Огни")],
            albums: vec![tile(CollectionKind::Album, "Ночь")],
            ..SearchResults::default()
        };
        assert_eq!(r.best("ночь").unwrap().title, "Ночь");
        assert_eq!(r.best("Ночн").unwrap().title, "Ночные Огни");
        r.tracks.push(TrackItem {
            album: "Маяк".into(),
            album_uri: Some("spotify:album:m".into()),
            ..TrackItem::default()
        });
        assert_eq!(r.best("zzz").unwrap().title, "Маяк");
        assert!(r.best("  ").is_none());
    }
}
