//! Small HTTP helpers shared by the services.

use std::time::Duration;

/// Response size caps: generous for real answers, small enough that a broken or hostile server
/// cannot make the app buffer hundreds of megabytes.
pub const MAX_JSON: usize = 4 * 1024 * 1024;
/// Web API pages (playlists of a few thousand tracks fit easily).
pub const MAX_API: usize = 16 * 1024 * 1024;
/// Cover images (Spotify's largest are well under 1 MB).
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;

/// Why a body could not be read.
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("ответ сервера слишком большой")]
    TooLarge,
    #[error("сервер перестал отвечать")]
    Stalled,
    #[error("{0}")]
    Network(String),
}

/// Reads the whole body, refusing more than `max` bytes and a server that goes silent for
/// `stall` between chunks.
pub async fn body_limited(
    mut resp: reqwest::Response,
    max: usize,
    stall: Duration,
) -> Result<Vec<u8>, BodyError> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(BodyError::TooLarge);
    }
    let mut data = Vec::new();
    loop {
        let chunk = tokio::time::timeout(stall, resp.chunk())
            .await
            .map_err(|_| BodyError::Stalled)?
            .map_err(|e| BodyError::Network(e.without_url().to_string()))?;
        let Some(chunk) = chunk else { break };
        if data.len() + chunk.len() > max {
            return Err(BodyError::TooLarge);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// Same, parsed as JSON (`None` on any failure; callers treat it like "no answer").
pub async fn json_limited(resp: reqwest::Response, max: usize) -> Option<serde_json::Value> {
    let bytes = body_limited(resp, max, Duration::from_secs(15))
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
