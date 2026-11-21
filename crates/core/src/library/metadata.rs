//! Artist pages through librespot-metadata: the Web API no longer serves artist top tracks to
//! development-mode apps, while the internal metadata endpoints do.

use futures_util::stream::{self, StreamExt};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::image::Images;
use librespot_metadata::{Album, Artist, Metadata, Track};

use super::model::{CollectionKind, PageHeader, Tile, TrackItem};

const TOP_TRACKS: usize = 10;
const MAX_ALBUMS: usize = 40;
const CONCURRENCY: usize = 8;

#[derive(Debug, thiserror::Error)]
#[error("не удалось загрузить данные артиста: {0}")]
pub struct MetadataError(String);

pub(super) fn image_url(images: &Images) -> Option<String> {
    images
        .iter()
        .max_by_key(|i| i.width)
        .and_then(|i| i.id.to_base16().ok())
        .map(|hex| format!("https://i.scdn.co/image/{hex}"))
}

/// Cover closest to 300 px wide (the 300 px rendition is what rows and the player bar need).
pub(super) fn image_url_small(images: &Images) -> Option<String> {
    images
        .iter()
        .min_by_key(|i| (i.width - 300).abs())
        .and_then(|i| i.id.to_base16().ok())
        .map(|hex| format!("https://i.scdn.co/image/{hex}"))
}

fn uri_string(uri: &SpotifyUri) -> String {
    uri.to_uri().unwrap_or_default()
}

pub(super) fn track_item(t: &Track) -> TrackItem {
    TrackItem {
        uri: uri_string(&t.id),
        title: t.name.clone(),
        artists: t
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        artist_uri: t.artists.first().map(|a| uri_string(&a.id)),
        album: t.album.name.clone(),
        album_uri: Some(uri_string(&t.album.id)),
        duration_ms: u32::try_from(t.duration).unwrap_or(0),
        explicit: t.is_explicit,
        playable: true,
        disc_number: u32::try_from(t.disc_number).unwrap_or(0),
        track_number: u32::try_from(t.number).unwrap_or(0),
        image_url: image_url_small(&t.album.covers),
    }
}

pub struct ArtistPage {
    pub header: PageHeader,
    pub top_tracks: Vec<TrackItem>,
    pub albums: Vec<Tile>,
}

pub async fn artist(session: &Session, uri: &str) -> Result<ArtistPage, MetadataError> {
    let id = SpotifyUri::from_uri(uri).map_err(|e| MetadataError(e.to_string()))?;
    let artist = Artist::get(session, &id)
        .await
        .map_err(|e| MetadataError(e.to_string()))?;

    let top_ids = artist.top_tracks.for_country(&session.country());
    let top_tracks: Vec<TrackItem> = stream::iter(top_ids.iter().take(TOP_TRACKS).cloned())
        .map(|tid| async move { Track::get(session, &tid).await.ok() })
        .buffered(CONCURRENCY)
        .filter_map(|t| async move { t.as_ref().map(track_item) })
        .collect()
        .await;

    // Each group holds releases of one album; the first one is enough for the discography.
    let album_ids: Vec<SpotifyUri> = artist
        .albums
        .iter()
        .chain(artist.singles.iter())
        .filter_map(|group| group.first().cloned())
        .take(MAX_ALBUMS)
        .collect();
    let mut albums: Vec<(i32, Tile)> = stream::iter(album_ids)
        .map(|aid| async move { Album::get(session, &aid).await.ok() })
        .buffered(CONCURRENCY)
        .filter_map(|a| async move {
            let a = a?;
            let year = a.date.0.year();
            Some((
                year,
                Tile {
                    uri: uri_string(&a.id),
                    kind: CollectionKind::Album,
                    title: a.name.clone(),
                    subtitle: year.to_string(),
                    image_url: image_url(&a.covers),
                },
            ))
        })
        .collect()
        .await;
    albums.sort_by_key(|a| std::cmp::Reverse(a.0));

    Ok(ArtistPage {
        header: PageHeader {
            kind: CollectionKind::Artist,
            uri: uri.to_owned(),
            title: artist.name.clone(),
            byline: String::new(),
            byline_uri: None,
            year: None,
            total_tracks: None,
            image_url: image_url(&artist.portraits),
        },
        top_tracks,
        albums: albums.into_iter().map(|(_, t)| t).collect(),
    })
}
