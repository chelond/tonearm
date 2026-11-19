//! Signs Soloist in to the user's account from Tonearm itself, without the official Spotify app.
//!
//! Soloist logs in only through Spotify Connect: a controller finds it on the local network
//! (mDNS `_spotify-connect._tcp`) and hands it the account with the ZeroConf `addUser` call.
//! Spotify's apps do exactly that when the device is picked in their list. Tonearm is already
//! signed in, so it can make the same call with its own reusable credentials: the blob is the
//! format librespot's discovery server decodes (`Credentials::with_blob`), encrypted with a
//! Diffie-Hellman key agreed with the device. Everything stays on this computer.

use std::net::IpAddr;
use std::time::Duration;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use librespot_core::diffie_hellman::DhLocalKeys;
use sha1::{Digest, Sha1};

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const SERVICE: &str = "_spotify-connect._tcp.local.";
/// `AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS`: what a librespot session holds after login.
pub const STORED_CREDENTIALS: u32 = 1;
/// `AUTHENTICATION_SPOTIFY_TOKEN`: an OAuth access token.
pub const SPOTIFY_TOKEN: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("Soloist не найден в локальной сети (mDNS)")]
    NotFound,
    #[error("Soloist просит другой способ входа ({0})")]
    TokenType(String),
    #[error("Soloist отклонил вход: {0}")]
    Refused(String),
    #[error("{0}")]
    Other(String),
}

/// The device's ZeroConf endpoint.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
}

/// Finds the Soloist that advertises `device_name`, preferring this computer's addresses.
pub async fn find(device_name: &str, wait: Duration) -> Result<Endpoint, PairError> {
    let name = device_name.to_owned();
    let candidates = tokio::task::spawn_blocking(move || browse(wait))
        .await
        .map_err(|e| PairError::Other(e.to_string()))??;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .map_err(|e| PairError::Other(e.to_string()))?;
    for url in candidates {
        let Ok(info) = get_info(&http, &url).await else {
            continue;
        };
        if info["remoteName"].as_str() == Some(name.as_str()) {
            return Ok(Endpoint { url });
        }
    }
    Err(PairError::NotFound)
}

/// Endpoint URLs of every Spotify Connect device that answers within `wait`, loopback first.
fn browse(wait: Duration) -> Result<Vec<String>, PairError> {
    let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| PairError::Other(e.to_string()))?;
    let events = daemon
        .browse(SERVICE)
        .map_err(|e| PairError::Other(e.to_string()))?;
    let deadline = std::time::Instant::now() + wait;
    let mut found = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match events.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(service)) => {
                let path = service
                    .txt_properties
                    .get_property_val_str("CPath")
                    .unwrap_or("/")
                    .to_owned();
                let mut addrs: Vec<IpAddr> =
                    service.addresses.iter().map(|a| a.to_ip_addr()).collect();
                // Soloist runs here: try our own addresses before the rest of the network.
                addrs.sort_by_key(|a| (!a.is_loopback(), a.is_ipv6()));
                for addr in addrs {
                    let host = match addr {
                        IpAddr::V4(v4) => v4.to_string(),
                        IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80 => continue,
                        IpAddr::V6(v6) => format!("[{v6}]"),
                    };
                    found.push(endpoint_url(&host, service.port, &path));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}

fn endpoint_url(host: &str, port: u16, path: &str) -> String {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    format!("http://{host}:{port}{path}")
}

async fn get_info(http: &reqwest::Client, url: &str) -> Result<serde_json::Value, PairError> {
    let resp = http
        .get(url)
        .query(&[("action", "getInfo")])
        .send()
        .await
        .map_err(|e| PairError::Other(e.to_string()))?;
    resp.json()
        .await
        .map_err(|e| PairError::Other(e.to_string()))
}

/// Hands the account to the device at `endpoint`.
pub async fn add_user(
    endpoint: &Endpoint,
    username: &str,
    auth_type: u32,
    auth_data: &[u8],
) -> Result<(), PairError> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|e| PairError::Other(e.to_string()))?;
    let info = get_info(&http, &endpoint.url).await?;
    let token_type = info["tokenType"].as_str().unwrap_or("default");
    tracing::info!(
        token_type,
        version = info["version"].as_str().unwrap_or(""),
        "soloist zeroconf info"
    );
    if token_type != "default" {
        return Err(PairError::TokenType(token_type.to_owned()));
    }
    let device_id = info["deviceID"]
        .as_str()
        .ok_or_else(|| PairError::Other("getInfo без deviceID".into()))?;
    let public_key = BASE64
        .decode(info["publicKey"].as_str().unwrap_or_default())
        .map_err(|e| PairError::Other(e.to_string()))?;

    let keys = DhLocalKeys::random(&mut rand::rng());
    let inner = credentials_blob(username, auth_type, auth_data, device_id);
    let mut iv = [0u8; 16];
    rand::Rng::fill(&mut rand::rng(), &mut iv);
    let blob = outer_blob(&keys.shared_secret(&public_key), &iv, inner.as_bytes());
    let client_key = BASE64.encode(keys.public_key());

    let body = [
        ("action", "addUser"),
        ("userName", username),
        ("blob", blob.as_str()),
        ("clientKey", client_key.as_str()),
        ("tokenType", "default"),
        ("deviceName", "Tonearm"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", form_encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let resp = http
        .post(&endpoint.url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| PairError::Other(e.to_string()))?;
    let answer: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| PairError::Other(e.to_string()))?;
    if answer["status"].as_i64() == Some(101) {
        Ok(())
    } else {
        Err(PairError::Refused(
            answer["statusString"].as_str().unwrap_or("?").to_owned(),
        ))
    }
}

fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn write_int(out: &mut Vec<u8>, n: u32) {
    // The reader takes one byte below 0x80, otherwise two (7 low bits + the rest).
    if n < 0x80 {
        out.push(u8::try_from(n).unwrap_or(0));
    } else {
        out.push(u8::try_from(n & 0x7f).unwrap_or(0) | 0x80);
        out.push(u8::try_from(n >> 7).unwrap_or(u8::MAX));
    }
}

fn write_bytes(out: &mut Vec<u8>, data: &[u8]) {
    write_int(out, u32::try_from(data.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(data);
}

/// The inner credentials blob (base64), the inverse of librespot's `Credentials::with_blob`.
fn credentials_blob(username: &str, auth_type: u32, auth_data: &[u8], device_id: &str) -> String {
    let mut plain = vec![0x49];
    write_bytes(&mut plain, username.as_bytes());
    plain.push(0x50);
    write_int(&mut plain, auth_type);
    plain.push(0x51);
    write_bytes(&mut plain, auth_data);
    while plain.len() % 16 != 0 {
        plain.push(0);
    }
    // Decoding XORs each byte with the one 16 before it, from the end; undo that front to back.
    for j in 16..plain.len() {
        plain[j] ^= plain[j - 16];
    }
    let key = blob_key(device_id, username);
    let cipher = aes::Aes192::new(GenericArray::from_slice(&key));
    for chunk in plain.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    BASE64.encode(plain)
}

fn blob_key(device_id: &str, username: &str) -> [u8; 24] {
    let secret = Sha1::digest(device_id.as_bytes());
    let mut key = [0u8; 24];
    pbkdf2::pbkdf2_hmac::<Sha1>(&secret, username.as_bytes(), 0x100, &mut key[..20]);
    let hash = Sha1::digest(&key[..20]);
    key[..20].copy_from_slice(&hash);
    key[20..].copy_from_slice(&20u32.to_be_bytes());
    key
}

/// The `blob` parameter: iv | AES-128-CTR(plain) | HMAC-SHA1, keys derived from the DH secret.
fn outer_blob(shared: &[u8], iv: &[u8; 16], plain: &[u8]) -> String {
    let base = Sha1::digest(shared);
    let base = &base[..16];
    let derive = |label: &[u8]| {
        let mut h = <Hmac<Sha1> as Mac>::new_from_slice(base).expect("any key length");
        h.update(label);
        h.finalize().into_bytes()
    };
    let checksum_key = derive(b"checksum");
    let encryption_key = derive(b"encryption");
    let mut data = plain.to_vec();
    let mut cipher =
        Aes128Ctr::new_from_slices(&encryption_key[..16], iv).expect("16-byte key and iv");
    cipher.apply_keystream(&mut data);
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(&checksum_key).expect("any key length");
    mac.update(&data);
    let mut out = iv.to_vec();
    out.extend_from_slice(&data);
    out.extend_from_slice(&mac.finalize().into_bytes());
    BASE64.encode(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_core::authentication::Credentials;
    use librespot_protocol::authentication::AuthenticationType;

    #[test]
    fn inner_blob_is_what_librespot_decodes() {
        for len in [0usize, 5, 100, 130, 300] {
            let auth: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let blob = credentials_blob("31qbjurkt2qa", STORED_CREDENTIALS, &auth, "dev-123");
            let creds = Credentials::with_blob("31qbjurkt2qa", blob, "dev-123").unwrap();
            assert_eq!(
                creds.auth_type,
                AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS
            );
            assert_eq!(creds.auth_data, auth);
            assert_eq!(creds.username.as_deref(), Some("31qbjurkt2qa"));
        }
    }

    #[test]
    fn outer_blob_matches_the_discovery_server() {
        // Mirrors librespot-discovery's handle_add_user.
        let device = DhLocalKeys::random(&mut rand::rng());
        let client = DhLocalKeys::random(&mut rand::rng());
        let iv = [9u8; 16];
        let blob = outer_blob(
            &client.shared_secret(&device.public_key()),
            &iv,
            b"hello blob",
        );
        let raw = BASE64.decode(blob).unwrap();
        let shared = device.shared_secret(&client.public_key());
        let base = Sha1::digest(shared);
        let base = &base[..16];
        let derive = |label: &[u8]| {
            let mut h = <Hmac<Sha1> as Mac>::new_from_slice(base).unwrap();
            h.update(label);
            h.finalize().into_bytes()
        };
        let (iv, rest) = raw.split_at(16);
        let (enc, mac) = rest.split_at(rest.len() - 20);
        let mut h = <Hmac<Sha1> as Mac>::new_from_slice(&derive(b"checksum")).unwrap();
        h.update(enc);
        h.verify_slice(mac).unwrap();
        let mut data = enc.to_vec();
        Aes128Ctr::new_from_slices(&derive(b"encryption")[..16], iv)
            .unwrap()
            .apply_keystream(&mut data);
        assert_eq!(data, b"hello blob");
    }

    #[test]
    fn urls_and_form_encoding() {
        assert_eq!(
            endpoint_url("127.0.0.1", 4070, "/zc"),
            "http://127.0.0.1:4070/zc"
        );
        assert_eq!(endpoint_url("[::1]", 1, "zc"), "http://[::1]:1/zc");
        assert_eq!(form_encode("a+b/c= d"), "a%2Bb%2Fc%3D%20d");
    }
}
