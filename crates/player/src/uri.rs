//! Parsing what the user pastes: `spotify:track:…` URIs or `open.spotify.com` links.

/// Kinds of items that can be played directly in stage 3.
const PLAYABLE: &[&str] = &["track", "episode"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UriError {
    #[error("это не ссылка и не URI Spotify")]
    NotSpotify,
    #[error("пока можно включить только трек или эпизод, а это «{0}»")]
    NotPlayable(String),
}

/// Normalises user input to `spotify:{kind}:{id}` for a playable item.
pub fn parse_playable(input: &str) -> Result<String, UriError> {
    let (kind, uri) = parse_link(input)?;
    if !PLAYABLE.contains(&kind.as_str()) {
        return Err(UriError::NotPlayable(kind));
    }
    Ok(uri)
}

/// Any Spotify URI or `open.spotify.com` link → (`kind`, `spotify:{kind}:{id}`), e.g. for
/// opening an album or playlist pasted into search.
pub fn parse_link(input: &str) -> Result<(String, String), UriError> {
    let input = input.trim();
    let (kind, id) = if let Some(rest) = input.strip_prefix("spotify:") {
        let mut parts = rest.split(':');
        (parts.next(), parts.next())
    } else {
        let rest = input
            .strip_prefix("https://")
            .or_else(|| input.strip_prefix("http://"))
            .unwrap_or(input);
        let path = rest
            .strip_prefix("open.spotify.com/")
            .ok_or(UriError::NotSpotify)?;
        // Drop the query (`?si=…`) and an optional locale segment (`intl-ru/`).
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let mut parts = path
            .split('/')
            .filter(|p| !p.is_empty() && !p.starts_with("intl-"));
        (parts.next(), parts.next())
    };
    let (Some(kind), Some(id)) = (kind, id) else {
        return Err(UriError::NotSpotify);
    };
    if !is_base62_id(id) {
        return Err(UriError::NotSpotify);
    }
    Ok((kind.to_owned(), format!("spotify:{kind}:{id}")))
}

fn is_base62_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "4uLU6hMCjMI75M1A2tKUQC";

    #[test]
    fn accepts_uri_and_links() {
        let want = format!("spotify:track:{ID}");
        assert_eq!(parse_playable(&want).unwrap(), want);
        assert_eq!(
            parse_playable(&format!("https://open.spotify.com/track/{ID}?si=abc")).unwrap(),
            want
        );
        assert_eq!(
            parse_playable(&format!(" open.spotify.com/intl-ru/track/{ID} ")).unwrap(),
            want
        );
    }

    #[test]
    fn links_to_collections() {
        assert_eq!(
            parse_link(&format!("https://open.spotify.com/playlist/{ID}?si=1")).unwrap(),
            ("playlist".into(), format!("spotify:playlist:{ID}"))
        );
        assert!(parse_link("ночные маршруты").is_err());
    }

    #[test]
    fn rejects_garbage_and_non_playable() {
        assert_eq!(parse_playable("hello"), Err(UriError::NotSpotify));
        assert_eq!(
            parse_playable("spotify:track:short"),
            Err(UriError::NotSpotify)
        );
        assert_eq!(
            parse_playable(&format!("https://open.spotify.com/album/{ID}")),
            Err(UriError::NotPlayable("album".into()))
        );
    }
}
